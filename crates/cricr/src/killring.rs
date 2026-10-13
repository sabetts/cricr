//! An Emacs-style kill ring: a bounded stack of killed/copied strings, with
//! appending consecutive kills and cycling on `yank-pop`.

use std::collections::VecDeque;

const MAX: usize = 60;

#[derive(Debug, Default)]
pub struct KillRing {
    ring: VecDeque<String>,
    /// Index of the entry returned by the most recent `yank` / `yank_pop`.
    index: usize,
}

impl KillRing {
    pub fn new() -> Self {
        KillRing::default()
    }

    /// Add TEXT to the ring. When APPEND is set (the previous command was also
    /// a kill/save) the text is appended to the most recent entry.
    pub fn push(&mut self, text: &str, append: bool) {
        if text.is_empty() {
            return;
        }
        if append {
            if let Some(front) = self.ring.front_mut() {
                front.push_str(text);
                self.index = 0;
                return;
            }
        }
        self.ring.push_front(text.to_string());
        if self.ring.len() > MAX {
            self.ring.pop_back();
        }
        self.index = 0;
    }

    /// The most recent entry, without changing the cycling index.
    pub fn current(&self) -> Option<String> {
        self.ring.front().cloned()
    }

    /// The entry that `yank` would insert, and reset cycling to it.
    pub fn yank(&mut self) -> Option<String> {
        self.index = 0;
        self.ring.front().cloned()
    }

    /// The next entry for `yank-pop`, cycling through the ring.
    pub fn yank_pop(&mut self) -> Option<String> {
        if self.ring.is_empty() {
            return None;
        }
        self.index = (self.index + 1) % self.ring.len();
        self.ring.get(self.index).cloned()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.ring.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_yank() {
        let mut ring = KillRing::new();
        ring.push("one", false);
        ring.push("two", false);
        assert_eq!(ring.yank().as_deref(), Some("two"));
        assert_eq!(ring.yank_pop().as_deref(), Some("one"));
        assert_eq!(ring.yank_pop().as_deref(), Some("two"));
    }

    #[test]
    fn append_consecutive() {
        let mut ring = KillRing::new();
        ring.push("foo", false);
        ring.push("bar", true);
        assert_eq!(ring.yank().as_deref(), Some("foobar"));
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn ignores_empty() {
        let mut ring = KillRing::new();
        ring.push("", false);
        assert_eq!(ring.yank(), None);
    }
}
