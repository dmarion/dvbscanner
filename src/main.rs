mod cli;
mod config;
mod debug;
mod frontend;
mod input;
mod ioctl;
mod monitor;
mod nit;
mod sdt;
mod si;
mod transponder;
mod tui;

use std::os::unix::io::AsRawFd;

use anyhow::{Context, Result};
use clap::Parser;

use frontend::Frontend;
use transponder::TransponderConfig;

const TRACE_LOG_PATH: &str = "dvbscanner-trace.log";

/// Points the process's own stderr at a log file instead of the terminal.
/// Every existing `eprintln!`-based trace call site (frontend.rs, nit.rs)
/// needs no changes - they all still just write to fd 2, which now happens
/// to be the file. Needed because the live TUI owns the terminal via an
/// alternate screen; trace output going to the real stderr would corrupt
/// the display instead of being readable.
fn redirect_trace_to_file() -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(TRACE_LOG_PATH)
        .with_context(|| format!("opening {TRACE_LOG_PATH}"))?;
    if unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
        return Err(std::io::Error::last_os_error()).context("redirecting trace output to file");
    }
    eprintln!("--- dvbscanner trace started, pid {} ---", std::process::id());
    eprintln!("(tail -f {TRACE_LOG_PATH} from another terminal to watch live)");
    Ok(())
}

/// Drops transponders whose delivery system this frontend doesn't support
/// at all, printing a warning per skipped entry - no point tuning to
/// something that can never lock.
fn filter_supported(fe: &Frontend, cfgs: Vec<TransponderConfig>) -> Vec<TransponderConfig> {
    let supported = match fe.supported_delivery_systems() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: could not query supported delivery systems ({e:#}), not filtering");
            return cfgs;
        }
    };
    cfgs.into_iter()
        .filter(|cfg| match cfg.resolve() {
            Ok(tp) if !supported.contains(&tp.system.as_dtv()) => {
                eprintln!(
                    "warning: skipping {} transponder at {} MHz: {} not supported by this frontend",
                    cfg.system, cfg.frequency, cfg.system
                );
                false
            }
            _ => true, // unsupported-system check passed, or resolve() failed for
                       // an unrelated reason that the normal error path handles later
        })
        .collect()
}

fn main() -> Result<()> {
    let args = cli::Args::parse();
    let cfg = config::Config::load(&args.config)?;
    let fe = Frontend::open(args.adapter, args.frontend, args.debug || args.trace)?;
    let transponders = filter_supported(&fe, cfg.transponders);
    if args.debug {
        debug::run_once(fe, args.adapter, args.frontend, args.trace, cfg.input, transponders)
    } else {
        if args.trace {
            redirect_trace_to_file()?;
        }
        tui::run(fe, args.adapter, args.frontend, args.trace, cfg.input, transponders)
    }
}
