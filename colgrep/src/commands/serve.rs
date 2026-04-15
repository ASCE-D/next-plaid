use std::path::PathBuf;
use anyhow::Result;

pub struct ServeConfig {
    pub port: u16,
    pub host: String,
    pub index: PathBuf,
    pub model: Option<String>,
    pub sessions: usize,
    pub timeout: u64,
    pub max_concurrent: usize,
    pub alpha: f32,
    pub no_hybrid_search: bool,
    pub no_prewarm: bool,
    pub quantized: bool,
    pub force_cpu: bool,
}

pub fn cmd_serve(_config: ServeConfig) -> Result<()> {
    eprintln!("colgrep serve is not yet implemented");
    std::process::exit(1);
}
