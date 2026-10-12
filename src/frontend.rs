use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::ioctl::{self, DtvProperty, Stat};
use crate::input::{InputConfig, Step};
use crate::transponder::Transponder;

pub struct Frontend {
    file: std::fs::File,
    verbose: bool,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")
}

fn voltage_name(v: i32) -> &'static str {
    match v {
        ioctl::SEC_VOLTAGE_13 => "13V",
        ioctl::SEC_VOLTAGE_18 => "18V",
        _ => "off",
    }
}

fn tone_name(t: i32) -> &'static str {
    if t == ioctl::SEC_TONE_ON { "on" } else { "off" }
}

fn burst_name(b: i32) -> &'static str {
    if b == ioctl::SEC_MINI_A { "A" } else { "B" }
}

impl Frontend {
    pub fn open(adapter: u32, frontend: u32, verbose: bool) -> Result<Frontend> {
        let path = format!("/dev/dvb/adapter{adapter}/frontend{frontend}");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .with_context(|| format!("opening {path}"))?;
        if verbose {
            eprintln!("fe: opened {path}");
        }
        Ok(Frontend { file, verbose })
    }

    fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    pub fn supported_delivery_systems(&self) -> Result<Vec<u32>> {
        Ok(ioctl::enum_delivery_systems(self.fd())?)
    }

    /// Reads back the modulation the demodulator actually settled on. When
    /// we tune with AUTO (terrestrial, since we don't know the real
    /// constellation up front), this is how we later learn what it really
    /// is - more reliable than guessing from SI tables, and works the same
    /// way for every delivery system.
    pub fn modulation(&self) -> Result<u32> {
        Ok(ioctl::get_property_data(self.fd(), ioctl::DTV_MODULATION)?)
    }

    fn log(&self, msg: impl std::fmt::Display) {
        if self.verbose {
            eprintln!("  {msg}");
        }
    }

    fn run_steps(&self, steps: &[Step]) -> Result<()> {
        for step in steps {
            match step {
                Step::Voltage(v) => {
                    self.log(format!("sec: voltage {}", voltage_name(*v)));
                    ioctl::set_voltage(self.fd(), *v)?;
                }
                Step::Tone(t) => {
                    self.log(format!("sec: tone {}", tone_name(*t)));
                    ioctl::set_tone(self.fd(), *t)?;
                }
                Step::Burst(b) => {
                    self.log(format!("sec: burst {}", burst_name(*b)));
                    ioctl::send_burst(self.fd(), *b)?;
                }
                Step::Diseqc(bytes) => {
                    self.log(format!("sec: diseqc send [{}]", hex(bytes)));
                    ioctl::diseqc_send(self.fd(), bytes)?;
                }
                Step::SleepMs(ms) => {
                    self.log(format!("sec: sleep {ms}ms"));
                    sleep(Duration::from_millis(*ms as u64));
                }
            }
        }
        Ok(())
    }

    pub fn tune(&self, input: &InputConfig, tp: &Transponder) -> Result<()> {
        self.log(format!(
            "tune: {} {} MHz pol={} sr={} fec={:#x} mod={:#x}",
            tp.system,
            tp.display_frequency,
            if tp.pol_h { "H" } else { "V" },
            tp.symbol_rate,
            tp.fec,
            tp.modulation,
        ));

        let frequency = if tp.system.is_satellite() {
            let seq = crate::input::plan(input, tp.position, tp.frequency, tp.pol_h)?;
            self.log(format!(
                "input: {:?} -> frontend frequency {} kHz",
                input.kind, seq.frequency_khz
            ));
            self.run_steps(&seq.steps)?;
            seq.frequency_khz
        } else {
            tp.frequency
        };

        self.log("fe: DTV_CLEAR");
        let mut clear = [DtvProperty::data(ioctl::DTV_CLEAR, 0)];
        ioctl::set_properties(self.fd(), &mut clear)?;

        let mut named: Vec<(&str, u32, u32)> = vec![
            ("DTV_DELIVERY_SYSTEM", ioctl::DTV_DELIVERY_SYSTEM, tp.system.as_dtv()),
            ("DTV_FREQUENCY", ioctl::DTV_FREQUENCY, frequency),
            ("DTV_INVERSION", ioctl::DTV_INVERSION, ioctl::INVERSION_AUTO),
            ("DTV_MODULATION", ioctl::DTV_MODULATION, tp.modulation),
            ("DTV_STREAM_ID", ioctl::DTV_STREAM_ID, tp.plp_id),
        ];

        if tp.system.is_satellite() {
            named.push(("DTV_SYMBOL_RATE", ioctl::DTV_SYMBOL_RATE, tp.symbol_rate));
            named.push(("DTV_INNER_FEC", ioctl::DTV_INNER_FEC, tp.fec));
            if matches!(tp.system, crate::transponder::System::Dvbs2) {
                named.push(("DTV_ROLLOFF", ioctl::DTV_ROLLOFF, tp.rolloff));
                named.push(("DTV_PILOT", ioctl::DTV_PILOT, ioctl::PILOT_AUTO));
            }
        } else {
            named.push(("DTV_BANDWIDTH_HZ", ioctl::DTV_BANDWIDTH_HZ, tp.bandwidth_hz));
        }

        for (name, _, val) in &named {
            self.log(format!("fe: set {name} = {val}"));
        }

        let mut props: Vec<DtvProperty> = named
            .iter()
            .map(|(_, cmd, val)| DtvProperty::data(*cmd, *val))
            .collect();
        props.push(DtvProperty::data(ioctl::DTV_TUNE, 0));

        ioctl::set_properties(self.fd(), &mut props).context("tuning frontend")?;
        self.log("fe: FE_SET_PROPERTY ok (tuned)");
        Ok(())
    }

    pub fn status(&self) -> Result<u32> {
        Ok(ioctl::read_status(self.fd(), self.verbose)?)
    }

    pub fn stats(&self) -> Result<Stats> {
        let s = ioctl::read_stats(self.fd(), self.verbose)?;
        Ok(Stats {
            signal: s.signal_strength,
            cnr: s.cnr,
            pre_error_bits: s.pre_error_bits,
            pre_total_bits: s.pre_total_bits,
            post_error_bits: s.post_error_bits,
            post_total_bits: s.post_total_bits,
            error_blocks: s.error_blocks,
            total_blocks: s.total_blocks,
            ber: ber(s.post_error_bits, s.post_total_bits),
            corrected_bits: corrected(s.pre_error_bits, s.post_error_bits),
        })
    }
}

fn ber(errors: Stat, total: Stat) -> Option<f64> {
    if let (Stat::Counter(e), Stat::Counter(t)) = (errors, total)
        && t > 0 {
            return Some(e as f64 / t as f64);
        }
    None
}

/// How many bit errors FEC actually fixed: the gap between the error count
/// *before* correction (PRE) and what's left over *after* (POST). Both are
/// cumulative since tune, like `error_blocks` - not a per-tick delta.
fn corrected(pre_errors: Stat, post_errors: Stat) -> Stat {
    match (pre_errors, post_errors) {
        (Stat::Counter(pre), Stat::Counter(post)) => Stat::Counter(pre.saturating_sub(post)),
        _ => Stat::Unavailable,
    }
}

pub struct Stats {
    pub signal: Stat,
    pub cnr: Stat,
    pub pre_error_bits: Stat,
    pub pre_total_bits: Stat,
    pub post_error_bits: Stat,
    pub post_total_bits: Stat,
    pub error_blocks: Stat,
    pub total_blocks: Stat,
    /// Derived convenience: post_error_bits / post_total_bits.
    pub ber: Option<f64>,
    /// Derived convenience: pre_error_bits - post_error_bits.
    pub corrected_bits: Stat,
}

pub const FE_HAS_LOCK: u32 = ioctl::FE_HAS_LOCK;

pub fn fmt_stat(s: Stat) -> String {
    match s {
        Stat::Decibel(v) => format!("{:.2} dB", v as f64 / 1000.0),
        Stat::Relative(v) => format!("{:.0}%", v as f64 / 65535.0 * 100.0),
        _ => "-".to_string(),
    }
}

pub fn fmt_ber(b: Option<f64>) -> String {
    match b {
        Some(v) => format!("{v:.2e}"),
        None => "-".to_string(),
    }
}

pub fn fmt_blocks(s: Stat) -> String {
    match s {
        Stat::Counter(v) => v.to_string(),
        _ => "-".to_string(),
    }
}
