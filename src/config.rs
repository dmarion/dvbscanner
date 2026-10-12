use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::input::InputConfig;
use crate::transponder::TransponderConfig;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub input: InputConfig,
    #[serde(rename = "transponder", default)]
    pub transponders: Vec<TransponderConfig>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        if cfg.transponders.is_empty() {
            anyhow::bail!("config file has no [[transponder]] entries");
        }
        Ok(cfg)
    }
}
