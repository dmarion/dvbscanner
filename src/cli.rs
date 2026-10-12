use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "dvbscanner", version, about = "Simple DVB transponder scan TUI")]
pub struct Args {
    /// DVB adapter number
    #[arg(short, long, default_value_t = 0)]
    pub adapter: u32,

    /// Frontend number
    #[arg(short, long, default_value_t = 0)]
    pub frontend: u32,

    /// Path to TOML config file (input settings + transponder list)
    #[arg(short, long, default_value = "dvbscanner.toml")]
    pub config: PathBuf,

    /// Scan every configured transponder once, printing verbose tuning
    /// details and results, instead of launching the TUI.
    #[arg(short, long)]
    pub debug: bool,

    /// Dump raw low-level protocol data (currently: received NIT section
    /// bytes and descriptor parsing steps; more may be added). With
    /// --debug, goes to stderr normally. Without it (live TUI), stderr
    /// would corrupt the display, so it's redirected to
    /// dvbscanner-trace.log instead - tail -f that file from another
    /// terminal to watch it live.
    #[arg(short, long)]
    pub trace: bool,
}
