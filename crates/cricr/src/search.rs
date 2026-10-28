//! Incremental search (isearch) state for `C-s` / `C-r`.

use crate::buffer::Pos;

/// The direction isearch moves in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchDirection {
    #[default]
    Forward,
    Backward,
}

/// Incremental search state. The application consults this and the buffer to
/// move point and highlight matches.
#[derive(Debug, Clone, Default)]
pub struct Search {
    pub active: bool,
    pub query: String,
    pub direction: SearchDirection,
    pub origin: Option<Pos>,
    /// End (exclusive) of the current match, if there is one.
    pub match_end: Option<Pos>,
    pub failed: bool,
    /// The last accepted search string, reused by `C-s C-s` / `C-r C-r`.
    pub last_query: Option<String>,
}

impl Search {
    /// Begin a new search from `origin`.
    pub fn start(&mut self, direction: SearchDirection, origin: Pos) {
        self.active = true;
        self.query.clear();
        self.direction = direction;
        self.origin = Some(origin);
        self.match_end = None;
        self.failed = false;
    }

    /// Abandon the search, returning the position to restore (if any).
    pub fn cancel(&mut self) -> Option<Pos> {
        let origin = self.origin;
        self.reset();
        origin
    }

    pub fn reset(&mut self) {
        self.active = false;
        self.query.clear();
        self.failed = false;
        self.origin = None;
        self.match_end = None;
    }

    /// Keep the current position and end the search.
    pub fn accept(&mut self) {
        if !self.query.is_empty() {
            self.last_query = Some(self.query.clone());
        }
        self.active = false;
        self.query.clear();
        self.failed = false;
        self.origin = None;
        self.match_end = None;
    }
}
