//! Shared machinery for DVB SI (Service Information) tables: the minimal
//! demux bindings (linux/dvb/dmx.h) needed to filter for any one PID/
//! table_id, a generic persistent listener thread engine, and a couple of
//! decode helpers shared by every table's own parser (nit.rs, sdt.rs, ...).

use std::fs::OpenOptions;
use std::mem::size_of;
use std::os::unix::io::AsRawFd;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

const fn ioc(dir: u32, nr: u32, size: u32) -> u32 {
    const IOC_NRBITS: u32 = 8;
    const IOC_TYPEBITS: u32 = 8;
    const TYPE_O: u32 = b'o' as u32;
    (dir << 30) | (TYPE_O << IOC_NRBITS) | nr | (size << (IOC_NRBITS + IOC_TYPEBITS))
}
const IOC_WRITE: u32 = 1;

pub(crate) const DMX_CHECK_CRC: u32 = 1;
pub(crate) const DMX_IMMEDIATE_START: u32 = 4;

// Mirrors struct dmx_filter (linux/dvb/dmx.h): all-u8 arrays, no padding.
#[repr(C)]
pub(crate) struct DmxFilter {
    pub filter: [u8; 16],
    pub mask: [u8; 16],
    pub mode: [u8; 16],
}

// Mirrors struct dmx_sct_filter_params exactly (verified against the system
// header: sizeof == 60, with 2 bytes of compiler-inserted padding before
// `timeout` to satisfy its u32 alignment - not packed, matches Rust's
// default repr(C) layout for these field types with no extra work needed).
#[repr(C)]
pub(crate) struct DmxSctFilterParams {
    pub pid: u16,
    pub filter: DmxFilter,
    pub timeout: u32,
    pub flags: u32,
}

pub(crate) const DMX_SET_FILTER: u32 = ioc(IOC_WRITE, 43, size_of::<DmxSctFilterParams>() as u32);

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// Best-effort decode of a DVB SI text field (EN 300 468 Annex A). A
/// leading byte below 0x20 selects an alternate character table; we just
/// skip it and decode the rest as Latin-1, which covers plain ASCII/Latin
/// names correctly and is good enough beyond that.
pub(crate) fn decode_dvb_text(bytes: &[u8]) -> String {
    let payload = match bytes.first() {
        Some(&b) if b < 0x20 => &bytes[1..],
        _ => bytes,
    };
    payload.iter().map(|&b| b as char).collect()
}

/// Command sent from the UI thread to a listener: watch this row from now
/// on, with this already-confirmed version_number (if any) so cheap
/// pre-filtering can skip redundant sections.
pub struct Watch {
    pub row: usize,
    pub known_version: Option<u8>,
}

/// One section's contribution, tagged with whichever row was being watched
/// at the moment it arrived.
pub struct Update<T> {
    pub row: usize,
    pub data: T,
}

/// Runs forever on its own thread, reporting whatever `parse` decodes from
/// each section back tagged with whichever row `cmd_rx` last said to watch.
/// Never blocks the caller - this is meant to be the whole body of a
/// thread, not called inline. Generic over the table: `pid`/`table_id`
/// select what the demux filters for, `label` is just for trace messages,
/// and `parse` turns one raw section's bytes into this table's own data
/// type (returning `None` to skip a section that didn't parse).
///
/// This replaces spawning a new bounded-duration fetch per visit: that
/// required guessing how long to wait (racing the fetch's own timeout
/// against how long we intended to dwell) and tagging results with a "tune
/// epoch" to detect staleness after the fact. Here, the demux fd itself is
/// reopened fresh every time the watch target changes (see
/// `listen_for_one_watch`) - a brand-new fd starts with zero buffered
/// backlog, so stale data from the previous target isn't a possibility to
/// even guard against, structurally rather than by timing.
#[allow(clippy::too_many_arguments)]
pub fn run_listener<T>(
    adapter: u32,
    frontend: u32,
    trace: bool,
    pid: u16,
    table_id: u8,
    label: &'static str,
    cmd_rx: Receiver<Watch>,
    update_tx: Sender<Update<T>>,
    mut parse: impl FnMut(&[u8], bool) -> Option<T>,
) {
    // Nothing useful to do until the UI thread tells us what to watch -
    // block for the first one.
    let Ok(mut watch) = cmd_rx.recv() else { return };
    loop {
        match listen_for_one_watch(adapter, frontend, trace, pid, table_id, label, &watch, &cmd_rx, &update_tx, &mut parse) {
            // The watch target changed - open a fresh fd for the new one.
            Some(next) => watch = next,
            // UI thread is gone, or open/filter setup failed - stop.
            None => return,
        }
    }
}

/// Opens a fresh demux fd for `watch`, then reads from it until a newer
/// `Watch` command arrives (returned, so the caller can reopen for it) or
/// the UI thread disconnects (returns None).
#[allow(clippy::too_many_arguments)]
fn listen_for_one_watch<T>(
    adapter: u32,
    frontend: u32,
    trace: bool,
    pid: u16,
    table_id: u8,
    label: &'static str,
    watch: &Watch,
    cmd_rx: &Receiver<Watch>,
    update_tx: &Sender<Update<T>>,
    parse: &mut impl FnMut(&[u8], bool) -> Option<T>,
) -> Option<Watch> {
    let path = format!("/dev/dvb/adapter{adapter}/demux{frontend}");
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            if trace {
                eprintln!("{label}: opening {path}: {e}");
            }
            return None;
        }
    };
    let fd = file.as_raw_fd();
    if trace {
        eprintln!(
            "{label}: listener opened {path} for row {}, filtering PID 0x{pid:04x} table_id 0x{table_id:02x}",
            watch.row
        );
    }

    let mut params = DmxSctFilterParams {
        pid,
        filter: DmxFilter {
            filter: [0; 16],
            mask: [0; 16],
            mode: [0; 16],
        },
        timeout: 0, // no kernel-side auto-stop - keep delivering indefinitely
        flags: DMX_CHECK_CRC | DMX_IMMEDIATE_START,
    };
    params.filter.filter[0] = table_id;
    params.filter.mask[0] = 0xFF;

    if unsafe { libc::ioctl(fd, DMX_SET_FILTER as _, &params) } < 0 {
        if trace {
            eprintln!("{label}: listener DMX_SET_FILTER failed: {}", std::io::Error::last_os_error());
        }
        return None;
    }

    let mut buf = [0u8; 4096];
    loop {
        // A newer watch target means the UI thread has retuned - stop
        // using this fd (it gets dropped/closed when we return) and let
        // the caller open a fresh one instead of continuing to read
        // whatever this one delivers next.
        match cmd_rx.try_recv() {
            Ok(newer) => return Some(newer),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return None,
        }

        // Short poll so we keep re-checking cmd_rx regularly even when no
        // data is arriving.
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, 500) };
        if rc <= 0 {
            continue;
        }
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n <= 0 {
            continue;
        }
        let data = &buf[..n as usize];
        if trace {
            eprintln!("{label}: received {n} bytes: {}", hex(data));
        }

        if data.len() > 5 && data[0] == table_id && watch.known_version == Some((data[5] >> 1) & 0x1F) {
            if trace {
                eprintln!("{label}: section matches already-known version {}, skipping", (data[5] >> 1) & 0x1F);
            }
            continue;
        }

        let Some(item) = parse(data, trace) else {
            continue;
        };
        let update = Update { row: watch.row, data: item };
        if update_tx.send(update).is_err() {
            return None; // UI thread is gone
        }
    }
}
