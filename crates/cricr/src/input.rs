//! Input history ring (`M-p` / `M-n`), mirroring `rcirc-insert-prev/next-input`.

const MAX: usize = 1024;

/// A bounded, most-recent-first history of submitted input lines.
#[derive(Debug, Default)]
pub struct History {
    items: Vec<String>,
    index: Option<usize>,
    draft: String,
}

impl History {
    pub fn push(&mut self, line: &str) {
        self.index = None;
        if line.is_empty() {
            return;
        }
        if self.items.last().map(String::as_str) == Some(line) {
            return;
        }
        self.items.push(line.to_string());
        if self.items.len() > MAX {
            self.items.remove(0);
        }
    }

    /// Move to the previous entry, remembering `current` as the draft.
    pub fn prev(&mut self, current: &str) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        let i = match self.index {
            None => {
                self.draft = current.to_string();
                self.items.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.index = Some(i);
        Some(self.items[i].clone())
    }

    /// Move to the next entry, restoring the draft after the newest entry.
    pub fn next(&mut self) -> Option<String> {
        let i = self.index?;
        if i + 1 < self.items.len() {
            self.index = Some(i + 1);
            Some(self.items[i + 1].clone())
        } else {
            self.index = None;
            Some(self.draft.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycles_through_history() {
        let mut h = History::default();
        h.push("one");
        h.push("two");
        assert_eq!(h.prev("draft"), Some("two".into()));
        assert_eq!(h.prev("x"), Some("one".into()));
        assert_eq!(h.prev("x"), Some("one".into()));
        assert_eq!(h.next(), Some("two".into()));
        assert_eq!(h.next(), Some("draft".into()));
        assert_eq!(h.next(), None);
    }

    #[test]
    fn dedupes_consecutive_and_ignores_empty() {
        let mut h = History::default();
        h.push("");
        h.push("a");
        h.push("a");
        assert_eq!(h.prev("x"), Some("a".into()));
        assert_eq!(h.next(), Some("x".into()));
    }
}
