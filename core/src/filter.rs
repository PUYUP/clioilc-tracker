use crate::store::TrackedEvent;

/// Criteria for selecting recorded events. Unset fields match everything.
#[derive(Debug, Default, Clone)]
pub struct EventFilter {
    /// Exact reader name.
    pub reader: Option<String>,
    /// Exact project name.
    pub project: Option<String>,
    /// Substring of the file path.
    pub path: Option<String>,
}

impl EventFilter {
    pub fn matches(&self, event: &TrackedEvent) -> bool {
        self.reader.as_deref().is_none_or(|r| event.reader == r)
            && self
                .project
                .as_deref()
                .is_none_or(|p| event.project.as_deref() == Some(p))
            && self
                .path
                .as_deref()
                .is_none_or(|p| event.path.to_string_lossy().contains(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::EventKind;
    use chrono::Utc;
    use std::path::PathBuf;

    fn event(reader: &str, project: Option<&str>, path: &str) -> TrackedEvent {
        TrackedEvent {
            timestamp: Utc::now(),
            kind: EventKind::Modified,
            path: PathBuf::from(path),
            reader: reader.into(),
            project: project.map(Into::into),
            size: None,
        }
    }

    #[test]
    fn empty_filter_matches_everything() {
        assert!(EventFilter::default().matches(&event("claude", None, "/x.json")));
    }

    #[test]
    fn all_criteria_must_match() {
        let filter = EventFilter {
            reader: Some("claude".into()),
            project: Some("p1".into()),
            path: Some("sess".into()),
        };
        assert!(filter.matches(&event("claude", Some("p1"), "/a/sess.jsonl")));
        assert!(!filter.matches(&event("gemini", Some("p1"), "/a/sess.jsonl")));
        assert!(!filter.matches(&event("claude", Some("p2"), "/a/sess.jsonl")));
        assert!(!filter.matches(&event("claude", None, "/a/sess.jsonl")));
        assert!(!filter.matches(&event("claude", Some("p1"), "/a/other.jsonl")));
    }
}
