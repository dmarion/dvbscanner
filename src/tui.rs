use std::io::Stdout;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState};

use crate::frontend::{self, Frontend};
use crate::ioctl::{self, Stat};
use crate::input::InputConfig;
use crate::monitor::{self, GraphVisibility, MonitorHistory};
use crate::nit;
use crate::sdt;
use crate::transponder::{self, Transponder, TransponderConfig};

pub(crate) struct Entry {
    pub(crate) tp: Transponder,
    tested: bool,
    /// Instantaneous FE_HAS_LOCK as of the last status read - can blip
    /// false for a single tick even on an otherwise perfectly healthy
    /// lock (normal demod jitter). Only meaningful for "are we literally
    /// demodulating right now" decisions (e.g. whether reading back
    /// modulation makes sense); anything about display or whether this
    /// visit "succeeded" should use `ever_locked` instead, which doesn't
    /// flicker.
    pub(crate) locked: bool,
    /// The full fe_status bitmask as of the last read (SIGNAL/CARRIER/
    /// VITERBI/SYNC/LOCK/...), for display via ioctl::format_status - the
    /// single `locked` bool only tracks the LOCK bit, which hides exactly
    /// which stage of acquisition is actually failing when it's not set.
    pub(crate) status: u32,
    /// Whether `locked` has been true at least once during the current
    /// (or just-finished) visit to this row. Unlike `locked`, this can't
    /// be undone by a single-tick blip, so it's what display (lock glyph,
    /// row dimming) and the dwell-duration/NIT-cache-wipe decisions use.
    ever_locked: bool,
    pub(crate) signal: Stat,
    pub(crate) cnr: Stat,
    pub(crate) ber: Option<f64>,
    /// Raw DTV_STAT_* readings beyond signal/cnr - cached the same way
    /// (last-known-good, frozen while unlocked), kept only for the
    /// Monitor popup's per-stat graphs (see monitor::Readings).
    pub(crate) pre_error_bits: Stat,
    pub(crate) pre_total_bits: Stat,
    pub(crate) post_error_bits: Stat,
    pub(crate) post_total_bits: Stat,
    pub(crate) blocks: Stat,
    pub(crate) total_blocks: Stat,
    /// Bit errors FEC has fixed since tune (pre-FEC minus post-FEC error
    /// count) - cumulative, like `blocks`, not a per-tick delta.
    pub(crate) corrected: Stat,
    /// Confirmed/displayed NIT - kept for as long as we stay locked (not
    /// reset on every tick while still locked, so we don't re-fetch it on
    /// every scan lap once known). Cleared only once a visit to this row
    /// ends without ever achieving lock - see `App::advance`.
    nit: Option<nit::NitData>,
    /// A newer `version_number`'s NIT being assembled in the background
    /// while `nit` (an older version) is already complete. Promoted to
    /// `nit` only once it's complete in its own right - see
    /// `Entry::apply_nit`. Keeps a fully-assembled older NIT on screen
    /// instead of replacing it with a still-partial newer one the moment
    /// the broadcaster bumps the version.
    nit_staging: Option<nit::NitData>,
    /// Same idea as `nit`/`nit_staging`, for the SDT (service list).
    sdt: Option<sdt::SdtData>,
    sdt_staging: Option<sdt::SdtData>,
}

impl Entry {
    /// The version_number we currently trust, if any - passed to the
    /// listener so it can cheaply skip sections that just confirm what we
    /// already know, only doing real work for a version we don't have.
    ///
    /// Only set once `nit` is *complete* - a partial NIT (missing
    /// sections) must not be reported here, even though it already has a
    /// version_number. The listener's cheap skip matches on version_number
    /// alone, which is shared by every section of a NIT regardless of
    /// completeness; if this returned Some for a merely-started version,
    /// the listener would permanently discard every remaining section of
    /// it from then on - including the ones we're still missing - leaving
    /// the row stuck at partial completion forever instead of eventually
    /// catching the rest.
    fn known_version(&self) -> Option<u8> {
        self.nit.as_ref().filter(|n| n.is_complete()).map(|n| n.version_number)
    }

    /// Same as `known_version`, for the SDT listener.
    fn known_sdt_version(&self) -> Option<u8> {
        self.sdt.as_ref().filter(|s| s.is_complete()).map(|s| s.version_number)
    }

    /// Folds a freshly-arrived fetch result into this row's NIT state.
    fn apply_nit(&mut self, new: nit::NitData) {
        let Some(current) = &mut self.nit else {
            self.nit = Some(new);
            return;
        };
        let Some(newer) = current.merge(new) else {
            return; // same version - merged straight into `nit`
        };
        // Different version than our confirmed `nit`. `nit` might already
        // be fully complete, in which case it stays on screen untouched
        // until the new version is itself complete - only then do we
        // promote it. If `nit` was never complete to begin with, there's
        // nothing worth protecting, so the newer version takes over as the
        // thing we're now assembling instead.
        if !current.is_complete() {
            self.nit = Some(newer);
            self.nit_staging = None;
            return;
        }
        match &mut self.nit_staging {
            Some(staged) => {
                if let Some(even_newer) = staged.merge(newer) {
                    // A third version showed up before the staged one
                    // finished - restart staging with that instead.
                    self.nit_staging = Some(even_newer);
                }
            }
            None => self.nit_staging = Some(newer),
        }
        if self.nit_staging.as_ref().is_some_and(|s| s.is_complete()) {
            self.nit = self.nit_staging.take();
        }
    }

    /// Same merge-or-stage logic as `apply_nit`, for the SDT.
    fn apply_sdt(&mut self, new: sdt::SdtData) {
        let Some(current) = &mut self.sdt else {
            self.sdt = Some(new);
            return;
        };
        let Some(newer) = current.merge(new) else {
            return;
        };
        if !current.is_complete() {
            self.sdt = Some(newer);
            self.sdt_staging = None;
            return;
        }
        match &mut self.sdt_staging {
            Some(staged) => {
                if let Some(even_newer) = staged.merge(newer) {
                    self.sdt_staging = Some(even_newer);
                }
            }
            None => self.sdt_staging = Some(newer),
        }
        if self.sdt_staging.as_ref().is_some_and(|s| s.is_complete()) {
            self.sdt = self.sdt_staging.take();
        }
    }
}

/// Which detail popup (if any) is currently shown.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Popup {
    Nit,
    Sdt,
    Monitor,
}

pub(crate) struct App {
    fe: Frontend,
    input: InputConfig,
    pub(crate) rows: Vec<Entry>,
    /// Row the permanent scan loop is currently tuned to.
    active: usize,
    /// Row the user is looking at - independent of `active`, moved with
    /// the arrow keys, used for the row highlight and the detail popups.
    pub(crate) cursor: TableState,
    dwell_start: Instant,
    message: Option<String>,
    popup: Option<Popup>,
    /// Vertical scroll offset into whichever popup is open - reset to 0
    /// whenever a popup is (re)opened or the cursor moves to a different
    /// row, so it never shows a leftover scroll position against new
    /// content.
    popup_scroll: u16,
    /// Tells the persistent NIT listener thread which row to attribute
    /// sections to. Sent on every retune (see tune_current) - the
    /// listener reopens its demux fd fresh every time it picks up a new
    /// one, so there's no way for stale data from a previous target to
    /// leak into the new one - see nit::run_listener.
    nit_cmd_tx: mpsc::Sender<nit::Watch>,
    /// Sections the listener has found, as they arrive.
    nit_update_rx: Receiver<nit::NitUpdate>,
    /// Same as the nit_* pair, for the SDT listener.
    sdt_cmd_tx: mpsc::Sender<sdt::Watch>,
    sdt_update_rx: Receiver<sdt::SdtUpdate>,
    /// Live history for the Monitor popup - only present while it's open,
    /// see App::pin_monitor. While it's open, auto-advance is suspended
    /// and `active` is pinned to whatever row it's watching (its `row`
    /// field), so the scan loop holds still on one transponder instead of
    /// moving on mid-observation.
    pub(crate) monitor: Option<MonitorHistory>,
    /// Which Monitor graphs are shown - persists across popup open/close
    /// and across rows, since it's a display preference, not per-row data.
    pub(crate) graph_visible: GraphVisibility,
}

// The kernel frontend driver keeps retrying/adjusting in the background
// after a tune, so a marginal signal can take several seconds to actually
// lock - giving up after only ~2s (as this used to) misses locks that a
// longer wait would catch.
const DWELL: Duration = Duration::from_millis(8000);
// Always used once locked, known NIT or not. We never stop re-attempting a
// NIT fetch (see refresh_current), and with results merged across however
// many independent attempts it takes, a short dwell just means convergence
// happens over more scan laps instead of needing one long wait up front.
// Keeps scanning uniformly fast regardless of NIT state.
const DWELL_LOCKED: Duration = Duration::from_millis(2500);

impl App {
    fn new(
        fe: Frontend,
        adapter: u32,
        frontend_num: u32,
        trace: bool,
        input: InputConfig,
        cfgs: Vec<TransponderConfig>,
    ) -> Result<App> {
        let mut rows = Vec::with_capacity(cfgs.len());
        for cfg in &cfgs {
            let tp = cfg
                .resolve()
                .map_err(|e| anyhow::anyhow!("transponder at {} MHz: {e:#}", cfg.frequency))?;
            rows.push(Entry {
                tp,
                tested: false,
                locked: false,
                status: 0,
                ever_locked: false,
                signal: Stat::Unavailable,
                cnr: Stat::Unavailable,
                ber: None,
                pre_error_bits: Stat::Unavailable,
                pre_total_bits: Stat::Unavailable,
                post_error_bits: Stat::Unavailable,
                post_total_bits: Stat::Unavailable,
                blocks: Stat::Unavailable,
                total_blocks: Stat::Unavailable,
                corrected: Stat::Unavailable,
                nit: None,
                nit_staging: None,
                sdt: None,
                sdt_staging: None,
            });
        }
        let mut cursor = TableState::default();
        if !rows.is_empty() {
            cursor.select(Some(0));
        }

        let (nit_cmd_tx, nit_cmd_rx) = mpsc::channel();
        let (nit_update_tx, nit_update_rx) = mpsc::channel();
        std::thread::spawn(move || {
            nit::run_listener(adapter, frontend_num, trace, nit_cmd_rx, nit_update_tx);
        });

        let (sdt_cmd_tx, sdt_cmd_rx) = mpsc::channel();
        let (sdt_update_tx, sdt_update_rx) = mpsc::channel();
        std::thread::spawn(move || {
            sdt::run_listener(adapter, frontend_num, trace, sdt_cmd_rx, sdt_update_tx);
        });

        Ok(App {
            fe,
            input,
            rows,
            active: 0,
            cursor,
            dwell_start: Instant::now(),
            message: None,
            popup: None,
            popup_scroll: 0,
            nit_cmd_tx,
            nit_update_rx,
            sdt_cmd_tx,
            sdt_update_rx,
            monitor: None,
            graph_visible: GraphVisibility::default(),
        })
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as isize;
        let cur = self.cursor.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, len - 1) as usize;
        self.cursor.select(Some(next));
        self.popup_scroll = 0;
        if self.popup == Some(Popup::Monitor) {
            self.pin_monitor(next);
        }
    }

    /// Opens or closes a detail popup, resetting scroll so a stale offset
    /// from a previous popup/row doesn't carry over.
    fn toggle_popup(&mut self, which: Popup) {
        if self.popup == Some(which) {
            self.popup = None;
            if which == Popup::Monitor {
                self.monitor = None;
            }
        } else {
            self.popup = Some(which);
            if which == Popup::Monitor {
                let idx = self.cursor.selected().unwrap_or(0);
                self.pin_monitor(idx);
            }
        }
        self.popup_scroll = 0;
    }

    /// Points the Monitor popup (and the live scan loop - see `tick`) at
    /// `idx`, retuning onto it first if it wasn't already the active row.
    /// Starts a fresh history each time, since a retune means whatever was
    /// collected before belongs to a different tuning attempt.
    fn pin_monitor(&mut self, idx: usize) {
        if self.active != idx {
            self.active = idx;
            self.tune_current();
        }
        self.monitor = Some(MonitorHistory::new(idx));
    }

    /// Toggles one of the Monitor popup's graphs on/off - keys 1-4, only
    /// meaningful while that popup is open.
    fn toggle_graph(&mut self, n: u8) {
        self.graph_visible.toggle(n);
    }

    fn scroll_popup(&mut self, delta: i32) {
        if self.popup.is_none() {
            return;
        }
        self.popup_scroll = (self.popup_scroll as i32 + delta).max(0) as u16;
    }

    fn tune_current(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let idx = self.active;
        match self.fe.tune(&self.input, &self.rows[idx].tp) {
            Ok(()) => self.message = None,
            Err(e) => self.message = Some(format!("tune failed: {e:#}")),
        }
        self.rows[idx].tested = true;
        self.rows[idx].locked = false;
        self.rows[idx].ever_locked = false;
        self.dwell_start = Instant::now();
        // Switched right here, at the retune itself - not deferred until
        // lock, which can lag an actual retune by seconds (or never arrive
        // this visit at all). The demux keeps delivering whatever the
        // frontend is *currently* tuned to regardless of lock state, so
        // gating the input on lock leaves a window where the listener is
        // reading the new transponder's real broadcast content but still
        // attributing it to the previous row. Data read before genuine
        // lock just fails DMX_CHECK_CRC and is dropped by the kernel, so
        // there's no downside to watching from the moment of retune.
        let known_version = self.rows[idx].known_version();
        let _ = self.nit_cmd_tx.send(nit::Watch { row: idx, known_version });
        let known_sdt_version = self.rows[idx].known_sdt_version();
        let _ = self.sdt_cmd_tx.send(sdt::Watch { row: idx, known_version: known_sdt_version });
    }

    /// Advance to the next transponder, wrapping around to keep scanning
    /// forever. If the row we're leaving never achieved lock during this
    /// whole visit, whatever was cached for it is presumed stale and
    /// cleared here - not on every transient "still acquiring lock" tick
    /// during the visit, which would defeat the point of caching at all.
    fn advance(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let leaving = self.active;
        // Whether to wipe is decided by `ever_locked` (did this visit
        // achieve lock at least once), not the instantaneous `locked`
        // flag read right now - FE_HAS_LOCK can blip off for a single
        // tick even on an otherwise perfectly healthy, fully-completed
        // row, and if that blip happens to land on the exact tick the
        // dwell timer expires, checking the live flag would wipe out good
        // accumulated data (even a fully-assembled NIT) over a one-tick
        // transient.
        if !self.rows[leaving].ever_locked {
            self.rows[leaving].nit = None;
            self.rows[leaving].nit_staging = None;
            self.rows[leaving].sdt = None;
            self.rows[leaving].sdt_staging = None;
            if !self.rows[leaving].tp.system.is_satellite() {
                self.rows[leaving].tp.modulation = ioctl::QAM_AUTO;
            }
        }
        self.active = (self.active + 1) % self.rows.len();
        self.tune_current();
    }

    fn refresh_current(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let idx = self.active;
        if let Ok(status) = self.fe.status() {
            let now_locked = status & frontend::FE_HAS_LOCK != 0;
            if now_locked && !self.rows[idx].ever_locked {
                // First lock of this visit. DWELL_LOCKED is meant to be
                // how long we listen *while locked*, not however much of
                // it happens to be left over from the original tune -
                // satellite transponders here commonly take 2-3s to lock
                // (Unicable retune + normal DVB-S2 acquisition) where
                // terrestrial locks almost immediately, so without this
                // reset, satellite rows would've already burned through
                // most or all of their locked-dwell budget before a NIT
                // fetch ever got a real window to run in. Guarded by the
                // flag (not just the locked-state transition) because
                // FE_HAS_LOCK can blip off and back on within one visit -
                // resetting on every such blip would restart the dwell
                // clock indefinitely and stall the scan on this row.
                self.dwell_start = Instant::now();
                self.rows[idx].ever_locked = true;
            }
            self.rows[idx].locked = now_locked;
            self.rows[idx].status = status;
        }
        // Instantaneous `locked`, confirmed via --trace: once the driver
        // reports FE_HAS_LOCK dropped, DTV_STAT_CNR/SIGNAL_STRENGTH keep
        // coming back with a non-"unavailable" scale/value regardless - a
        // bogus reading, not a real one, since the frontend isn't actually
        // demodulating. So this has to skip the read while unlocked (which
        // freezes the last known-good value) rather than trust whatever
        // the driver hands back during the gap.
        //
        // FE_READ_STATUS and FE_GET_PROPERTY are two separate ioctls, not
        // one atomic read - lock can legitimately drop in the gap between
        // them, especially on a mux that's already flapping. So this
        // re-checks status again right after fetching stats and only
        // keeps the reading if it was *still* locked then too; a drop
        // caught here updates `locked`/`status` immediately rather than
        // waiting for next tick's read to notice it.
        if self.rows[idx].locked
            && let Ok(stats) = self.fe.stats()
            && let Ok(status_after) = self.fe.status()
        {
            self.rows[idx].locked = status_after & frontend::FE_HAS_LOCK != 0;
            self.rows[idx].status = status_after;
            if self.rows[idx].locked {
                self.rows[idx].signal = stats.signal;
                self.rows[idx].cnr = stats.cnr;
                self.rows[idx].ber = stats.ber;
                self.rows[idx].pre_error_bits = stats.pre_error_bits;
                self.rows[idx].pre_total_bits = stats.pre_total_bits;
                self.rows[idx].post_error_bits = stats.post_error_bits;
                self.rows[idx].post_total_bits = stats.post_total_bits;
                self.rows[idx].blocks = stats.error_blocks;
                self.rows[idx].total_blocks = stats.total_blocks;
                self.rows[idx].corrected = stats.corrected_bits;
            }
        }
        if self.rows[idx].locked
            && self.rows[idx].tp.modulation == ioctl::QAM_AUTO
            && let Ok(m) = self.fe.modulation()
            && m != ioctl::QAM_AUTO
        {
            self.rows[idx].tp.modulation = m;
        }
    }

    /// Applies any sections the listener thread has found so far, without
    /// ever blocking - see nit::run_listener. Every update it sends is
    /// already correctly attributed (it reopens its demux fd fresh on
    /// every watch-target input), so nothing here needs to second-guess
    /// staleness.
    fn poll_nit(&mut self) {
        loop {
            match self.nit_update_rx.try_recv() {
                Ok(update) => {
                    if let Some(row) = self.rows.get_mut(update.row) {
                        row.apply_nit(update.data);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    /// Same as `poll_nit`, for the SDT listener.
    fn poll_sdt(&mut self) {
        loop {
            match self.sdt_update_rx.try_recv() {
                Ok(update) => {
                    if let Some(row) = self.rows.get_mut(update.row) {
                        row.apply_sdt(update.data);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn tick(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.poll_nit();
        self.poll_sdt();
        self.refresh_current();
        // Always sample, every tick, regardless of lock state - MonitorHistory
        // itself turns an unlocked tick into an explicit "no data" entry
        // (confirmed via --trace that the driver keeps reporting non-
        // "unavailable" CNR/signal readings even while actually unlocked, so
        // those can't just be plotted as if they were real measurements).
        // Skipping the sample call entirely here instead would stall the
        // whole graph in place for the duration of the gap rather than
        // showing it scroll through as a visibly distinct absent region.
        if self.monitor.as_ref().is_some_and(|m| m.row == self.active) {
            let entry = &self.rows[self.active];
            let locked = entry.locked;
            let readings = monitor::Readings {
                signal: entry.signal,
                cnr: entry.cnr,
                pre_error_bits: entry.pre_error_bits,
                pre_total_bits: entry.pre_total_bits,
                post_error_bits: entry.post_error_bits,
                post_total_bits: entry.post_total_bits,
                error_blocks: entry.blocks,
                total_blocks: entry.total_blocks,
                corrected_bits: entry.corrected,
                ber: entry.ber,
            };
            self.monitor.as_mut().unwrap().sample(locked, readings);
        }
        // While the Monitor popup is open, it pins the scan loop to one
        // row (see pin_monitor) - auto-advancing out from under it would
        // cut an observation short and silently retune away mid-view.
        if self.popup != Some(Popup::Monitor) {
            // `ever_locked`, not the instantaneous `locked` - a single-tick
            // blip right at the deadline check shouldn't fall back to the
            // long unlocked dwell on an otherwise healthy row.
            let locked = self.rows[self.active].ever_locked;
            let dwell = if locked { DWELL_LOCKED } else { DWELL };
            if self.dwell_start.elapsed() >= dwell {
                self.advance();
            }
        }
    }
}

pub fn run(
    fe: Frontend,
    adapter: u32,
    frontend_num: u32,
    trace: bool,
    input: InputConfig,
    cfgs: Vec<TransponderConfig>,
) -> Result<()> {
    let mut app = App::new(fe, adapter, frontend_num, trace, input, cfgs)?;
    app.tune_current();

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_app(&mut terminal, &mut app);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();

    res
}

/// How often `tick()` re-checks frontend status/stats. Deliberately much
/// shorter than the old 250ms: a lock blip can come and go well within
/// that window, and at 250ms we'd only ever see whichever state happened
/// to be live at the one instant we sampled it - not whether it's actually
/// flapping in between.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;
        if event::poll(POLL_INTERVAL)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Esc => {
                        if let Some(which) = app.popup {
                            app.toggle_popup(which);
                        } else {
                            return Ok(());
                        }
                    }
                    KeyCode::Char('n') => app.toggle_popup(Popup::Nit),
                    KeyCode::Char('s') => app.toggle_popup(Popup::Sdt),
                    KeyCode::Char('m') => app.toggle_popup(Popup::Monitor),
                    KeyCode::Up => app.move_cursor(-1),
                    KeyCode::Down => app.move_cursor(1),
                    KeyCode::PageUp => app.scroll_popup(-PAGE_STEP),
                    KeyCode::PageDown => app.scroll_popup(PAGE_STEP),
                    KeyCode::Char(c @ '0'..='9') if app.popup == Some(Popup::Monitor) => {
                        app.toggle_graph(c as u8 - b'0');
                    }
                    _ => {}
                }
            }
        } else {
            app.tick();
        }
    }
}

/// Lines scrolled per PageUp/PageDown press in a detail popup.
const PAGE_STEP: i32 = 10;

pub(crate) const ACCENT: Color = Color::Cyan;
pub(crate) const GOOD: Color = Color::Green;
pub(crate) const WARN: Color = Color::Yellow;
pub(crate) const BAD: Color = Color::Red;
pub(crate) const DIM: Color = Color::DarkGray;

/// Rough (good, warn) CNR thresholds in dB for a given DTV_MODULATION.
/// `warn` sits at the modulation's QEF (quasi-error-free) CNR floor for a
/// representative, commonly-used FEC rate - 3/4 for the DVB-S/S2 entries
/// (ETSI EN 302307 Annex A/C reference tables), 2/3 for the DVB-T2 QAM
/// entries (the typical broadcast default). `good` is a flat 1.5 dB above
/// that: enough margin to call it comfortable without demanding so much
/// headroom that a solid, error-free lock still shows as merely "ok".
///
/// This can't be exact for every FEC rate actually in use (we only key on
/// modulation here), but 3/4 and 2/3 are representative enough to be a
/// sane single default per modulation rather than guessing one global
/// cutoff for every modulation alike - e.g. QPSK locks cleanly under 5 dB
/// where 256-QAM needs nearly 20 dB.
///
/// The QAM_* entries are DVB-T2 figures specifically (our tool never
/// produces a QAM_* modulation for anything but T/T2 - DVB-C isn't
/// supported). That matters: DVB-C uses plain Reed-Solomon FEC and needs
/// much higher CNR for a given constellation (e.g. ~30 dB for 256-QAM),
/// while T2 uses the same LDPC+BCH coding family as DVB-S2 and locks
/// cleanly around 20 dB for 256-QAM. Using DVB-C-style thresholds here
/// would paint a perfectly healthy, error-free T2 lock as red. QAM_128
/// isn't an actual DVB-T2 constellation - it's kept only as an
/// interpolated placeholder in case a driver ever reports it.
fn cnr_thresholds(modulation: u32) -> Option<(f64, f64)> {
    use crate::ioctl::*;
    Some(match modulation {
        QPSK => (5.5, 4.0),
        PSK_8 => (9.0, 7.5),
        APSK_16 => (11.5, 10.0),
        APSK_32 => (14.0, 12.5),
        QAM_16 => (12.0, 10.5),
        QAM_64 => (17.5, 16.0),
        QAM_128 => (19.5, 18.0),
        QAM_256 => (21.0, 19.5),
        _ => return None,
    })
}

/// Quality color for CNR, aware of the modulation in use. Not used for
/// signal strength: that's a raw dBm-ish power level with no universal
/// "good" threshold (it depends on driver/tuner calibration), unlike CNR
/// where higher is always better once you know the modulation.
fn cnr_style(modulation: u32, s: Stat) -> Style {
    match s {
        Stat::Decibel(v) => {
            let db = v as f64 / 1000.0;
            match cnr_thresholds(modulation) {
                Some((good, warn)) => {
                    let c = if db >= good {
                        GOOD
                    } else if db >= warn {
                        WARN
                    } else {
                        BAD
                    };
                    Style::new().fg(c)
                }
                // Modulation is AUTO or unrecognized (e.g. T/T2, where we
                // don't know what the demod actually settled on) - don't
                // pretend to judge quality against the wrong scale.
                None => Style::new(),
            }
        }
        Stat::Relative(v) => {
            let pct = v as f64 / 65535.0 * 100.0;
            let c = if pct >= 60.0 {
                GOOD
            } else if pct >= 30.0 {
                WARN
            } else {
                BAD
            };
            Style::new().fg(c)
        }
        _ => Style::new().fg(DIM),
    }
}

fn ber_style(b: Option<f64>) -> Style {
    match b {
        None => Style::new().fg(DIM),
        Some(v) if v <= 0.0 => Style::new().fg(GOOD),
        Some(v) if v < 1e-4 => Style::new().fg(WARN),
        _ => Style::new().fg(BAD),
    }
}

/// Displays a satellite/input-leg position (0-based) as a letter, A, B,
/// C, ... matching how multi-satellite switches are commonly labeled.
pub(crate) fn sat_letter(position: u8) -> char {
    (b'A' + position.min(25)) as char
}

fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(area);

    let header = Row::new(vec![
        "Sys", "Pos", "Freq", "Pol", "Mod", "SR", "Lock", "Signal", "CNR", "BER", "Blk", "Network",
    ])
    .style(Style::new().fg(ACCENT).bold());

    let rows: Vec<Row> = app
        .rows
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let pol = if e.tp.system.is_satellite() {
                if e.tp.pol_h { "H" } else { "V" }
            } else {
                "-"
            };
            let sr = if e.tp.system.is_satellite() {
                format!("{}", e.tp.symbol_rate / 1000)
            } else {
                "-".to_string()
            };
            // `ever_locked`, not the instantaneous `locked` - once this
            // visit has locked at all, a later single-tick blip (normal
            // demod jitter) shouldn't flip a finished row's result from
            // success to failure.
            let (lock, lock_style) = if !e.tested {
                ("-", Style::new().fg(DIM))
            } else if e.ever_locked {
                ("✔", Style::new().fg(GOOD).bold())
            } else if i == app.active {
                // Still actively dwelling on this one, hasn't locked (yet) -
                // distinct from a finished attempt that never locked at all.
                ("…", Style::new().fg(WARN))
            } else {
                ("✘", Style::new().fg(BAD))
            };
            let row_style = if e.tested && !e.ever_locked && i != app.active {
                Style::new().fg(DIM)
            } else {
                Style::new()
            };
            let scanning = i == app.active && !e.locked;
            let cnr_sty = if scanning {
                // Still settling, not locked yet - this reading isn't a
                // meaningful quality signal, so don't paint it red/green.
                Style::new()
            } else {
                cnr_style(e.tp.modulation, e.cnr)
            };
            let marker = if i == app.active { "*" } else { " " };
            let pos = if e.tp.system.is_satellite() {
                sat_letter(e.tp.position).to_string()
            } else {
                "-".to_string()
            };
            let network = match &e.nit {
                Some(n) if n.network_name.is_some() => n.network_name.clone().unwrap(),
                Some(n) if n.is_complete() => "(no name)".to_string(),
                Some(n) => n.section_map(),
                None => "-".to_string(),
            };
            Row::new(vec![
                Cell::from(format!("{marker}{}", e.tp.system)),
                Cell::from(pos),
                Cell::from(format!("{}", e.tp.display_frequency)),
                Cell::from(pol),
                Cell::from(transponder::modulation_name(e.tp.modulation)),
                Cell::from(sr),
                Cell::from(lock).style(lock_style),
                Cell::from(frontend::fmt_stat(e.signal)),
                Cell::from(frontend::fmt_stat(e.cnr)).style(cnr_sty),
                Cell::from(frontend::fmt_ber(e.ber)).style(ber_style(e.ber)),
                Cell::from(frontend::fmt_blocks(e.blocks)),
                Cell::from(network),
            ])
            .style(row_style)
        })
        .collect();

    let widths = [
        Constraint::Length(4),
        Constraint::Length(4),
        Constraint::Length(8),
        Constraint::Length(4),
        Constraint::Length(7),
        Constraint::Length(7),
        Constraint::Length(6),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(6),
        Constraint::Length(16),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::new().bg(ACCENT).fg(Color::Black).bold());

    f.render_stateful_widget(table, chunks[0], &mut app.cursor);

    let help_style = if app.message.is_some() {
        Style::new().fg(BAD)
    } else {
        Style::new().fg(DIM)
    };
    let help_text = app.message.clone().unwrap_or_else(|| {
        if app.popup == Some(Popup::Monitor) {
            "up/down switch mux  1-9,0 toggle graphs  n/s/m/esc close  q quit".to_string()
        } else if app.popup.is_some() {
            "up/down switch mux  pgup/pgdn scroll  n/s/m/esc close  q quit".to_string()
        } else {
            "up/down cursor  n Network details  s Service details  m Monitor  q quit".to_string()
        }
    });
    let help = Paragraph::new(Line::from(Span::styled(help_text, help_style)));
    f.render_widget(help, chunks[1]);

    match app.popup {
        Some(Popup::Nit) => render_nit_popup(f, app),
        Some(Popup::Sdt) => render_sdt_popup(f, app),
        Some(Popup::Monitor) => monitor::render(f, app),
        None => {}
    }
}

/// A rect of exactly `width` x `height` (clamped to fit within `r`),
/// centered in `r`.
pub(crate) fn centered_rect_fixed(width: u16, height: u16, r: Rect) -> Rect {
    let width = width.min(r.width);
    let height = height.min(r.height);
    let x = r.x + (r.width - width) / 2;
    let y = r.y + (r.height - height) / 2;
    Rect { x, y, width, height }
}

fn hex_preview(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

fn render_nit_popup(f: &mut Frame, app: &App) {
    let idx = app.cursor.selected().unwrap_or(0).min(app.rows.len().saturating_sub(1));
    let entry = &app.rows[idx];
    let mut lines: Vec<Line> = Vec::new();

    match &entry.nit {
        None => {
            lines.push(Line::from("No NIT data cached for this transponder yet."));
            lines.push(Line::from("(only available once this row has locked long enough to catch a NIT section)"));
        }
        Some(n) => {
            lines.push(Line::from(vec![
                Span::styled("Network ID: ", Style::new().fg(ACCENT)),
                Span::raw(format!("0x{:04x}", n.network_id)),
            ]));
            let name = match &n.network_name {
                Some(name) => name.clone(),
                None if n.is_complete() => "(no name)".to_string(),
                None => format!("(incomplete: {})", n.section_map()),
            };
            lines.push(Line::from(vec![
                Span::styled("Name: ", Style::new().fg(ACCENT)),
                Span::raw(name),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("Transport streams ({}):", n.transport_streams.len()),
                Style::new().fg(ACCENT).bold(),
            )));
            for ts in &n.transport_streams {
                lines.push(Line::from(format!(
                    "  TSID=0x{:04x} ONID=0x{:04x}",
                    ts.transport_stream_id, ts.original_network_id
                )));
                if let Some(sat) = &ts.satellite {
                    let rolloff = sat
                        .rolloff
                        .map(|r| format!(" rolloff={r}"))
                        .unwrap_or_default();
                    lines.push(Line::from(format!(
                        "    sat: {:.3} MHz {:.1}{} pol={} {} {}{} SR={} FEC={}",
                        sat.frequency_mhz,
                        sat.orbital_position,
                        if sat.west { 'W' } else { 'E' },
                        sat.polarization,
                        sat.system,
                        sat.modulation,
                        rolloff,
                        sat.symbol_rate_ksps,
                        sat.fec_inner,
                    )));
                }
                for (tag, data) in &ts.other_descriptors {
                    lines.push(Line::from(Span::styled(
                        format!("    tag=0x{tag:02x} len={}: {}", data.len(), hex_preview(data)),
                        Style::new().fg(DIM),
                    )));
                }
            }
        }
    }

    let title = format!(" Network - {} MHz ", entry.tp.display_frequency);
    render_lines_popup(f, app, &title, lines);
}

fn render_sdt_popup(f: &mut Frame, app: &App) {
    let idx = app.cursor.selected().unwrap_or(0).min(app.rows.len().saturating_sub(1));
    let entry = &app.rows[idx];
    let mut lines: Vec<Line> = Vec::new();

    match &entry.sdt {
        None => {
            lines.push(Line::from("No SDT data cached for this transponder yet."));
            lines.push(Line::from("(only available once this row has locked long enough to catch an SDT section)"));
        }
        Some(s) => {
            lines.push(Line::from(vec![
                Span::styled("Transport Stream ID: ", Style::new().fg(ACCENT)),
                Span::raw(format!("0x{:04x}", s.transport_stream_id)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Original Network ID: ", Style::new().fg(ACCENT)),
                Span::raw(format!("0x{:04x}", s.original_network_id)),
            ]));
            if !s.is_complete() {
                lines.push(Line::from(vec![
                    Span::styled("Sections: ", Style::new().fg(ACCENT)),
                    Span::raw(format!("incomplete ({})", s.section_map())),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("Services ({}):", s.services.len()),
                Style::new().fg(ACCENT).bold(),
            )));
            for svc in &s.services {
                let name = svc.service_name.as_deref().unwrap_or("(no name)");
                let provider = svc.provider_name.as_deref().unwrap_or("");
                let ca = if svc.free_ca_mode { " [encrypted]" } else { "" };
                let style = if svc.running_status == 4 {
                    Style::new()
                } else {
                    Style::new().fg(DIM)
                };
                lines.push(Line::from(Span::styled(
                    format!(
                        "  0x{:04x}  {:<13} {:<11} {}{}{}{}",
                        svc.service_id,
                        sdt::service_type_name(svc.service_type),
                        sdt::running_status_name(svc.running_status),
                        name,
                        if provider.is_empty() { "" } else { " - " },
                        provider,
                        ca,
                    ),
                    style,
                )));
            }
        }
    }

    let title = format!(" Service - {} MHz ", entry.tp.display_frequency);
    render_lines_popup(f, app, &title, lines);
}

/// Common popup chrome for render_nit_popup/render_sdt_popup: sizes the
/// popup to fit the content (clamped to the terminal), then applies
/// `app.popup_scroll` (itself clamped to how much of the content doesn't
/// fit) so content taller than the terminal can be paged through with
/// PageUp/PageDown rather than being silently cut off.
fn render_lines_popup(f: &mut Frame, app: &App, title: &str, lines: Vec<Line>) {
    let content_width = lines.iter().map(Line::width).max().unwrap_or(20) as u16;
    let content_height = lines.len() as u16;
    // At least 90% of the terminal width, regardless of how short the
    // content is - a popup sized tight to a couple of short lines looked
    // cramped next to the full-width table behind it.
    let min_width = f.area().width * 9 / 10;
    let area = centered_rect_fixed(
        (content_width + 4).max(title.len() as u16 + 4).max(min_width),
        content_height + 2,
        f.area(),
    );
    let inner_height = area.height.saturating_sub(2);
    let max_scroll = content_height.saturating_sub(inner_height);
    let scroll = app.popup_scroll.min(max_scroll);

    f.render_widget(Clear, area);
    let popup = Paragraph::new(lines).scroll((scroll, 0)).block(
        Block::bordered()
            .border_style(Style::new().fg(ACCENT))
            .title(Span::styled(title.to_string(), Style::new().fg(ACCENT).bold())),
    );
    f.render_widget(popup, area);
}
