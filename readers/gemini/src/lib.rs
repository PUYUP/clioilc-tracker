use anyhow::Result;
use clioilc_core::{home_dir, Reader};
use std::path::PathBuf;

/// Reader for Gemini CLI files in `~/.gemini`.
#[derive(Debug, Default, Clone, Copy)]
pub struct GeminiReader;

impl Reader for GeminiReader {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn default_watch_dir(&self) -> Result<PathBuf> {
        Ok(home_dir()?.join(".gemini"))
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["json", "jsonl"]
    }
}
