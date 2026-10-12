use anyhow::{anyhow, bail, Result};
use serde::Deserialize;

use crate::ioctl;

#[derive(Debug, Deserialize, Clone)]
pub struct TransponderConfig {
    /// "S", "S2", "T" or "T2"
    pub system: String,
    /// MHz (fractional, e.g. 10719.25)
    pub frequency: f64,
    #[serde(default)]
    pub polarization: Option<String>,
    /// ksym/s
    #[serde(default)]
    pub symbol_rate: Option<u32>,
    #[serde(default)]
    pub modulation: Option<String>,
    #[serde(default)]
    pub fec: Option<String>,
    #[serde(default)]
    pub rolloff: Option<String>,
    /// MHz, terrestrial only (fractional, e.g. 1.712)
    #[serde(default)]
    pub bandwidth: Option<f64>,
    #[serde(default)]
    pub plp_id: Option<u32>,
    /// Satellite/input-leg position for this transponder, as a letter (A,
    /// B, C, ...) matching the TUI's own sat-letter column - A is the
    /// first satellite/leg, B the second, etc. Needed for a combined scan
    /// list spanning more than one satellite on the same multi-sat input;
    /// defaults to A when unset (single-satellite setups). Meaningless for
    /// terrestrial entries.
    #[serde(default, deserialize_with = "de_position_opt")]
    pub position: Option<u8>,
}

fn de_position_opt<'de, D>(deserializer: D) -> std::result::Result<Option<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: Option<String> = Option::deserialize(deserializer)?;
    s.map(|s| crate::input::parse_position(&s).map_err(serde::de::Error::custom)).transpose()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum System {
    Dvbs,
    Dvbs2,
    Dvbt,
    Dvbt2,
}

impl System {
    pub fn is_satellite(self) -> bool {
        matches!(self, System::Dvbs | System::Dvbs2)
    }

    pub fn as_dtv(self) -> u32 {
        match self {
            System::Dvbs => ioctl::SYS_DVBS,
            System::Dvbs2 => ioctl::SYS_DVBS2,
            System::Dvbt => ioctl::SYS_DVBT,
            System::Dvbt2 => ioctl::SYS_DVBT2,
        }
    }
}

impl std::fmt::Display for System {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            System::Dvbs => "S",
            System::Dvbs2 => "S2",
            System::Dvbt => "T",
            System::Dvbt2 => "T2",
        };
        write!(f, "{s}")
    }
}

/// A transponder with all parameters resolved to the numeric values the
/// S2API / input code needs, with units normalized to Hz/kHz.
#[derive(Debug, Clone)]
pub struct Transponder {
    pub system: System,
    /// MHz, as entered by the user - used only for display.
    pub display_frequency: f64,
    /// kHz (satellite RF frequency) or Hz (terrestrial), as used by the API.
    pub frequency: u32,
    pub pol_h: bool,
    pub symbol_rate: u32, // sym/s
    pub modulation: u32,
    pub fec: u32,
    pub rolloff: u32,
    pub bandwidth_hz: u32,
    pub plp_id: u32,
    /// Satellite/input-leg position - see TransponderConfig::position.
    /// Meaningless (always 0) for terrestrial entries.
    pub position: u8,
}

fn parse_fec(s: &str) -> Result<u32> {
    Ok(match s {
        "none" | "NONE" => ioctl::FEC_NONE,
        "1/2" => ioctl::FEC_1_2,
        "2/3" => ioctl::FEC_2_3,
        "3/4" => ioctl::FEC_3_4,
        "4/5" => ioctl::FEC_4_5,
        "5/6" => ioctl::FEC_5_6,
        "6/7" => ioctl::FEC_6_7,
        "7/8" => ioctl::FEC_7_8,
        "8/9" => ioctl::FEC_8_9,
        "3/5" => ioctl::FEC_3_5,
        "9/10" => ioctl::FEC_9_10,
        "2/5" => ioctl::FEC_2_5,
        "auto" | "AUTO" => ioctl::FEC_AUTO,
        other => bail!("unknown FEC '{other}'"),
    })
}

fn parse_modulation(s: &str) -> Result<u32> {
    Ok(match s.to_uppercase().as_str() {
        "QPSK" => ioctl::QPSK,
        "8PSK" => ioctl::PSK_8,
        "16APSK" => ioctl::APSK_16,
        "32APSK" => ioctl::APSK_32,
        "QAM16" | "16QAM" => ioctl::QAM_16,
        "QAM32" | "32QAM" => ioctl::QAM_32,
        "QAM64" | "64QAM" => ioctl::QAM_64,
        "QAM128" | "128QAM" => ioctl::QAM_128,
        "QAM256" | "256QAM" => ioctl::QAM_256,
        "AUTO" => ioctl::QAM_AUTO,
        other => bail!("unknown modulation '{other}'"),
    })
}

/// Short display name for a DTV_MODULATION value, e.g. for a TUI column.
pub fn modulation_name(m: u32) -> &'static str {
    match m {
        ioctl::QPSK => "QPSK",
        ioctl::PSK_8 => "8PSK",
        ioctl::APSK_16 => "16APSK",
        ioctl::APSK_32 => "32APSK",
        ioctl::QAM_16 => "16QAM",
        ioctl::QAM_32 => "32QAM",
        ioctl::QAM_64 => "64QAM",
        ioctl::QAM_128 => "128QAM",
        ioctl::QAM_256 => "256QAM",
        ioctl::QAM_AUTO => "AUTO",
        _ => "?",
    }
}

fn parse_rolloff(s: &str) -> Result<u32> {
    Ok(match s {
        "0.35" | "35" => ioctl::ROLLOFF_35,
        "0.25" | "25" => ioctl::ROLLOFF_25,
        "0.20" | "0.2" | "20" => ioctl::ROLLOFF_20,
        "auto" | "AUTO" => ioctl::ROLLOFF_AUTO,
        other => bail!("unknown rolloff '{other}'"),
    })
}

impl TransponderConfig {
    pub fn resolve(&self) -> Result<Transponder> {
        let system = match self.system.to_uppercase().as_str() {
            "S" => System::Dvbs,
            "S2" => System::Dvbs2,
            "T" => System::Dvbt,
            "T2" => System::Dvbt2,
            other => bail!("unknown system '{other}'"),
        };

        if system.is_satellite() {
            let pol = self
                .polarization
                .as_deref()
                .ok_or_else(|| anyhow!("satellite transponder at {} MHz needs 'polarization'", self.frequency))?;
            let pol_h = match pol.to_uppercase().as_str() {
                "H" | "L" => true,
                "V" | "R" => false,
                other => bail!("unknown polarization '{other}'"),
            };
            let symbol_rate = self
                .symbol_rate
                .ok_or_else(|| anyhow!("satellite transponder at {} MHz needs 'symbol_rate'", self.frequency))?
                * 1000;
            let modulation = match &self.modulation {
                Some(m) => parse_modulation(m)?,
                None if system == System::Dvbs => ioctl::QPSK,
                None => ioctl::PSK_8,
            };
            let fec = match &self.fec {
                Some(f) => parse_fec(f)?,
                None => ioctl::FEC_AUTO,
            };
            let rolloff = match &self.rolloff {
                Some(r) => parse_rolloff(r)?,
                None => ioctl::ROLLOFF_35,
            };
            Ok(Transponder {
                system,
                display_frequency: self.frequency,
                frequency: (self.frequency * 1000.0).round() as u32, // kHz
                pol_h,
                symbol_rate,
                modulation,
                fec,
                rolloff,
                bandwidth_hz: 0,
                plp_id: 0,
                position: self.position.unwrap_or(0),
            })
        } else {
            let bandwidth = self
                .bandwidth
                .ok_or_else(|| anyhow!("terrestrial transponder at {} MHz needs 'bandwidth'", self.frequency))?;
            Ok(Transponder {
                system,
                display_frequency: self.frequency,
                frequency: (self.frequency * 1_000_000.0).round() as u32, // Hz
                pol_h: false,
                symbol_rate: 0,
                modulation: ioctl::QAM_AUTO,
                fec: ioctl::FEC_AUTO,
                rolloff: ioctl::ROLLOFF_AUTO,
                bandwidth_hz: (bandwidth * 1_000_000.0).round() as u32,
                plp_id: self.plp_id.unwrap_or(ioctl::NO_STREAM_ID_FILTER),
                position: 0, // meaningless for terrestrial
            })
        }
    }
}
