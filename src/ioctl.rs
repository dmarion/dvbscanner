//! Raw bindings for the Linux DVB frontend API (linux/dvb/frontend.h),
//! using the modern S2API (DTV_PROPERTY) plus the handful of legacy
//! ioctls that are still the correct way to do things (voltage, tone,
//! DiSEqC, lock status).

#![allow(dead_code)]

use std::io;
use std::mem::size_of;
use std::os::unix::io::RawFd;

const fn ioc(dir: u32, nr: u32, size: u32) -> u32 {
    const IOC_NRBITS: u32 = 8;
    const IOC_TYPEBITS: u32 = 8;
    const TYPE_O: u32 = b'o' as u32;
    (dir << 30) | (TYPE_O << IOC_NRBITS) | nr | (size << (IOC_NRBITS + IOC_TYPEBITS))
}

const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

// ---- DTV property commands ----
pub const DTV_TUNE: u32 = 1;
pub const DTV_CLEAR: u32 = 2;
pub const DTV_FREQUENCY: u32 = 3;
pub const DTV_MODULATION: u32 = 4;
pub const DTV_BANDWIDTH_HZ: u32 = 5;
pub const DTV_INVERSION: u32 = 6;
pub const DTV_SYMBOL_RATE: u32 = 8;
pub const DTV_INNER_FEC: u32 = 9;
pub const DTV_PILOT: u32 = 12;
pub const DTV_ROLLOFF: u32 = 13;
pub const DTV_DELIVERY_SYSTEM: u32 = 17;
pub const DTV_STREAM_ID: u32 = 42;
pub const DTV_ENUM_DELSYS: u32 = 44;
pub const DTV_STAT_SIGNAL_STRENGTH: u32 = 62;
pub const DTV_STAT_CNR: u32 = 63;
/// Bit error count/total *before* FEC correction - the gap between this
/// and the POST_* pair below is how many bit errors FEC actually fixed.
pub const DTV_STAT_PRE_ERROR_BIT_COUNT: u32 = 64;
pub const DTV_STAT_PRE_TOTAL_BIT_COUNT: u32 = 65;
pub const DTV_STAT_POST_ERROR_BIT_COUNT: u32 = 66;
pub const DTV_STAT_POST_TOTAL_BIT_COUNT: u32 = 67;
pub const DTV_STAT_ERROR_BLOCK_COUNT: u32 = 68;
pub const DTV_STAT_TOTAL_BLOCK_COUNT: u32 = 69;

pub const NO_STREAM_ID_FILTER: u32 = 0xFFFF_FFFF;

// ---- fe_delivery_system ----
pub const SYS_DVBT: u32 = 3;
pub const SYS_DVBS: u32 = 5;
pub const SYS_DVBS2: u32 = 6;
pub const SYS_DVBT2: u32 = 16;

// ---- fe_modulation ----
pub const QPSK: u32 = 0;
pub const QAM_16: u32 = 1;
pub const QAM_32: u32 = 2;
pub const QAM_64: u32 = 3;
pub const QAM_128: u32 = 4;
pub const QAM_256: u32 = 5;
pub const QAM_AUTO: u32 = 6;
pub const PSK_8: u32 = 9;
pub const APSK_16: u32 = 10;
pub const APSK_32: u32 = 11;

// ---- fe_code_rate ----
pub const FEC_NONE: u32 = 0;
pub const FEC_1_2: u32 = 1;
pub const FEC_2_3: u32 = 2;
pub const FEC_3_4: u32 = 3;
pub const FEC_4_5: u32 = 4;
pub const FEC_5_6: u32 = 5;
pub const FEC_6_7: u32 = 6;
pub const FEC_7_8: u32 = 7;
pub const FEC_8_9: u32 = 8;
pub const FEC_AUTO: u32 = 9;
pub const FEC_3_5: u32 = 10;
pub const FEC_9_10: u32 = 11;
pub const FEC_2_5: u32 = 12;

// ---- fe_pilot / fe_rolloff ----
pub const PILOT_AUTO: u32 = 2;
pub const ROLLOFF_35: u32 = 0;
pub const ROLLOFF_20: u32 = 1;
pub const ROLLOFF_25: u32 = 2;
pub const ROLLOFF_AUTO: u32 = 3;

// ---- fe_spectral_inversion ----
pub const INVERSION_AUTO: u32 = 2;

// ---- fe_sec_voltage ----
pub const SEC_VOLTAGE_13: i32 = 0;
pub const SEC_VOLTAGE_18: i32 = 1;
pub const SEC_VOLTAGE_OFF: i32 = 2;

// ---- fe_sec_tone_mode ----
pub const SEC_TONE_ON: i32 = 0;
pub const SEC_TONE_OFF: i32 = 1;

// ---- fe_sec_mini_cmd ----
pub const SEC_MINI_A: i32 = 0;
pub const SEC_MINI_B: i32 = 1;

// ---- fe_status ----
pub const FE_HAS_SIGNAL: u32 = 0x01;
pub const FE_HAS_CARRIER: u32 = 0x02;
pub const FE_HAS_VITERBI: u32 = 0x04;
pub const FE_HAS_SYNC: u32 = 0x08;
pub const FE_HAS_LOCK: u32 = 0x10;
pub const FE_TIMEDOUT: u32 = 0x20;
pub const FE_REINIT: u32 = 0x40;

// ---- fecap_scale_params ----
pub const FE_SCALE_NOT_AVAILABLE: u8 = 0;
pub const FE_SCALE_DECIBEL: u8 = 1;
pub const FE_SCALE_RELATIVE: u8 = 2;
pub const FE_SCALE_COUNTER: u8 = 3;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DvbDiseqcMasterCmd {
    pub msg: [u8; 6],
    pub msg_len: u8,
}

// struct dtv_stats is `__attribute__((packed))`: 1 byte scale + 8 byte union.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct DtvStatsRaw {
    scale: u8,
    value: u64,
}

// struct dtv_fe_stats is also packed: 1 byte len + 4 * dtv_stats.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct DtvFeStatsRaw {
    len: u8,
    stat: [DtvStatsRaw; 4],
}

// struct { __u8 data[32]; __u32 len; __u32 reserved1[3]; void *reserved2; }
// NOT packed - natural alignment (the pointer forces 8-byte alignment on 64-bit).
#[repr(C)]
#[derive(Clone, Copy)]
struct DtvBufferRaw {
    data: [u8; 32],
    len: u32,
    reserved1: [u32; 3],
    reserved2: *mut core::ffi::c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
union DtvPropertyUnion {
    data: u32,
    st: DtvFeStatsRaw,
    buffer: DtvBufferRaw,
}

// Mirrors struct dtv_property exactly. The kernel header marks the whole
// struct `__attribute__((packed))` (verified via a test program against the
// system header: sizeof == 76 on 64-bit Linux, not the 80 you'd get from
// natural alignment) - this is essential to get right, since a wrong stride
// here silently corrupts every property past the first one in a multi-prop
// FE_SET_PROPERTY/FE_GET_PROPERTY call.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct DtvProperty {
    cmd: u32,
    reserved: [u32; 3],
    u: DtvPropertyUnion,
    result: i32,
}

impl DtvProperty {
    pub fn data(cmd: u32, data: u32) -> Self {
        DtvProperty {
            cmd,
            reserved: [0; 3],
            u: DtvPropertyUnion { data },
            result: 0,
        }
    }
}

#[repr(C)]
struct DtvProperties {
    num: u32,
    props: *mut DtvProperty,
}

/// One statistic value read back from the frontend (DTV_STAT_*).
#[derive(Clone, Copy, Debug)]
pub enum Stat {
    Unavailable,
    Decibel(i64),  // 0.001 dB units
    Relative(u32), // 0..0xffff ~ 0..100%
    Counter(u64),
}

fn read_stat(prop: &DtvProperty, label: &str, trace: bool) -> Stat {
    // SAFETY: cmd was one of the DTV_STAT_* reads, so `.st` is the active union member.
    let st = unsafe { prop.u.st };
    if trace {
        // Dumps every entry the driver returned, not just the stat[0] we
        // actually use below - some drivers report more than one (e.g. the
        // same quantity in two scales), and seeing all of them is the only
        // way to tell a genuinely odd driver/chip reading apart from a bug
        // in how we're picking which entry to trust.
        let entries: Vec<String> = (0..st.len.min(4) as usize)
            .map(|i| {
                let s = st.stat[i];
                let (scale, value) = (s.scale, s.value); // copy out: format!() takes packed fields by reference otherwise
                format!("scale={scale} value={value}")
            })
            .collect();
        eprintln!("fe: stat {label}: len={} [{}]", st.len, entries.join(", "));
    }
    if st.len == 0 {
        return Stat::Unavailable;
    }
    let s = st.stat[0];
    match s.scale {
        FE_SCALE_DECIBEL => Stat::Decibel(s.value as i64),
        FE_SCALE_RELATIVE => Stat::Relative(s.value as u32),
        FE_SCALE_COUNTER => Stat::Counter(s.value),
        _ => Stat::Unavailable,
    }
}

pub const FE_DISEQC_SEND_MASTER_CMD: u32 =
    ioc(IOC_WRITE, 63, size_of::<DvbDiseqcMasterCmd>() as u32);
pub const FE_DISEQC_SEND_BURST: u32 = ioc(IOC_NONE, 65, 0);
pub const FE_SET_TONE: u32 = ioc(IOC_NONE, 66, 0);
pub const FE_SET_VOLTAGE: u32 = ioc(IOC_NONE, 67, 0);
pub const FE_READ_STATUS: u32 = ioc(IOC_READ, 69, 4);
pub const FE_SET_PROPERTY: u32 = ioc(IOC_WRITE, 82, size_of::<DtvProperties>() as u32);
pub const FE_GET_PROPERTY: u32 = ioc(IOC_READ, 83, size_of::<DtvProperties>() as u32);

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn set_voltage(fd: RawFd, volt: i32) -> io::Result<()> {
    check(unsafe { libc::ioctl(fd, FE_SET_VOLTAGE as _, volt) })
}

pub fn set_tone(fd: RawFd, tone: i32) -> io::Result<()> {
    check(unsafe { libc::ioctl(fd, FE_SET_TONE as _, tone) })
}

pub fn send_burst(fd: RawFd, burst: i32) -> io::Result<()> {
    check(unsafe { libc::ioctl(fd, FE_DISEQC_SEND_BURST as _, burst) })
}

pub fn diseqc_send(fd: RawFd, bytes: &[u8]) -> io::Result<()> {
    assert!(bytes.len() <= 6);
    let mut cmd = DvbDiseqcMasterCmd::default();
    cmd.msg[..bytes.len()].copy_from_slice(bytes);
    cmd.msg_len = bytes.len() as u8;
    check(unsafe { libc::ioctl(fd, FE_DISEQC_SEND_MASTER_CMD as _, &cmd) })
}

/// Every fe_status bit paired with its name and whether *set* is the good
/// state for it, in acquisition order - shared by `format_status` (trace
/// logging) and the Monitor popup (which colors each name individually
/// rather than just joining the set ones). The first five are acquisition
/// stages, each a strict superset of the last on a healthy lock (SIGNAL
/// through LOCK) - set is good. TIMEDOUT/REINIT are the opposite: alarm
/// bits that are normally clear, so set is bad for those two.
pub const STATUS_FLAGS: &[(u32, &str, bool)] = &[
    (FE_HAS_SIGNAL, "SIGNAL", true),
    (FE_HAS_CARRIER, "CARRIER", true),
    (FE_HAS_VITERBI, "VITERBI", true),
    (FE_HAS_SYNC, "SYNC", true),
    (FE_HAS_LOCK, "LOCK", true),
    (FE_TIMEDOUT, "TIMEDOUT", false),
    (FE_REINIT, "REINIT", false),
];

/// Renders every fe_status bit by name (set bits only) - e.g.
/// "SIGNAL|CARRIER|VITERBI|SYNC|LOCK" for a clean lock, or just "SIGNAL" for
/// a carrier that's present but never made it through demodulation.
pub fn format_status(status: u32) -> String {
    let names: Vec<&str> = STATUS_FLAGS.iter().filter(|(bit, ..)| status & bit != 0).map(|(_, name, _)| *name).collect();
    if names.is_empty() {
        format!("0x{status:02x} (none)")
    } else {
        format!("0x{status:02x} ({})", names.join("|"))
    }
}

pub fn read_status(fd: RawFd, trace: bool) -> io::Result<u32> {
    let mut status: u32 = 0;
    check(unsafe { libc::ioctl(fd, FE_READ_STATUS as _, &mut status) })?;
    if trace {
        eprintln!("fe: status {}", format_status(status));
    }
    Ok(status)
}

pub fn set_properties(fd: RawFd, props: &mut [DtvProperty]) -> io::Result<()> {
    let mut p = DtvProperties {
        num: props.len() as u32,
        props: props.as_mut_ptr(),
    };
    check(unsafe { libc::ioctl(fd, FE_SET_PROPERTY as _, &mut p) })
}

/// Reads back a single scalar DTV property (one that uses the `data`
/// union member, e.g. DTV_MODULATION) via FE_GET_PROPERTY.
pub fn get_property_data(fd: RawFd, cmd: u32) -> io::Result<u32> {
    let mut props = [DtvProperty::data(cmd, 0)];
    let mut p = DtvProperties {
        num: props.len() as u32,
        props: props.as_mut_ptr(),
    };
    check(unsafe { libc::ioctl(fd, FE_GET_PROPERTY as _, &mut p) })?;
    // SAFETY: cmd was a scalar property, so `.data` is the active union member.
    Ok(unsafe { props[0].u.data })
}

/// Returns the list of fe_delivery_system (SYS_*) values this frontend
/// actually supports. DTV_ENUM_DELSYS is the one property that returns its
/// result through the `buffer` union member rather than `data`.
pub fn enum_delivery_systems(fd: RawFd) -> io::Result<Vec<u32>> {
    let mut props = [DtvProperty::data(DTV_ENUM_DELSYS, 0)];
    let mut p = DtvProperties {
        num: props.len() as u32,
        props: props.as_mut_ptr(),
    };
    check(unsafe { libc::ioctl(fd, FE_GET_PROPERTY as _, &mut p) })?;
    // SAFETY: cmd was DTV_ENUM_DELSYS, so `.buffer` is the active union member.
    let buf = unsafe { props[0].u.buffer };
    let len = (buf.len as usize).min(buf.data.len());
    Ok(buf.data[..len].iter().map(|&b| b as u32).collect())
}

/// Reads back the standard set of quality statistics in a single ioctl call.
pub struct FeStats {
    pub signal_strength: Stat,
    pub cnr: Stat,
    pub pre_error_bits: Stat,
    pub pre_total_bits: Stat,
    pub post_error_bits: Stat,
    pub post_total_bits: Stat,
    pub error_blocks: Stat,
    pub total_blocks: Stat,
}

pub fn read_stats(fd: RawFd, trace: bool) -> io::Result<FeStats> {
    let mut props = [
        DtvProperty::data(DTV_STAT_SIGNAL_STRENGTH, 0),
        DtvProperty::data(DTV_STAT_CNR, 0),
        DtvProperty::data(DTV_STAT_PRE_ERROR_BIT_COUNT, 0),
        DtvProperty::data(DTV_STAT_PRE_TOTAL_BIT_COUNT, 0),
        DtvProperty::data(DTV_STAT_POST_ERROR_BIT_COUNT, 0),
        DtvProperty::data(DTV_STAT_POST_TOTAL_BIT_COUNT, 0),
        DtvProperty::data(DTV_STAT_ERROR_BLOCK_COUNT, 0),
        DtvProperty::data(DTV_STAT_TOTAL_BLOCK_COUNT, 0),
    ];
    let mut p = DtvProperties {
        num: props.len() as u32,
        props: props.as_mut_ptr(),
    };
    check(unsafe { libc::ioctl(fd, FE_GET_PROPERTY as _, &mut p) })?;
    Ok(FeStats {
        signal_strength: read_stat(&props[0], "signal_strength", trace),
        cnr: read_stat(&props[1], "cnr", trace),
        pre_error_bits: read_stat(&props[2], "pre_error_bits", trace),
        pre_total_bits: read_stat(&props[3], "pre_total_bits", trace),
        post_error_bits: read_stat(&props[4], "post_error_bits", trace),
        post_total_bits: read_stat(&props[5], "post_total_bits", trace),
        error_blocks: read_stat(&props[6], "error_blocks", trace),
        total_blocks: read_stat(&props[7], "total_blocks", trace),
    })
}
