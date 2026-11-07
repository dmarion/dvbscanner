//! Link margin: measured CNR minus the CNR the signal's mode needs for
//! quasi-error-free reception. Every threshold here is copied from the spec
//! cited on its table. A mode those tables don't list has no margin, and the
//! caller shows `-`.

use crate::ioctl::*;
use crate::transponder::System;

/// Satellite CNR readings at or above this are treated as not real. The
/// ddbridge frontend sometimes reports exactly 30.00 dB, alongside no BER and
/// no NIT, on muxes that read 9-12 dB on other scans.
const SATELLITE_CNR_CEILING_MDB: i64 = 30_000;

/// DVB-S2: ETSI EN 302 307-1 V1.4.1, table 13. Ideal Es/No in dB at PER 1e-7
/// on AWGN, 64800-bit FECFRAME, perfect synchronization, no phase noise. The
/// spec adds 0.2-0.3 dB for short FECFRAMEs, which we can't see from here.
fn dvbs2_required(modulation: u32, fec: u32) -> Option<f64> {
    Some(match (modulation, fec) {
        (QPSK, FEC_1_4) => -2.35,
        (QPSK, FEC_1_3) => -1.24,
        (QPSK, FEC_2_5) => -0.30,
        (QPSK, FEC_1_2) => 1.00,
        (QPSK, FEC_3_5) => 2.23,
        (QPSK, FEC_2_3) => 3.10,
        (QPSK, FEC_3_4) => 4.03,
        (QPSK, FEC_4_5) => 4.68,
        (QPSK, FEC_5_6) => 5.18,
        (QPSK, FEC_8_9) => 6.20,
        (QPSK, FEC_9_10) => 6.42,
        (PSK_8, FEC_3_5) => 5.50,
        (PSK_8, FEC_2_3) => 6.62,
        (PSK_8, FEC_3_4) => 7.91,
        (PSK_8, FEC_5_6) => 9.35,
        (PSK_8, FEC_8_9) => 10.69,
        (PSK_8, FEC_9_10) => 10.98,
        (APSK_16, FEC_2_3) => 8.97,
        (APSK_16, FEC_3_4) => 10.21,
        (APSK_16, FEC_4_5) => 11.03,
        (APSK_16, FEC_5_6) => 11.61,
        (APSK_16, FEC_8_9) => 12.89,
        (APSK_16, FEC_9_10) => 13.13,
        (APSK_32, FEC_3_4) => 12.73,
        (APSK_32, FEC_4_5) => 13.64,
        (APSK_32, FEC_5_6) => 14.28,
        (APSK_32, FEC_8_9) => 15.69,
        (APSK_32, FEC_9_10) => 16.05,
        _ => return None,
    })
}

/// DVB-S: ETSI EN 300 421 V1.1.2, table 3 gives Eb/No for QEF after
/// Reed-Solomon, including a 0.8 dB modem implementation margin. Its note 1
/// defines Eb against the useful bit rate before RS coding, so converting to
/// Es/No uses 2 QPSK bits per symbol, the code rate, and RS 188/204.
fn dvbs_required(modulation: u32, fec: u32) -> Option<f64> {
    if modulation != QPSK {
        return None;
    }
    let (eb_n0, rate) = match fec {
        FEC_1_2 => (4.5, 1.0 / 2.0),
        FEC_2_3 => (5.0, 2.0 / 3.0),
        FEC_3_4 => (5.5, 3.0 / 4.0),
        FEC_5_6 => (6.0, 5.0 / 6.0),
        FEC_7_8 => (6.4, 7.0 / 8.0),
        _ => return None,
    };
    Some(eb_n0 + 10.0 * f64::log10(2.0 * rate * 188.0 / 204.0))
}

/// DVB-T2: ETSI TS 102 831 V1.2.1, table 44, Gaussian (AWGN) column. Raw
/// (C/N)0 for BER 1e-7 after LDPC, 64800-bit blocks, ideal channel estimation.
/// Clause 14.5 adds a real channel estimation penalty that depends on the
/// receiver's interpolator, which we can't know, so it isn't included.
fn dvbt2_raw_required(modulation: u32, fec: u32) -> Option<f64> {
    Some(match (modulation, fec) {
        (QPSK, FEC_1_2) => 1.0,
        (QPSK, FEC_3_5) => 2.3,
        (QPSK, FEC_2_3) => 3.1,
        (QPSK, FEC_3_4) => 4.1,
        (QPSK, FEC_4_5) => 4.7,
        (QPSK, FEC_5_6) => 5.2,
        (QAM_16, FEC_1_2) => 6.0,
        (QAM_16, FEC_3_5) => 7.6,
        (QAM_16, FEC_2_3) => 8.9,
        (QAM_16, FEC_3_4) => 10.0,
        (QAM_16, FEC_4_5) => 10.8,
        (QAM_16, FEC_5_6) => 11.4,
        (QAM_64, FEC_1_2) => 9.9,
        (QAM_64, FEC_3_5) => 12.0,
        (QAM_64, FEC_2_3) => 13.5,
        (QAM_64, FEC_3_4) => 15.1,
        (QAM_64, FEC_4_5) => 16.1,
        (QAM_64, FEC_5_6) => 16.8,
        (QAM_256, FEC_1_2) => 13.2,
        (QAM_256, FEC_3_5) => 16.1,
        (QAM_256, FEC_2_3) => 17.8,
        (QAM_256, FEC_3_4) => 20.0,
        (QAM_256, FEC_4_5) => 21.3,
        (QAM_256, FEC_5_6) => 22.0,
        _ => return None,
    })
}

/// Largest pilot boosting correction in TS 102 831 V1.2.1 table 46. The real
/// value depends on the pilot pattern, which the Linux DVB API doesn't expose,
/// so assume the worst case rather than understate the requirement.
const DVBT2_MAX_PILOT_CORRECTION_DB: f64 = 0.53;

/// Required CNR in dB. DVB-T has no table yet, so it's always None.
fn required_cnr_db(system: System, modulation: u32, fec: u32) -> Option<f64> {
    match system {
        System::Dvbs => dvbs_required(modulation, fec),
        System::Dvbs2 => dvbs2_required(modulation, fec),
        System::Dvbt2 => dvbt2_raw_required(modulation, fec).map(|raw| raw + DVBT2_MAX_PILOT_CORRECTION_DB),
        System::Dvbt => None,
    }
}

/// Margin in dB, from what the demodulator reported. None unless the mode is
/// known and listed and the CNR is a believable reading in dB.
pub fn margin_db(system: System, modulation: Option<u32>, fec: Option<u32>, cnr: Stat) -> Option<f64> {
    let Stat::Decibel(cnr_mdb) = cnr else {
        return None;
    };
    if system.is_satellite() && cnr_mdb >= SATELLITE_CNR_CEILING_MDB {
        return None;
    }
    let required = required_cnr_db(system, modulation?, fec?)?;
    Some(cnr_mdb as f64 / 1000.0 - required)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn margin(system: System, modulation: u32, fec: u32, cnr_mdb: i64) -> Option<f64> {
        margin_db(system, Some(modulation), Some(fec), Stat::Decibel(cnr_mdb))
    }

    fn assert_close(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("expected a margin");
        assert!((actual - expected).abs() < 0.005, "{actual} != {expected}");
    }

    #[test]
    fn dvbs2_uses_table_13() {
        assert_close(margin(System::Dvbs2, PSK_8, FEC_5_6, 9_700), 0.35);
        assert_close(margin(System::Dvbs2, QPSK, FEC_2_3, 8_520), 5.42);
    }

    #[test]
    fn dvbs_converts_eb_n0_to_es_n0() {
        // 6.0 dB Eb/No + 10log10(2 * 5/6 * 188/204) = 7.86 dB Es/No.
        assert_close(margin(System::Dvbs, QPSK, FEC_5_6, 14_460), 6.60);
    }

    #[test]
    fn dvbt2_uses_table_44_awgn_plus_worst_case_pilot_correction() {
        // 27.24 - (17.8 + 0.53)
        assert_close(margin(System::Dvbt2, QAM_256, FEC_2_3, 27_240), 8.91);
    }

    #[test]
    fn modes_without_a_source_have_no_margin() {
        assert!(margin(System::Dvbs2, PSK_8, FEC_4_5, 12_000).is_none());
        assert!(margin(System::Dvbs, PSK_8, FEC_3_4, 12_000).is_none());
        assert!(margin(System::Dvbt, QAM_64, FEC_2_3, 20_000).is_none());
    }

    #[test]
    fn missing_or_implausible_inputs_have_no_margin() {
        assert!(margin_db(System::Dvbs2, None, Some(FEC_2_3), Stat::Decibel(9_000)).is_none());
        assert!(margin_db(System::Dvbs2, Some(PSK_8), None, Stat::Decibel(9_000)).is_none());
        assert!(margin_db(System::Dvbs2, Some(PSK_8), Some(FEC_2_3), Stat::Relative(100)).is_none());
        assert!(margin(System::Dvbs2, PSK_8, FEC_5_6, 30_000).is_none());
    }

    #[test]
    fn ceiling_only_applies_to_satellite() {
        assert!(margin(System::Dvbt2, QAM_64, FEC_2_3, 31_000).is_some());
    }
}
