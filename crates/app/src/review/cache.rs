//! Byte-bounded LRU of parsed per-file patches.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use prcore::diff::Row;

use super::diffview::PatchLayout;

/// Total budget for cached patches (rows and their layouts).
pub const PATCH_CACHE_BYTES: usize = 256 * 1024 * 1024;

/// Accounted size of one row beyond its text: the struct itself.
const ROW_OVERHEAD: usize = std::mem::size_of::<Row>();

/// Accounted size of a parsed patch: row text plus per-row overhead.
pub fn rows_bytes(rows: &[Row]) -> usize {
    rows.iter().map(|r| r.text.len() + ROW_OVERHEAD).sum()
}

struct Entry {
    rows: Arc<Vec<Row>>,
    layout: Arc<PatchLayout>,
    size: usize,
    tick: u64,
}

pub struct PatchCache {
    cap: usize,
    bytes: usize,
    tick: u64,
    map: HashMap<String, Entry>,
    /// tick -> path, oldest first
    order: BTreeMap<u64, String>,
}

impl PatchCache {
    pub fn new(cap_bytes: usize) -> PatchCache {
        PatchCache {
            cap: cap_bytes,
            bytes: 0,
            tick: 0,
            map: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Returns the rows and their layout and marks the entry most recently used.
    pub fn get(&mut self, path: &str) -> Option<(Arc<Vec<Row>>, Arc<PatchLayout>)> {
        let tick = self.next_tick();
        let e = self.map.get_mut(path)?;
        let path = self
            .order
            .remove(&e.tick)
            .expect("order tracks every entry");
        e.tick = tick;
        self.order.insert(tick, path);
        Some((e.rows.clone(), e.layout.clone()))
    }

    fn remove(&mut self, path: &str) {
        if let Some(e) = self.map.remove(path) {
            self.order.remove(&e.tick);
            self.bytes -= e.size;
        }
    }

    /// Presence check that does not change recency.
    pub fn contains(&self, path: &str) -> bool {
        self.map.contains_key(path)
    }

    /// Inserts (or replaces) `path`, then evicts least recently used entries until under
    /// the cap. An entry larger than the whole cap is not kept.
    pub fn insert(&mut self, path: String, rows: Arc<Vec<Row>>, layout: Arc<PatchLayout>) {
        self.remove(&path);
        let size = rows_bytes(&rows) + layout.bytes();
        if size > self.cap {
            return;
        }
        while self.bytes + size > self.cap {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            if let Some(e) = self.map.remove(&oldest) {
                self.bytes -= e.size;
            }
        }
        let tick = self.next_tick();
        self.order.insert(tick, path.clone());
        self.map.insert(
            path,
            Entry {
                rows,
                layout,
                size,
                tick,
            },
        );
        self.bytes += size;
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prcore::diff::RowKind;

    fn rows(text_len: usize, n: usize) -> Arc<Vec<Row>> {
        Arc::new(
            (0..n)
                .map(|i| Row {
                    kind: RowKind::Add,
                    old_line: None,
                    new_line: Some(i as u32 + 1),
                    text: "x".repeat(text_len),
                })
                .collect(),
        )
    }

    #[test]
    fn size_is_text_plus_row_overhead() {
        let r = rows(10, 3);
        assert_eq!(rows_bytes(&r), 3 * (10 + ROW_OVERHEAD));
        let mut c = PatchCache::new(1 << 20);
        c.insert("a".into(), r, Default::default());
        assert_eq!(c.bytes(), 3 * (10 + ROW_OVERHEAD));
        c.insert("a".into(), rows(10, 1), Default::default());
        assert_eq!(
            c.bytes(),
            10 + ROW_OVERHEAD,
            "replacing does not double count"
        );
    }

    #[test]
    fn cache_evicts_lru_under_cap() {
        let one = rows_bytes(&rows(100, 10));
        let mut c = PatchCache::new(3 * one);
        c.insert("a".into(), rows(100, 10), Default::default());
        c.insert("b".into(), rows(100, 10), Default::default());
        c.insert("c".into(), rows(100, 10), Default::default());
        assert_eq!(c.bytes(), 3 * one);
        // touch "a" so "b" becomes the least recently used
        assert!(c.get("a").is_some());
        c.insert("d".into(), rows(100, 10), Default::default());
        assert!(c.bytes() <= 3 * one);
        assert!(c.contains("a") && c.contains("c") && c.contains("d"));
        assert!(!c.contains("b"));
        assert!(c.get("b").is_none());
    }

    #[test]
    fn entry_larger_than_cap_is_not_kept() {
        let mut c = PatchCache::new(50);
        c.insert("small".into(), rows(1, 1), Default::default());
        c.insert("huge".into(), rows(1000, 1), Default::default());
        assert!(!c.contains("huge"));
        assert!(c.contains("small"));
        assert!(c.bytes() <= 50);
    }

    #[test]
    fn clear_empties() {
        let mut c = PatchCache::new(PATCH_CACHE_BYTES);
        c.insert("a".into(), rows(5, 5), Default::default());
        c.clear();
        assert_eq!(c.bytes(), 0);
        assert!(!c.contains("a"));
    }
}
