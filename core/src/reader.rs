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

    /// Tools that keep their history in a database instead of append-only
    /// logs (e.g. Devin's SQLite file): how to read it. `None` for log readers.
    fn database(&self) -> Option<&dyn DatabaseSource> {
        None
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

/// A tool history kept in a database: read the rows added after a cursor.
pub trait DatabaseSource {
    /// The database file, e.g. `~/.local/share/devin/cli/sessions.db`.
    fn path(&self) -> Result<PathBuf>;

    /// Records from rows after `cursor` (0 = from the start), each with its
    /// project name, oldest first; returns the new cursor. Must open the
    /// database read-only: it belongs to the tool.
    fn read_since(
        &self,
        path: &Path,
        cursor: i64,
        out: &mut dyn FnMut(Option<&str>, Record),
    ) -> Result<i64>;
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
