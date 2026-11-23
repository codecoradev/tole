//! A small cache of parsed answers, so a repaint does not parse them again.
//!
//! The window draws the rows near the screen every frame, and an answer is
//! Markdown that [`crate::markdown::parse`] reads in full; a long answer under a
//! scroll would be parsed sixty times a second. The key is the text itself
//! (a row's text does not change while it is not being streamed, and a streamed
//! one changes the key, so it is parsed again, as it must be), and the cache
//! holds the few most recently drawn answers, never more.

use std::sync::Arc;

use crate::markdown::{parse, Block};

/// Answers kept: a screen shows a handful.
pub const CAPACITY: usize = 16;

#[derive(Debug, Default)]
pub struct Cache {
    /// Most recently used first.
    entries: Vec<(String, Arc<Vec<Block>>)>,
}

impl Cache {
    /// The blocks of `source`, parsed now only when they are not held.
    pub fn blocks(&mut self, source: &str) -> Arc<Vec<Block>> {
        if let Some(at) = self.entries.iter().position(|(key, _)| key == source) {
            let hit = self.entries.remove(at);
            let blocks = Arc::clone(&hit.1);
            self.entries.insert(0, hit);
            return blocks;
        }
        let blocks = Arc::new(parse(source));
        self.entries
            .insert(0, (source.to_owned(), Arc::clone(&blocks)));
        self.entries.truncate(CAPACITY);
        blocks
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_text_is_parsed_once_and_a_changed_text_is_parsed_again() {
        let mut cache = Cache::default();
        assert!(cache.is_empty());
        let first = cache.blocks("a **b**");
        assert!(Arc::ptr_eq(&first, &cache.blocks("a **b**")), "a hit");
        let grown = cache.blocks("a **b** and more");
        assert!(
            !Arc::ptr_eq(&first, &grown),
            "a streamed answer is parsed again"
        );
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn the_cache_holds_the_most_recent_answers_and_never_more() {
        let mut cache = Cache::default();
        let kept = cache.blocks("kept");
        for n in 0..(CAPACITY * 3) {
            cache.blocks(&format!("answer {n}"));
            // The one that is drawn every frame stays.
            assert!(Arc::ptr_eq(&kept, &cache.blocks("kept")));
        }
        assert_eq!(cache.len(), CAPACITY);
        assert!(
            !Arc::ptr_eq(&kept, &cache.blocks("answer 0")),
            "an old one is gone"
        );
    }
}
