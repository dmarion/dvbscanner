//! NIT (Network Information Table, ETSI EN 300 468) section parsing, on
//! top of the generic listener engine in si.rs.

use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

use crate::si::{self, decode_dvb_text, hex, DmxFilter, DmxSctFilterParams, DMX_CHECK_CRC, DMX_IMMEDIATE_START, DMX_SET_FILTER};

const NIT_PID: u16 = 0x10;
const TABLE_ID_NIT_ACTUAL: u8 = 0x40;
const DESCRIPTOR_TAG_NETWORK_NAME: u8 = 0x40;
const DESCRIPTOR_TAG_SATELLITE_DELIVERY: u8 = 0x43;

/// Decoded satellite_delivery_system_descriptor (EN 300 468 table 28).
#[derive(Debug, Clone)]
pub struct SatelliteDelivery {
    pub frequency_mhz: f64,
    pub orbital_position: f64,
    pub west: bool,
    pub polarization: char, // H, V, L, R
    pub rolloff: Option<f64>,
    pub system: &'static str, // DVB-S / DVB-S2
    pub modulation: &'static str,
    pub symbol_rate_ksps: u32,
    pub fec_inner: &'static str,
}

#[derive(Debug, Clone)]
pub struct TransportStreamEntry {
    pub transport_stream_id: u16,
    pub original_network_id: u16,
    pub satellite: Option<SatelliteDelivery>,
    /// Anything else in this TS's descriptor loop we didn't specifically
    /// decode, as (tag, raw payload) - shown as a hex dump in the NIT popup.
    pub other_descriptors: Vec<(u8, Vec<u8>)>,
}

#[derive(Debug, Clone)]
pub struct NitData {
    pub network_id: u16,
    /// The NIT's version_number at the time this data was collected. A
    /// broadcaster bumps this whenever the NIT's content actually changes
    /// (e.g. a transponder added/removed, network renamed).
    pub version_number: u8,
    pub network_name: Option<String>,
    pub transport_streams: Vec<TransportStreamEntry>,
    /// Which section_numbers (0..=last_section_number) have been collected
    /// so far, for this network_id/version_number. Persisted in the
    /// struct itself (rather than discarded as fetch-local state) so
    /// completeness can be tracked across multiple independent fetch
    /// attempts, not just within one.
    sections_seen: Vec<bool>,
}

impl NitData {
    /// True once every section of this NIT (for this version_number) has
    /// been collected - i.e. nothing more is missing.
    pub fn is_complete(&self) -> bool {
        !self.sections_seen.is_empty() && self.sections_seen.iter().all(|&s| s)
    }

    /// One char per section, in order - 'x' if collected, '.' if not. E.g.
    /// "x." for a 2-section NIT where only section 0 has arrived so far.
    /// For a progress display before the name (usually in section 0)
    /// arrives.
    pub fn section_map(&self) -> String {
        self.sections_seen.iter().map(|&s| if s { 'x' } else { '.' }).collect()
    }

    /// Folds a later, independent fetch attempt's result into this one.
    /// Each attempt starts from scratch (its own fresh demux read), so two
    /// attempts can easily catch *different* sections of the same
    /// multi-section NIT - one might get the name but not every transport
    /// stream, another might get more streams but not the name. Replacing
    /// wholesale would let a later, less-complete catch erase earlier
    /// progress; merging instead means what's known only ever grows - as
    /// long as both attempts are looking at the same `version_number`.
    ///
    /// If the broadcaster updated the NIT between visits, the versions
    /// won't match, and merging their sections would produce an
    /// inconsistent mix of old and new content. In that case `other` is
    /// handed back to the caller instead of being absorbed: the caller is
    /// expected to stage it separately and only swap it in once *it*
    /// becomes complete on its own, rather than losing a (possibly
    /// complete) older version for a still-partial newer one.
    pub fn merge(&mut self, other: NitData) -> Option<NitData> {
        if self.version_number != other.version_number {
            return Some(other);
        }
        if self.network_name.is_none() {
            self.network_name = other.network_name;
        }
        for ts in other.transport_streams {
            let known = self
                .transport_streams
                .iter()
                .any(|e| e.transport_stream_id == ts.transport_stream_id);
            if !known {
                self.transport_streams.push(ts);
            }
        }
        for (i, seen) in other.sections_seen.into_iter().enumerate() {
            if let Some(slot) = self.sections_seen.get_mut(i) {
                *slot |= seen;
            }
        }
        None
    }
}

struct ParsedSection {
    network_id: u16,
    version_number: u8,
    section_number: u8,
    last_section_number: u8,
    network_name: Option<String>,
    transport_streams: Vec<TransportStreamEntry>,
}

impl ParsedSection {
    /// Wraps just this one section as a single-section NitData, suitable
    /// for feeding into an existing NitData's `merge()`.
    fn into_nit_data(self) -> NitData {
        let mut sections_seen = vec![false; self.last_section_number as usize + 1];
        if let Some(seen) = sections_seen.get_mut(self.section_number as usize) {
            *seen = true;
        }
        NitData {
            network_id: self.network_id,
            version_number: self.version_number,
            network_name: self.network_name,
            transport_streams: self.transport_streams,
            sections_seen,
        }
    }
}

pub type Watch = si::Watch;
pub type NitUpdate = si::Update<NitData>;

/// Runs forever on its own thread - see si::run_listener.
pub fn run_listener(
    adapter: u32,
    frontend: u32,
    trace: bool,
    cmd_rx: std::sync::mpsc::Receiver<Watch>,
    update_tx: std::sync::mpsc::Sender<NitUpdate>,
) {
    si::run_listener(adapter, frontend, trace, NIT_PID, TABLE_ID_NIT_ACTUAL, "nit", cmd_rx, update_tx, |data, trace| {
        parse_nit_section(data, trace).map(ParsedSection::into_nit_data)
    });
}

/// Opens the demux for `adapter`/`frontend`, filters for the NIT (actual
/// network) section(s) on PID 0x10, and returns the assembled NIT if all
/// its sections arrive within `timeout`. When `trace` is set, dumps every
/// received section and the descriptor-parsing steps to stderr.
///
/// `known_version`, if given, is a version_number the caller already has
/// fully assembled - sections carrying that version are cheaply skipped
/// (checked before doing any real parsing) rather than reprocessed, since
/// they can't tell us anything new. Only a *different* version is worth
/// the work of tracking and accumulating.
pub fn read_nit(
    adapter: u32,
    frontend: u32,
    timeout: Duration,
    trace: bool,
    known_version: Option<u8>,
) -> Option<NitData> {
    let path = format!("/dev/dvb/adapter{adapter}/demux{frontend}");
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            if trace {
                eprintln!("nit: opening {path}: {e}");
            }
            return None;
        }
    };
    let fd = file.as_raw_fd();
    if trace {
        eprintln!("nit: opened {path}, filtering PID 0x{NIT_PID:04x} table_id 0x{TABLE_ID_NIT_ACTUAL:02x}");
    }

    let mut params = DmxSctFilterParams {
        pid: NIT_PID,
        filter: DmxFilter {
            filter: [0; 16],
            mask: [0; 16],
            mode: [0; 16],
        },
        timeout: timeout.as_millis() as u32,
        flags: DMX_CHECK_CRC | DMX_IMMEDIATE_START,
    };
    params.filter.filter[0] = TABLE_ID_NIT_ACTUAL;
    params.filter.mask[0] = 0xFF;

    if unsafe { libc::ioctl(fd, DMX_SET_FILTER as _, &params) } < 0 {
        if trace {
            eprintln!("nit: DMX_SET_FILTER failed: {}", std::io::Error::last_os_error());
        }
        return None;
    }

    let start = Instant::now();
    let mut buf = [0u8; 4096];
    let mut result: Option<NitData> = None;

    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            if trace {
                eprintln!("nit: timed out after {:.1}s", timeout.as_secs_f32());
            }
            return result;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as i32) };
        if rc <= 0 {
            if trace {
                eprintln!("nit: poll returned {rc} (no data before timeout)");
            }
            return result;
        }
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n <= 0 {
            if trace {
                eprintln!("nit: read returned {n}: {}", std::io::Error::last_os_error());
            }
            return result;
        }
        let data = &buf[..n as usize];
        if trace {
            eprintln!("nit: received {n} bytes: {}", hex(data));
        }

        // Cheap pre-check on just the header, before the real parse: if
        // this section is a version we've already fully got, there's
        // nothing it can teach us - skip it without even walking its
        // descriptor loop.
        if data.len() > 5
            && data[0] == TABLE_ID_NIT_ACTUAL
            && known_version == Some((data[5] >> 1) & 0x1F)
        {
            if trace {
                eprintln!("nit: section matches already-known version {}, skipping", (data[5] >> 1) & 0x1F);
            }
            continue;
        }

        let Some(section) = parse_nit_section(data, trace) else {
            continue;
        };

        if let Some(nit) = &result
            && (section.network_id != nit.network_id || section.version_number != nit.version_number)
        {
            // Different network (shouldn't happen given our table_id
            // filter, but be safe), or the broadcaster updated the NIT
            // mid-collection - either way, old partial sections are no
            // longer consistent with what we're receiving now.
            if trace && section.version_number != nit.version_number {
                eprintln!(
                    "nit: version changed {} -> {} mid-collection, restarting",
                    nit.version_number, section.version_number
                );
            }
            result = None;
        }

        if result.is_none() {
            result = Some(NitData {
                network_id: section.network_id,
                version_number: section.version_number,
                network_name: None,
                transport_streams: Vec::new(),
                sections_seen: vec![false; section.last_section_number as usize + 1],
            });
        }
        let nit = result.as_mut().unwrap();
        let idx = section.section_number as usize;
        if idx < nit.sections_seen.len() && !nit.sections_seen[idx] {
            nit.sections_seen[idx] = true;
            if section.network_name.is_some() {
                nit.network_name = section.network_name;
            }
            nit.transport_streams.extend(section.transport_streams);
        }
        if nit.is_complete() {
            if trace {
                eprintln!("nit: all {} section(s) collected", nit.sections_seen.len());
            }
            return result;
        }
    }
}

fn parse_nit_section(data: &[u8], trace: bool) -> Option<ParsedSection> {
    if data.len() < 3 {
        if trace {
            eprintln!("nit: section too short ({} bytes)", data.len());
        }
        return None;
    }
    if data[0] != TABLE_ID_NIT_ACTUAL {
        if trace {
            eprintln!("nit: unexpected table_id 0x{:02x} (wanted 0x{TABLE_ID_NIT_ACTUAL:02x})", data[0]);
        }
        return None;
    }
    let section_length = (((data[1] & 0x0F) as usize) << 8) | data[2] as usize;
    let total_len = 3 + section_length;
    if data.len() < total_len {
        if trace {
            eprintln!("nit: section_length {section_length} wants {total_len} bytes, only have {}", data.len());
        }
        return None;
    }
    let header_end = 3 + 5;
    if data.len() < header_end + 2 {
        if trace {
            eprintln!("nit: section too short for network_descriptors_length field");
        }
        return None;
    }
    let network_id = u16::from_be_bytes([data[3], data[4]]);
    // data[5] = reserved(2) + version_number(5) + current_next_indicator(1)
    let version_number = (data[5] >> 1) & 0x1F;
    let section_number = data[6];
    let last_section_number = data[7];
    let desc_len = (((data[header_end] & 0x0F) as usize) << 8) | data[header_end + 1] as usize;
    let desc_start = header_end + 2;
    let desc_end = (desc_start + desc_len).min(data.len());
    if trace {
        eprintln!(
            "nit: network_id=0x{network_id:04x} version={version_number} section={section_number}/{last_section_number} descriptor_loop_len={desc_len}"
        );
    }

    let mut network_name = None;
    let mut i = desc_start;
    while i + 2 <= desc_end {
        let tag = data[i];
        let len = data[i + 1] as usize;
        let start = i + 2;
        let end = (start + len).min(desc_end);
        if trace {
            eprintln!("nit: network descriptor tag=0x{tag:02x} len={len} data={}", hex(&data[start..end]));
        }
        if tag == DESCRIPTOR_TAG_NETWORK_NAME {
            network_name = Some(decode_dvb_text(&data[start..end]));
        }
        i = end;
    }

    let transport_streams = if data.len() >= desc_end + 2 {
        let ts_loop_len = (((data[desc_end] & 0x0F) as usize) << 8) | data[desc_end + 1] as usize;
        let ts_start = desc_end + 2;
        let ts_end = (ts_start + ts_loop_len).min(data.len());
        parse_ts_loop(&data[ts_start..ts_end], trace)
    } else {
        Vec::new()
    };

    Some(ParsedSection {
        network_id,
        version_number,
        section_number,
        last_section_number,
        network_name,
        transport_streams,
    })
}

fn parse_ts_loop(data: &[u8], trace: bool) -> Vec<TransportStreamEntry> {
    let mut out = Vec::new();
    let mut j = 0;
    while j + 6 <= data.len() {
        let tsid = u16::from_be_bytes([data[j], data[j + 1]]);
        let onid = u16::from_be_bytes([data[j + 2], data[j + 3]]);
        let td_len = (((data[j + 4] & 0x0F) as usize) << 8) | data[j + 5] as usize;
        let td_start = j + 6;
        let td_end = (td_start + td_len).min(data.len());

        let mut satellite = None;
        let mut other = Vec::new();
        let mut k = td_start;
        while k + 2 <= td_end {
            let tag = data[k];
            let len = data[k + 1] as usize;
            let s = k + 2;
            let e = (s + len).min(td_end);
            if trace {
                eprintln!(
                    "nit: ts 0x{tsid:04x}/0x{onid:04x} descriptor tag=0x{tag:02x} len={len} data={}",
                    hex(&data[s..e])
                );
            }
            if tag == DESCRIPTOR_TAG_SATELLITE_DELIVERY {
                satellite = parse_satellite_delivery(&data[s..e]);
            } else {
                other.push((tag, data[s..e].to_vec()));
            }
            k = e;
        }
        out.push(TransportStreamEntry {
            transport_stream_id: tsid,
            original_network_id: onid,
            satellite,
            other_descriptors: other,
        });
        j = td_end;
    }
    out
}

fn bcd_to_u64(bytes: &[u8]) -> u64 {
    let mut v = 0u64;
    for &b in bytes {
        v = v * 100 + (b >> 4) as u64 * 10 + (b & 0x0F) as u64;
    }
    v
}

/// Decodes satellite_delivery_system_descriptor, EN 300 468 table 28
/// (fixed 11-byte payload after tag+length).
fn parse_satellite_delivery(d: &[u8]) -> Option<SatelliteDelivery> {
    if d.len() < 11 {
        return None;
    }
    let frequency_mhz = bcd_to_u64(&d[0..4]) as f64 / 100.0; // BCD unit = 10 kHz
    let orbital_position = bcd_to_u64(&d[4..6]) as f64 / 10.0; // BCD unit = 0.1 deg
    let b6 = d[6];
    let west = (b6 & 0x80) == 0;
    let polarization = match (b6 >> 5) & 0x03 {
        0 => 'H',
        1 => 'V',
        2 => 'L',
        _ => 'R',
    };
    let is_s2 = (b6 >> 2) & 0x01 == 1;
    let rolloff = if is_s2 {
        Some(match (b6 >> 3) & 0x03 {
            0 => 0.35,
            1 => 0.25,
            2 => 0.20,
            _ => 0.0, // reserved
        })
    } else {
        None
    };
    let modulation = match b6 & 0x03 {
        1 => "QPSK",
        2 => "8PSK",
        3 => "16APSK",
        _ => "AUTO",
    };
    let raw = u32::from_be_bytes([d[7], d[8], d[9], d[10]]);
    let sr_bcd_bits = raw >> 4; // top 28 bits: 7 BCD digits
    let mut sr_val = 0u64;
    for shift in (0..7).rev() {
        sr_val = sr_val * 10 + ((sr_bcd_bits >> (shift * 4)) & 0xF) as u64;
    }
    let symbol_rate_ksps = (sr_val / 10) as u32; // BCD unit = 100 sym/s
    let fec_inner = match raw & 0x0F {
        1 => "1/2",
        2 => "2/3",
        3 => "3/4",
        4 => "5/6",
        5 => "7/8",
        6 => "8/9",
        7 => "3/5",
        8 => "4/5",
        9 => "9/10",
        0xF => "none",
        _ => "?",
    };

    Some(SatelliteDelivery {
        frequency_mhz,
        orbital_position,
        west,
        polarization,
        rolloff,
        system: if is_s2 { "DVB-S2" } else { "DVB-S" },
        modulation,
        symbol_rate_ksps,
        fec_inner,
    })
}
