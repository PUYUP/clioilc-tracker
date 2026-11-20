use crate::usage::Record;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// A reader plugin knows where its tool stores files and which files to track.
pub trait Reader {
    /// Short identifier used on the CLI, e.g. "claude".
    fn name(&self) -> &'static str;

    /// Default directory to watch recursively.
    fn default_watch_dir(&self) -> Result<PathBuf>;

    /// File extensions this reader tracks (matched case-insensitively).
    fn extensions(&self) -> &'static [&'static str];

    /// Extract a project/group name for a tracked file, if applicable.
    ///
    /// `relative` is the file's path relative to the watched directory.
    fn project_name(&self, _relative: &Path) -> Option<String> {
        None
    }

    /// Extract usage facts from one complete log line (without its newline).
    /// Readers without a parser return nothing. Must be cheap for lines that
    /// carry no usage data: most lines in a log are not interesting.
    fn parse_line(&self, _line: &[u8]) -> Vec<Record> {
        Vec::new()
    }

    fn is_tracked(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| {
                self.extensions()
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(ext))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Json;

    impl Reader for Json {
        fn name(&self) -> &'static str {
            "json"
        }
        fn default_watch_dir(&self) -> Result<PathBuf> {
            Ok(PathBuf::from("/tmp"))
        }
        fn extensions(&self) -> &'static [&'static str] {
            &["json", "jsonl"]
        }
    }

    #[test]
    fn tracks_matching_extensions_case_insensitively() {
        assert!(Json.is_tracked(Path::new("a/b.json")));
        assert!(Json.is_tracked(Path::new("a/b.JSONL")));
        assert!(!Json.is_tracked(Path::new("a/b.txt")));
        assert!(!Json.is_tracked(Path::new("a/json")));
    }
}
