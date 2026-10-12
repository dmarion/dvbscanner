//! `--debug`: tune every configured transponder once, printing verbose
//! logging, and report a final summary. No TUI.

use std::thread::sleep;
use std::time::Duration;

use anyhow::Result;

use crate::frontend::{self, Frontend, FE_HAS_LOCK};
use crate::input::InputConfig;
use crate::nit;
use crate::transponder::TransponderConfig;

// A NIT can span multiple sections (large networks easily have enough
// transport streams to need 2+), and ETSI TR 101 211 allows broadcasters
// up to ~10s between repeats of a given section - confirmed via --trace
// that 5s reliably catches only one section of a 2-section NIT and misses
// the other (usually section 0, carrying the network name). Comfortably
// above the spec ceiling instead.
const NIT_TIMEOUT: Duration = Duration::from_millis(11000);

const LOCK_TIMEOUT: Duration = Duration::from_millis(8000);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const SETTLE_AFTER_LOCK: Duration = Duration::from_millis(300);

/// Polls status until locked or `LOCK_TIMEOUT` elapses, logging each poll.
/// The kernel frontend driver keeps retrying/adjusting in the background
/// after a tune, so a marginal signal can take a few seconds to actually
/// lock - a single fixed short sleep gives up too early.
fn wait_for_lock(fe: &Frontend) -> (u32, bool) {
    let start = std::time::Instant::now();
    loop {
        let status = fe.status().unwrap_or(0);
        let locked = status & FE_HAS_LOCK != 0;
        if locked {
            eprintln!("  (locked after {:.1}s)", start.elapsed().as_secs_f32());
            sleep(SETTLE_AFTER_LOCK);
            return (status, true);
        }
        if start.elapsed() >= LOCK_TIMEOUT {
            return (status, false);
        }
        sleep(POLL_INTERVAL);
    }
}

pub fn run_once(
    fe: Frontend,
    adapter: u32,
    frontend_num: u32,
    trace: bool,
    input: InputConfig,
    cfgs: Vec<TransponderConfig>,
) -> Result<()> {
    eprintln!("input config: {input:?}");
    eprintln!("scanning {} transponder(s)\n", cfgs.len());

    let mut locked_count = 0;
    let mut results = Vec::with_capacity(cfgs.len());

    for (i, cfg) in cfgs.iter().enumerate() {
        println!(
            "[{}/{}] system={} frequency={} MHz",
            i + 1,
            cfgs.len(),
            cfg.system,
            cfg.frequency
        );

        let tp = match cfg.resolve() {
            Ok(tp) => tp,
            Err(e) => {
                println!("  error: {e:#}\n");
                results.push((cfg.frequency, None));
                continue;
            }
        };

        if let Err(e) = fe.tune(&input, &tp) {
            println!("  error: {e:#}\n");
            results.push((cfg.frequency, None));
            continue;
        }

        let (status, locked) = wait_for_lock(&fe);
        let stats = fe.stats().ok();

        println!(
            "  status=0x{status:02x} lock={}",
            if locked { "YES" } else { "no" }
        );
        if let Some(s) = &stats {
            println!(
                "  signal={} cnr={} ber={} uncorrected_blocks={}",
                frontend::fmt_stat(s.signal),
                frontend::fmt_stat(s.cnr),
                frontend::fmt_ber(s.ber),
                frontend::fmt_blocks(s.error_blocks),
            );
        }
        if locked {
            match nit::read_nit(adapter, frontend_num, NIT_TIMEOUT, trace, None) {
                Some(n) => {
                    println!(
                        "  network=0x{:04x} name={} transport_streams={}",
                        n.network_id,
                        n.network_name.as_deref().unwrap_or("-"),
                        n.transport_streams.len()
                    );
                }
                None => println!("  network=-"),
            }
        }
        println!();

        if locked {
            locked_count += 1;
        }
        results.push((cfg.frequency, Some(locked)));
    }

    println!("--- summary ---");
    for (freq, locked) in &results {
        let s = match locked {
            Some(true) => "LOCK",
            Some(false) => "no lock",
            None => "error",
        };
        println!("  {freq:>6} MHz: {s}");
    }
    println!("{locked_count}/{} transponder(s) locked", cfgs.len());

    Ok(())
}
