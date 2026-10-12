//! The "Monitor" popup (`m` key): pins the live scan loop to one
//! transponder and renders live numeric readouts plus scrolling graphs for
//! its stats - see MonitorHistory, GraphVisibility, and `render`.

use std::collections::VecDeque;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Clear, Paragraph, Sparkline};

use crate::frontend;
use crate::ioctl::{self, Stat};
use crate::transponder;
use crate::tui::{centered_rect_fixed, sat_letter, App, ACCENT, BAD, DIM, GOOD, WARN};

/// A single tick's worth of raw readings to feed into `MonitorHistory::sample`,
/// one field per `DTV_STAT_*` this driver exposes (`ioctl::FeStats`), plus
/// the two derived convenience values (`ber`, `corrected`) that
/// `frontend::Stats` already computes from pairs of them.
pub struct Readings {
    pub signal: Stat,
    pub cnr: Stat,
    pub pre_error_bits: Stat,
    pub pre_total_bits: Stat,
    pub post_error_bits: Stat,
    pub post_total_bits: Stat,
    pub error_blocks: Stat,
    pub total_blocks: Stat,
    pub corrected_bits: Stat,
    pub ber: Option<f64>,
}

/// Which of the Monitor popup's graphs are currently shown - toggled with
/// keys 1-9 and 0 while it's open (see `GraphVisibility::toggle`). CNR/
/// Signal are worth watching by default; the other eight start hidden -
/// most of them are only interesting when actively debugging a specific
/// problem (a stuck FEC, a flapping lock), not during routine viewing.
pub struct GraphVisibility {
    signal: bool,
    cnr: bool,
    pre_error_bits: bool,
    pre_total_bits: bool,
    post_error_bits: bool,
    post_total_bits: bool,
    error_blocks: bool,
    total_blocks: bool,
    corrected_bits: bool,
    ber: bool,
}

impl Default for GraphVisibility {
    fn default() -> Self {
        GraphVisibility {
            signal: true,
            cnr: true,
            pre_error_bits: false,
            pre_total_bits: false,
            post_error_bits: false,
            post_total_bits: false,
            error_blocks: false,
            total_blocks: false,
            corrected_bits: false,
            ber: false,
        }
    }
}

impl GraphVisibility {
    /// Toggles one graph on/off - keys 1-9 and 0 (for the 10th), only
    /// meaningful while the Monitor popup is open.
    pub fn toggle(&mut self, n: u8) {
        match n {
            1 => self.cnr = !self.cnr,
            2 => self.signal = !self.signal,
            3 => self.pre_error_bits = !self.pre_error_bits,
            4 => self.pre_total_bits = !self.pre_total_bits,
            5 => self.post_error_bits = !self.post_error_bits,
            6 => self.post_total_bits = !self.post_total_bits,
            7 => self.error_blocks = !self.error_blocks,
            8 => self.total_blocks = !self.total_blocks,
            9 => self.corrected_bits = !self.corrected_bits,
            0 => self.ber = !self.ber,
            _ => {}
        }
    }
}

/// How many samples each Monitor graph keeps - old samples scroll off the
/// left as new ones arrive. At the ~50ms idle tick cadence (POLL_INTERVAL),
/// 1200 samples is about a minute of history - comfortably more than even
/// a very wide terminal's graph width, so there's always enough buffered
/// to scroll smoothly (see the tail-slicing in `render`).
const MONITOR_HISTORY_LEN: usize = 1200;

fn push_capped(buf: &mut VecDeque<Option<u64>>, v: Option<u64>) {
    buf.push_back(v);
    if buf.len() > MONITOR_HISTORY_LEN {
        buf.pop_front();
    }
}

/// Reduces a Stat reading to a plain magnitude a Sparkline can plot.
/// `Relative` is already a non-negative 0..0xffff range, but `Decibel` is
/// signed: CNR readings are always positive, while signal-strength readings
/// are commonly reported as negative dBm-like values, so clamping at 0 (as
/// opposed to taking the magnitude) would flatten every sample to zero and
/// hide all variation. The sparkline only needs relative variation to show
/// a trend, not an absolute calibrated meaning, so the sign is discarded.
/// `Counter`/`Unavailable` aren't meaningful to graph this way directly -
/// counters go through `DeltaSeries` instead.
fn stat_magnitude(s: Stat) -> Option<u64> {
    match s {
        Stat::Decibel(v) => Some(v.unsigned_abs()),
        Stat::Relative(v) => Some(v as u64),
        Stat::Counter(_) | Stat::Unavailable => None,
    }
}

/// BER spans many orders of magnitude (1 down to 1e-11 or smaller), so it's
/// graphed as its order of magnitude ("number of nines") rather than the
/// raw ratio - bigger bar means fewer errors, same sense as the other
/// graphs. `None` (not enough bits counted yet to compute a ratio) stays
/// `None`; a `Some(0.0)` (bits counted, zero errors so far - a real, good
/// reading) is clamped to the ceiling rather than treated as "no data".
const MAX_BER_EXPONENT: f64 = 15.0;
fn ber_exponent(ber: Option<f64>) -> Option<u64> {
    let b = ber?;
    if b <= 0.0 {
        return Some(MAX_BER_EXPONENT as u64);
    }
    Some((-b.log10()).clamp(0.0, MAX_BER_EXPONENT) as u64)
}

/// A graphed series that samples a live magnitude directly each tick (CNR,
/// Signal, BER) - as opposed to `DeltaSeries`, which derives one from a
/// cumulative counter.
#[derive(Default)]
struct InstantSeries(VecDeque<Option<u64>>);

impl InstantSeries {
    fn sample(&mut self, value: Option<u64>) {
        push_capped(&mut self.0, value);
    }
}

/// A graphed series backed by a cumulative-since-tune `Stat::Counter` (bit
/// counts, block counts) - turned into a per-tick delta so a burst of
/// errors shows up as a spike instead of an ever-climbing ramp. `last`
/// tracks the previous reading across calls.
#[derive(Default)]
struct DeltaSeries {
    history: VecDeque<Option<u64>>,
    last: Option<u64>,
}

impl DeltaSeries {
    /// `locked` is the frontend's instantaneous lock state for this exact
    /// tick: while unlocked, `value` is a last-known-good reading frozen
    /// by refresh_current, not a real current one, so this records `None`
    /// ("no data") rather than computing a (likely zero, misleadingly
    /// "no errors") delta against it.
    fn sample(&mut self, locked: bool, value: Stat) {
        let delta = match (locked, value) {
            (true, Stat::Counter(c)) => {
                let d = self.last.map(|l| c.saturating_sub(l)).unwrap_or(0);
                self.last = Some(c);
                Some(d)
            }
            _ => None,
        };
        push_capped(&mut self.history, delta);
    }
}

/// Rolling history of a single pinned row's live stats, sampled once per
/// tick while the Monitor popup is open on it - see
/// `crate::tui::App::pin_monitor`. Each series takes exactly one entry per
/// tick (`None` when there's nothing real to report) so they all stay
/// aligned in time and the graph keeps scrolling at a steady rate instead
/// of stalling whenever there's a gap.
#[derive(Default)]
pub struct MonitorHistory {
    pub row: usize,
    signal: InstantSeries,
    cnr: InstantSeries,
    ber: InstantSeries,
    pre_error_bits: DeltaSeries,
    pre_total_bits: DeltaSeries,
    post_error_bits: DeltaSeries,
    post_total_bits: DeltaSeries,
    error_blocks: DeltaSeries,
    total_blocks: DeltaSeries,
    corrected_bits: DeltaSeries,
}

impl MonitorHistory {
    pub fn new(row: usize) -> Self {
        MonitorHistory { row, ..Default::default() }
    }

    pub fn sample(&mut self, locked: bool, r: Readings) {
        self.signal.sample(if locked { stat_magnitude(r.signal) } else { None });
        self.cnr.sample(if locked { stat_magnitude(r.cnr) } else { None });
        self.ber.sample(if locked { ber_exponent(r.ber) } else { None });
        self.pre_error_bits.sample(locked, r.pre_error_bits);
        self.pre_total_bits.sample(locked, r.pre_total_bits);
        self.post_error_bits.sample(locked, r.post_error_bits);
        self.post_total_bits.sample(locked, r.post_total_bits);
        self.error_blocks.sample(locked, r.error_blocks);
        self.total_blocks.sample(locked, r.total_blocks);
        self.corrected_bits.sample(locked, r.corrected_bits);
    }
}

/// Every fe_status *acquisition-stage* flag name, always shown (not just
/// the set ones) so the stage that's actually failing is visible at a
/// glance - green for set, red for clear - rather than needing to
/// mentally diff against a clean-lock status to spot what's missing.
/// TIMEDOUT/REINIT are left out here: they're alarm bits that are clear
/// almost all the time, so showing them permanently would just be two
/// boring tokens rather than useful signal - `ioctl::format_status` still
/// includes them for `--trace`, where a rare occurrence is worth digging
/// into after the fact.
fn status_flag_spans(status: u32) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw("status: ")];
    for &(bit, name, good_when_set) in ioctl::STATUS_FLAGS.iter().filter(|(_, _, good_when_set)| *good_when_set) {
        let good = (status & bit != 0) == good_when_set;
        let style = if good { Style::new().fg(GOOD) } else { Style::new().fg(BAD) };
        spans.push(Span::styled(name, style));
        spans.push(Span::raw(" "));
    }
    spans
}

/// What unit a Monitor graph's magnitude is in, for `fmt_magnitude`/
/// `round_scale` below - covers both the raw Stat-backed graphs (CNR,
/// Signal, the counter-delta ones) and BER, which isn't a Stat at all
/// (it's a derived ratio, graphed as its order of magnitude).
#[derive(Clone, Copy)]
enum GraphUnit {
    Decibel,
    Relative,
    Count,
    BerExponent,
}

fn graph_unit_of(s: Stat) -> GraphUnit {
    match s {
        Stat::Decibel(_) => GraphUnit::Decibel,
        Stat::Relative(_) => GraphUnit::Relative,
        Stat::Counter(_) | Stat::Unavailable => GraphUnit::Count,
    }
}

/// Renders a raw graph magnitude back into its display unit - e.g. "15.03
/// dB" for CNR/Signal, "1e-7" for BER, or a plain count for the
/// counter-delta graphs.
fn fmt_magnitude(unit: GraphUnit, v: u64) -> String {
    match unit {
        GraphUnit::Decibel => format!("{:.2} dB", v as f64 / 1000.0),
        GraphUnit::Relative => format!("{:.0}%", v as f64 / 65535.0 * 100.0),
        GraphUnit::Count => v.to_string(),
        GraphUnit::BerExponent => format!("1e-{v}"),
    }
}

fn fmt_magnitude_opt(unit: GraphUnit, v: Option<u64>) -> String {
    v.map_or("-".to_string(), |v| fmt_magnitude(unit, v))
}

/// Rounds a raw magnitude up to the next multiple of 5 *in its displayed
/// unit* (5 dB, 5%, 5 blocks, or 5 orders of magnitude), then converts back
/// to raw units - so a scale of "0-27.3 dB" (whatever the data happens to
/// peak at) always becomes a clean "0-30 dB" instead, and the bars drawn
/// via this same value as Sparkline's `.max()` always match that rounder
/// label.
fn round_scale(unit: GraphUnit, raw: u64) -> u64 {
    let round_up_5 = |v: f64| (v / 5.0).ceil() * 5.0;
    match unit {
        GraphUnit::Decibel => (round_up_5(raw as f64 / 1000.0) * 1000.0) as u64,
        GraphUnit::Relative => (round_up_5(raw as f64 / 65535.0 * 100.0) / 100.0 * 65535.0) as u64,
        GraphUnit::Count | GraphUnit::BerExponent => round_up_5(raw as f64) as u64,
    }
}

/// Live-updating detail view for the row pinned by Popup::Monitor: numeric
/// readouts plus a scrolling graph for each stat currently toggled on - see
/// MonitorHistory/GraphVisibility. Unlike the NIT/SDT popups this has no
/// scrolling of its own (there's nothing below the fold to page through),
/// so it ignores `app.popup_scroll`.
pub fn render(f: &mut Frame, app: &App) {
    let idx = app.cursor.selected().unwrap_or(0).min(app.rows.len().saturating_sub(1));
    let entry = &app.rows[idx];

    let title = format!(" Monitor - {} MHz ", entry.tp.display_frequency);
    let area = centered_rect_fixed(f.area().width * 9 / 10, (f.area().height * 9 / 10).max(16), f.area());
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(title, Style::new().fg(ACCENT).bold()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let pos = if entry.tp.system.is_satellite() {
        sat_letter(entry.tp.position).to_string()
    } else {
        "-".to_string()
    };
    // No separate lock=yes/no or signal=/CNR=/blocks= readout here - the
    // LOCK bit is already in the status flag row below, and signal/CNR/
    // blocks already have their own "now ..." label on each graph's title
    // when it's shown. BER has no graph on by default, so it keeps a text
    // fallback here regardless of whether its own graph is toggled on.
    let details = vec![
        Line::from(format!(
            "{} {} MHz  pol={}  mod={}  SR={}  pos={}",
            entry.tp.system,
            entry.tp.display_frequency,
            if entry.tp.pol_h { "H" } else { "V" },
            transponder::modulation_name(entry.tp.modulation),
            entry.tp.symbol_rate / 1000,
            pos,
        )),
        Line::from(format!("BER={}", frontend::fmt_ber(entry.ber))),
        Line::from(status_flag_spans(entry.status)),
    ];

    // Sparkline renders from the *start* of the slice it's given, not the
    // tail - it never scrolls on its own. Keeping the full MONITOR_HISTORY_LEN
    // history around (for a possible future scrollback) and handing it all
    // over would show the same oldest samples, frozen, until the deque's own
    // cap finally started popping them off the front. Taking just the tail
    // that fits the graph's rendered width here makes it scroll every tick,
    // as soon as there's more history than the graph is wide. All graphs
    // share the same width (only the vertical split varies below), so this
    // doesn't need to wait on knowing how many graphs end up visible.
    let graph_width = inner.width.saturating_sub(2) as usize; // minus the Sparkline's own border
    let tail = |buf: &VecDeque<Option<u64>>| -> Vec<Option<u64>> { buf.iter().rev().take(graph_width).rev().copied().collect() };
    let hist = app.monitor.as_ref().filter(|m| m.row == idx);
    let last_text = |data: &[Option<u64>]| data.last().copied().flatten().map_or("-".to_string(), |v| v.to_string());

    struct Graph {
        /// The digit that toggles this graph (see GraphVisibility::toggle),
        /// shown in the title so the mapping doesn't have to be memorized,
        /// especially for whichever graphs are currently hidden.
        key: u8,
        label: &'static str,
        current: String,
        unit: GraphUnit,
        color: Color,
        floor: u64,
        data: Vec<Option<u64>>,
    }

    // 25 dB comfortably covers every DVB-S2 MODCOD's required Es/N0 (the
    // most demanding, 32APSK 9/10, needs ~16 dB) with headroom to spare -
    // without a floor, a steady signal looks like it's jumping around
    // just because the window auto-scales tight around small variation.
    // None of the others need one: counts/BER naturally start at/near 0.
    const CNR_GRAPH_FLOOR: u64 = 25_000; // milli-dB, matches Stat::Decibel's own units

    let mut graphs: Vec<Graph> = Vec::new();
    if app.graph_visible.cnr {
        let data = hist.map(|m| tail(&m.cnr.0)).unwrap_or_default();
        graphs.push(Graph { key: 1, label: "CNR", current: frontend::fmt_stat(entry.cnr), unit: graph_unit_of(entry.cnr), color: GOOD, floor: CNR_GRAPH_FLOOR, data });
    }
    if app.graph_visible.signal {
        let data = hist.map(|m| tail(&m.signal.0)).unwrap_or_default();
        graphs.push(Graph { key: 2, label: "Signal", current: frontend::fmt_stat(entry.signal), unit: graph_unit_of(entry.signal), color: ACCENT, floor: 0, data });
    }
    if app.graph_visible.pre_error_bits {
        let data = hist.map(|m| tail(&m.pre_error_bits.history)).unwrap_or_default();
        graphs.push(Graph { key: 3, label: "Pre-FEC error bits/tick", current: last_text(&data), unit: GraphUnit::Count, color: WARN, floor: 0, data });
    }
    if app.graph_visible.pre_total_bits {
        let data = hist.map(|m| tail(&m.pre_total_bits.history)).unwrap_or_default();
        graphs.push(Graph { key: 4, label: "Pre-FEC total bits/tick", current: last_text(&data), unit: GraphUnit::Count, color: DIM, floor: 0, data });
    }
    if app.graph_visible.post_error_bits {
        let data = hist.map(|m| tail(&m.post_error_bits.history)).unwrap_or_default();
        graphs.push(Graph { key: 5, label: "Post-FEC error bits/tick", current: last_text(&data), unit: GraphUnit::Count, color: WARN, floor: 0, data });
    }
    if app.graph_visible.post_total_bits {
        let data = hist.map(|m| tail(&m.post_total_bits.history)).unwrap_or_default();
        graphs.push(Graph { key: 6, label: "Post-FEC total bits/tick", current: last_text(&data), unit: GraphUnit::Count, color: DIM, floor: 0, data });
    }
    if app.graph_visible.error_blocks {
        let data = hist.map(|m| tail(&m.error_blocks.history)).unwrap_or_default();
        graphs.push(Graph { key: 7, label: "Uncorrected blocks/tick", current: last_text(&data), unit: GraphUnit::Count, color: BAD, floor: 0, data });
    }
    if app.graph_visible.total_blocks {
        let data = hist.map(|m| tail(&m.total_blocks.history)).unwrap_or_default();
        graphs.push(Graph { key: 8, label: "Total blocks/tick", current: last_text(&data), unit: GraphUnit::Count, color: DIM, floor: 0, data });
    }
    if app.graph_visible.corrected_bits {
        let data = hist.map(|m| tail(&m.corrected_bits.history)).unwrap_or_default();
        graphs.push(Graph { key: 9, label: "Corrected bits/tick", current: last_text(&data), unit: GraphUnit::Count, color: GOOD, floor: 0, data });
    }
    if app.graph_visible.ber {
        let data = hist.map(|m| tail(&m.ber.0)).unwrap_or_default();
        graphs.push(Graph { key: 0, label: "BER", current: frontend::fmt_ber(entry.ber), unit: GraphUnit::BerExponent, color: WARN, floor: 0, data });
    }

    let mut constraints = vec![Constraint::Length(3)];
    constraints.extend(std::iter::repeat_n(Constraint::Min(4), graphs.len()));
    let chunks = Layout::vertical(constraints).split(inner);
    f.render_widget(Paragraph::new(details), chunks[0]);

    // The right-aligned title has no style of its own, so without an
    // explicit color here it'd inherit the border's DIM color underneath
    // it instead of staying readable - give it the graph's own color
    // (matching its bars) rather than leaving it gray. `min`/`max` are the
    // actual range seen in the currently visible window - distinct from
    // `scale`, which is that same max rounded up to a clean ceiling (see
    // `round_scale`) and is what the bars are actually drawn against.
    let graph_block = |key: u8, label: &str, current: &str, min: Option<u64>, data_max: Option<u64>, scale: u64, unit: GraphUnit, color: Color| {
        Block::bordered().border_style(Style::new().fg(DIM)).title(Line::from(format!(" ({key}) {label} ")).left_aligned()).title(
            Line::styled(
                format!(
                    " now {current}  min {}  max {}  scale 0-{} ",
                    fmt_magnitude_opt(unit, min),
                    fmt_magnitude_opt(unit, data_max),
                    fmt_magnitude(unit, scale)
                ),
                Style::new().fg(color),
            )
            .right_aligned(),
        )
    };

    // A gap (no lock, so no real reading) renders as a solid full-height
    // DIM bar instead of the usual partial-height bar in the graph's own
    // color - unmistakably different from real data (including a real
    // error spike, which is BAD/red rather than gray), and still visibly
    // scrolling through at the normal rate rather than leaving the graph
    // looking stalled in place for the gap's duration.
    fn absent(s: Sparkline<'_>) -> Sparkline<'_> {
        s.absent_value_style(Style::new().fg(DIM)).absent_value_symbol(symbols::shade::FULL)
    }

    for (i, g) in graphs.iter().enumerate() {
        let data_min = g.data.iter().filter_map(|v| *v).min();
        let data_max = g.data.iter().filter_map(|v| *v).max();
        let scale = round_scale(g.unit, data_max.unwrap_or(0).max(g.floor));
        f.render_widget(
            absent(
                Sparkline::default()
                    .block(graph_block(g.key, g.label, &g.current, data_min, data_max, scale, g.unit, g.color))
                    .style(Style::new().fg(g.color))
                    .max(scale)
                    .data(&g.data),
            ),
            chunks[i + 1],
        );
    }
}
