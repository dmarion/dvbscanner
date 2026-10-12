use std::path::PathBuf;
use std::{env, fs};

use clap::CommandFactory;
use clap_complete::{
    generate_to,
    shells::{Bash, Zsh},
};
#[path = "src/cli.rs"]
mod cli;

use cli::Args;

fn main() {
    let profile = env::var("PROFILE").expect("PROFILE not set");
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
    let target_dir = env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest_dir.join("target"));

    let completions_dir = target_dir.join(profile).join("completions");
    fs::create_dir_all(&completions_dir).expect("Failed to create completions directory");

    generate_to(Bash, &mut Args::command(), "dvbscanner", &completions_dir)
        .expect("Failed to generate Bash completion");
    generate_to(Zsh, &mut Args::command(), "dvbscanner", &completions_dir)
        .expect("Failed to generate Zsh completion");

    println!("cargo:rerun-if-changed=src/cli.rs");
    println!("cargo:rerun-if-changed=build.rs");
}
