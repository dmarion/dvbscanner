//! SDT (Service Description Table, ETSI EN 300 468) section parsing, on
//! top of the generic listener engine in si.rs. Lists the actual
//! services/channels carried by a transponder - a level below NIT (which
//! only names the network as a whole).

use crate::si::{self, decode_dvb_text, hex};

const SDT_PID: u16 = 0x11;
const TABLE_ID_SDT_ACTUAL: u8 = 0x42;
const DESCRIPTOR_TAG_SERVICE: u8 = 0x48;

/// Short label for a running_status value (EN 300 468 table 6).
pub fn running_status_name(s: u8) -> &'static str {
    match s {
        1 => "not running",
        2 => "starting",
        3 => "pausing",
        4 => "running",
        5 => "off-air",
        _ => "?",
    }
}

/// Short label for a service_type value (EN 300 468 table 87) - just the
/// common ones seen in the wild, anything else falls back to its hex code.
pub fn service_type_name(t: u8) -> &'static str {
    match t {
        0x01 => "TV",
        0x02 => "Radio",
        0x0A => "Radio (adv.)",
        0x11 => "HD TV",
        0x16 => "SD TV (adv.)",
        0x19 => "HD TV (adv.)",
        0x1F => "UHD TV",
        _ => "?",
    }
}

#[derive(Debug, Clone)]
pub struct ServiceEntry {
    pub service_id: u16,
    pub service_type: u8,
    pub running_status: u8,
    pub free_ca_mode: bool,
    pub provider_name: Option<String>,
    pub service_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SdtData {
    pub transport_stream_id: u16,
    pub original_network_id: u16,
    pub version_number: u8,
    pub services: Vec<ServiceEntry>,
    /// Which section_numbers (0..=last_section_number) have been collected
    /// so far - same purpose as NitData::sections_seen.
    sections_seen: Vec<bool>,
}

impl SdtData {
    pub fn is_complete(&self) -> bool {
        !self.sections_seen.is_empty() && self.sections_seen.iter().all(|&s| s)
    }

    pub fn section_map(&self) -> String {
        self.sections_seen.iter().map(|&s| if s { 'x' } else { '.' }).collect()
    }

    /// Folds a later, independent fetch attempt into this one - see
    /// NitData::merge for why this merges rather than replaces.
    pub fn merge(&mut self, other: SdtData) -> Option<SdtData> {
        if self.version_number != other.version_number {
            return Some(other);
        }
        for svc in other.services {
            let known = self.services.iter().any(|e| e.service_id == svc.service_id);
            if !known {
                self.services.push(svc);
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
    transport_stream_id: u16,
    original_network_id: u16,
    version_number: u8,
    section_number: u8,
    last_section_number: u8,
    services: Vec<ServiceEntry>,
}

impl ParsedSection {
    fn into_sdt_data(self) -> SdtData {
        let mut sections_seen = vec![false; self.last_section_number as usize + 1];
        if let Some(seen) = sections_seen.get_mut(self.section_number as usize) {
            *seen = true;
        }
        SdtData {
            transport_stream_id: self.transport_stream_id,
            original_network_id: self.original_network_id,
            version_number: self.version_number,
            services: self.services,
            sections_seen,
        }
    }
}

pub type Watch = si::Watch;
pub type SdtUpdate = si::Update<SdtData>;

/// Runs forever on its own thread - see si::run_listener.
pub fn run_listener(
    adapter: u32,
    frontend: u32,
    trace: bool,
    cmd_rx: std::sync::mpsc::Receiver<Watch>,
    update_tx: std::sync::mpsc::Sender<SdtUpdate>,
) {
    si::run_listener(adapter, frontend, trace, SDT_PID, TABLE_ID_SDT_ACTUAL, "sdt", cmd_rx, update_tx, |data, trace| {
        parse_sdt_section(data, trace).map(ParsedSection::into_sdt_data)
    });
}

fn parse_sdt_section(data: &[u8], trace: bool) -> Option<ParsedSection> {
    if data.len() < 11 {
        if trace {
            eprintln!("sdt: section too short ({} bytes)", data.len());
        }
        return None;
    }
    if data[0] != TABLE_ID_SDT_ACTUAL {
        if trace {
            eprintln!("sdt: unexpected table_id 0x{:02x} (wanted 0x{TABLE_ID_SDT_ACTUAL:02x})", data[0]);
        }
        return None;
    }
    let section_length = (((data[1] & 0x0F) as usize) << 8) | data[2] as usize;
    let total_len = 3 + section_length;
    if data.len() < total_len {
        if trace {
            eprintln!("sdt: section_length {section_length} wants {total_len} bytes, only have {}", data.len());
        }
        return None;
    }
    let transport_stream_id = u16::from_be_bytes([data[3], data[4]]);
    // data[5] = reserved(2) + version_number(5) + current_next_indicator(1)
    let version_number = (data[5] >> 1) & 0x1F;
    let section_number = data[6];
    let last_section_number = data[7];
    let original_network_id = u16::from_be_bytes([data[8], data[9]]);
    // data[10] = reserved_future_use
    if trace {
        eprintln!(
            "sdt: transport_stream_id=0x{transport_stream_id:04x} version={version_number} section={section_number}/{last_section_number}"
        );
    }

    // Service loop runs from byte 11 to just before the trailing CRC32.
    let services_end = total_len.saturating_sub(4).min(data.len());
    let mut services = Vec::new();
    let mut j = 11;
    while j + 5 <= services_end {
        let service_id = u16::from_be_bytes([data[j], data[j + 1]]);
        let b3 = data[j + 3];
        let b4 = data[j + 4];
        let running_status = (b3 >> 5) & 0x07;
        let free_ca_mode = (b3 >> 4) & 0x01 == 1;
        let desc_len = (((b3 & 0x0F) as usize) << 8) | b4 as usize;
        let desc_start = j + 5;
        let desc_end = (desc_start + desc_len).min(services_end);

        let mut service_type = 0u8;
        let mut provider_name = None;
        let mut service_name = None;
        let mut k = desc_start;
        while k + 2 <= desc_end {
            let tag = data[k];
            let len = data[k + 1] as usize;
            let s = k + 2;
            let e = (s + len).min(desc_end);
            if trace {
                eprintln!(
                    "sdt: service 0x{service_id:04x} descriptor tag=0x{tag:02x} len={len} data={}",
                    hex(&data[s..e])
                );
            }
            if tag == DESCRIPTOR_TAG_SERVICE && !data[s..e].is_empty() {
                let payload = &data[s..e];
                service_type = payload[0];
                if let Some(&prov_len) = payload.get(1) {
                    let prov_len = prov_len as usize;
                    let prov_start = 2;
                    let prov_end = (prov_start + prov_len).min(payload.len());
                    provider_name = Some(decode_dvb_text(&payload[prov_start..prov_end]));
                    if let Some(&name_len) = payload.get(prov_end) {
                        let name_start = prov_end + 1;
                        let name_end = (name_start + name_len as usize).min(payload.len());
                        service_name = Some(decode_dvb_text(&payload[name_start..name_end]));
                    }
                }
            }
            k = e;
        }
        services.push(ServiceEntry {
            service_id,
            service_type,
            running_status,
            free_ca_mode,
            provider_name,
            service_name,
        });
        j = desc_end;
    }

    Some(ParsedSection {
        transport_stream_id,
        original_network_id,
        version_number,
        section_number,
        last_section_number,
        services,
    })
}
