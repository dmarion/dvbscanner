//! DiSEqC/Unicable input control: computes the voltage/tone/DiSEqC sequence needed
//! to select a transponder, and the frequency the frontend itself should
//! be tuned to afterwards.
//!
//! EN50494 (Unicable I) and EN50607 (Unicable II / JESS) encodings and
//! timings follow the published specs and have been verified against
//! real hardware.

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::ioctl::{SEC_MINI_A, SEC_MINI_B, SEC_TONE_OFF, SEC_TONE_ON, SEC_VOLTAGE_13, SEC_VOLTAGE_18};

#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    #[default]
    Direct,
    Unicable,
    Unicable2,
}

/// How a `direct` input selects between positions - unicable/unicable2
/// always carry the position in their own addressing instead, so this is
/// meaningless (and ignored) for those types.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiseqcKind {
    /// Mini-DiSEqC: just a 12.5ms tone burst, no DiSEqC command at all.
    /// Only 2 positions (A/B).
    Toneburst,
    /// DiSEqC 1.0 committed switch (command 0x38). Up to 4 positions
    /// (A-D), falls back to a tone burst for the low bit so it also works
    /// with switches that only understand tone bursts.
    Committed,
    /// DiSEqC 1.1 uncommitted switch (command 0x39), for larger cascaded
    /// multiswitch setups. Up to 16 positions, no tone burst needed.
    Uncommitted,
}

/// Parses a satellite/input-leg position letter ("A", "B", "C", ...) into
/// its 0-based index - the same convention the TUI's sat-letter column
/// displays, so the config file and the screen always agree without
/// anyone needing to convert between a letter and a number by hand.
pub fn parse_position(s: &str) -> Result<u8> {
    let s = s.trim();
    let mut chars = s.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        bail!("position '{s}' must be a single letter (A, B, C, ...)");
    };
    if !c.is_ascii_alphabetic() {
        bail!("position '{s}' must be a single letter (A, B, C, ...)");
    }
    Ok(c.to_ascii_uppercase() as u8 - b'A')
}

#[derive(Debug, Deserialize, Clone)]
pub struct InputConfig {
    #[serde(rename = "type", default)]
    pub kind: InputType,
    /// How a `direct` input switches position - see DiseqcKind. Absent =
    /// no switching at all (single fixed LNB, position is ignored).
    #[serde(default)]
    pub diseqc: Option<DiseqcKind>,
    /// Unicable user band slot, 1-based (slot 1 = user band id 0).
    #[serde(default)]
    pub slot: u8,
    /// Unicable user band center frequency, MHz.
    #[serde(default)]
    pub frequency: u16,
    /// Unicable PIN, 0-255. Absent = no PIN sent.
    #[serde(default)]
    pub pin: Option<u16>,
    #[serde(default = "default_lo_low")]
    pub lo_low: u32,
    #[serde(default = "default_lo_high")]
    pub lo_high: u32,
    #[serde(default = "default_switch")]
    pub switch_freq: u32,
}

fn default_lo_low() -> u32 {
    9750
}
fn default_lo_high() -> u32 {
    10600
}
fn default_switch() -> u32 {
    11700
}

impl Default for InputConfig {
    fn default() -> Self {
        InputConfig {
            kind: InputType::Direct,
            diseqc: None,
            slot: 0,
            frequency: 0,
            pin: None,
            lo_low: default_lo_low(),
            lo_high: default_lo_high(),
            switch_freq: default_switch(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Step {
    Voltage(i32),
    Tone(i32),
    Burst(i32),
    Diseqc(Vec<u8>),
    SleepMs(u32),
}

#[derive(Debug)]
pub struct TuneSequence {
    pub steps: Vec<Step>,
    /// Frequency (kHz) the frontend itself should be tuned to.
    pub frequency_khz: u32,
}

/// `freq_khz` is the real satellite downlink (RF) frequency; `pol_h` is
/// true for horizontal/left, false for vertical/right. `position` is the
/// satellite/input-leg position to use for this tune, always taken from
/// the transponder itself (see TransponderConfig::position) - there is no
/// separate input-level default to fall back on.
pub fn plan(input: &InputConfig, position: u8, freq_khz: u32, pol_h: bool) -> Result<TuneSequence> {
    let high_band = input.lo_high > 0 && freq_khz > input.switch_freq * 1000;
    let lo = if high_band { input.lo_high } else { input.lo_low };
    let l_band_khz = (freq_khz as i64 - (lo as i64) * 1000).unsigned_abs() as u32;

    let pol_bit: u32 = if pol_h { 1 } else { 0 };
    let band_bit: u32 = if high_band { 1 } else { 0 };
    let volt = if pol_h { SEC_VOLTAGE_18 } else { SEC_VOLTAGE_13 };

    match input.kind {
        InputType::Direct => match input.diseqc {
            None => Ok(TuneSequence {
                steps: vec![
                    Step::Voltage(volt),
                    Step::SleepMs(15),
                    Step::Tone(if high_band { SEC_TONE_ON } else { SEC_TONE_OFF }),
                ],
                frequency_khz: l_band_khz,
            }),

            Some(DiseqcKind::Toneburst) => {
                let pos0 = position as u32 % 2;
                let burst = if pos0 == 0 { SEC_MINI_A } else { SEC_MINI_B };
                Ok(TuneSequence {
                    steps: vec![
                        Step::Voltage(volt),
                        Step::SleepMs(15),
                        Step::Tone(SEC_TONE_OFF),
                        Step::SleepMs(15),
                        Step::Burst(burst),
                        Step::SleepMs(15),
                        Step::Tone(if high_band { SEC_TONE_ON } else { SEC_TONE_OFF }),
                    ],
                    frequency_khz: l_band_khz,
                })
            }

            Some(DiseqcKind::Committed) => {
                let pos0 = position as u32 % 4;
                let data0: u32 = 0xf0 | band_bit | (if pol_h { 0 } else { 2 }) | (pos0 << 2);
                let cmd = vec![0xE0, 0x10, 0x38, data0 as u8];
                let burst = if pos0.is_multiple_of(2) { SEC_MINI_A } else { SEC_MINI_B };
                Ok(TuneSequence {
                    steps: vec![
                        Step::Voltage(volt),
                        Step::SleepMs(15),
                        Step::Tone(SEC_TONE_OFF),
                        Step::SleepMs(15),
                        Step::Diseqc(cmd),
                        Step::SleepMs(15),
                        Step::Burst(burst),
                        Step::SleepMs(15),
                        Step::Tone(if high_band { SEC_TONE_ON } else { SEC_TONE_OFF }),
                    ],
                    frequency_khz: l_band_khz,
                })
            }

            Some(DiseqcKind::Uncommitted) => {
                let pos0 = position as u32 % 16;
                let data0: u32 = 0xf0 | pos0;
                let cmd = vec![0xE0, 0x10, 0x39, data0 as u8];
                Ok(TuneSequence {
                    steps: vec![
                        Step::Voltage(volt),
                        Step::SleepMs(15),
                        Step::Tone(SEC_TONE_OFF),
                        Step::SleepMs(15),
                        Step::Diseqc(cmd),
                        Step::SleepMs(15),
                        Step::Tone(if high_band { SEC_TONE_ON } else { SEC_TONE_OFF }),
                    ],
                    frequency_khz: l_band_khz,
                })
            }
        },

        InputType::Unicable => {
            if input.slot == 0 || input.frequency == 0 {
                bail!("unicable input requires 'slot' and 'frequency' to be set");
            }
            let id0 = (input.slot - 1) as u32;
            let t: i64 = ((l_band_khz as i64 / 1000) + 2 + input.frequency as i64) / 4 - 350;
            if !(0..1023).contains(&t) {
                bail!("unicable tuning word {t} out of range for frequency {freq_khz} kHz");
            }
            let rfreq = (t + 350) * 4000 - l_band_khz as i64;
            if rfreq <= 0 {
                bail!("unicable computed an invalid tune frequency");
            }
            let data1: u32 =
                ((id0 & 7) << 5) | ((position as u32 & 1) << 4) | (pol_bit << 3) | (band_bit << 2) | ((t as u32 >> 8) & 3);
            let data2: u32 = t as u32 & 0xff;
            let mut cmd = vec![0xE0u8, 0x10, 0, data1 as u8, data2 as u8];
            if let Some(pin) = input.pin {
                if pin > 255 {
                    bail!("unicable PIN must be 0-255");
                }
                cmd[2] = 0x5C; // CMD_NORMAL_MULTIHOME
                cmd.push(pin as u8);
            } else {
                cmd[2] = 0x5A; // CMD_NORMAL
            }
            Ok(TuneSequence {
                steps: vec![
                    Step::Voltage(SEC_VOLTAGE_18),
                    Step::SleepMs(15),
                    Step::Diseqc(cmd),
                    Step::SleepMs(50),
                    Step::Voltage(SEC_VOLTAGE_13),
                ],
                frequency_khz: rfreq as u32,
            })
        }

        InputType::Unicable2 => {
            if input.slot == 0 || input.frequency == 0 {
                bail!("unicable2 input requires 'slot' and 'frequency' to be set");
            }
            let id0 = (input.slot - 1) as u32;
            let t: i64 = ((l_band_khz as f64 / 1000.0).round() as i64) - 100;
            if !(0..=2047).contains(&t) {
                bail!("unicable2 tuning word {t} out of range for frequency {freq_khz} kHz");
            }
            let data1: u32 = ((id0 & 0x1f) << 3) | ((t as u32 >> 8) & 7);
            let data2: u32 = t as u32 & 0xff;
            let data3: u32 = ((position as u32 & 0x3f) << 2) | (pol_bit << 1) | band_bit;
            let mut cmd = vec![0x70u8, data1 as u8, data2 as u8, data3 as u8];
            if let Some(pin) = input.pin {
                if pin > 255 {
                    bail!("unicable2 PIN must be 0-255");
                }
                cmd[0] = 0x71;
                cmd.push(pin as u8);
            }
            Ok(TuneSequence {
                steps: vec![
                    Step::Voltage(SEC_VOLTAGE_18),
                    Step::SleepMs(15),
                    Step::Diseqc(cmd),
                    Step::SleepMs(50),
                    Step::Voltage(SEC_VOLTAGE_13),
                ],
                frequency_khz: input.frequency as u32 * 1000,
            })
        }
    }
}
