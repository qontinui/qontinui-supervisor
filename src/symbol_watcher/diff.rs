//! Per-file last-known-symbol map + diff computation.
//!
//! The state is intentionally non-persistent: on daemon startup the map is
//! empty, so the *first* save of any file produces "added = all symbols
//! currently in the file." That is correct behavior — the first save under
//! a running daemon is the first claim-window for that file.

use super::tree_sitter_extract::Symbol;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Outcome of comparing a file's prior symbol set against the freshly
/// extracted one. Three buckets:
///
/// - `added`: present now, absent before. Acquire a claim.
/// - `modified`: present in both sets by `name`, but `start_line`/`end_line`
///   moved. Re-acquire the claim (refreshes TTL + audit row).
/// - `removed`: absent now, present before. Release the claim.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SymbolDiff {
    pub added: Vec<Symbol>,
    pub modified: Vec<Symbol>,
    pub removed: Vec<Symbol>,
}

impl SymbolDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.removed.is_empty()
    }
}

/// In-memory per-file symbol map. Cheap clone — `Symbol` is small.
#[derive(Default, Debug)]
pub struct SymbolMemory {
    files: HashMap<PathBuf, Vec<Symbol>>,
}

impl SymbolMemory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the stored symbols for `path` to `new_symbols` and return
    /// the diff against the prior state. Caller is responsible for the
    /// downstream claim acquire/release; this function only updates the
    /// memory + computes the delta.
    pub fn update(&mut self, path: &PathBuf, new_symbols: Vec<Symbol>) -> SymbolDiff {
        let prior = self.files.get(path).cloned().unwrap_or_default();
        let diff = compute_diff(&prior, &new_symbols);
        self.files.insert(path.clone(), new_symbols);
        diff
    }

    /// Drop the entry for `path`. Used when a file is deleted; the
    /// resulting diff is "all prior symbols removed." Caller must
    /// release their claims separately — this only mutates the memory.
    pub fn forget(&mut self, path: &PathBuf) -> Vec<Symbol> {
        self.files.remove(path).unwrap_or_default()
    }

    #[cfg(test)]
    pub fn known_files(&self) -> Vec<PathBuf> {
        self.files.keys().cloned().collect()
    }
}

/// Pure diff function — exposed at module level so the unit tests can
/// hit it without going through `SymbolMemory`.
pub fn compute_diff(prior: &[Symbol], current: &[Symbol]) -> SymbolDiff {
    let prior_names: HashSet<&str> = prior.iter().map(|s| s.name.as_str()).collect();
    let current_names: HashSet<&str> = current.iter().map(|s| s.name.as_str()).collect();

    let mut added: Vec<Symbol> = Vec::new();
    let mut modified: Vec<Symbol> = Vec::new();
    for s in current {
        if !prior_names.contains(s.name.as_str()) {
            added.push(s.clone());
            continue;
        }
        // Same name in both — check for movement.
        let prior_match = prior.iter().find(|p| p.name == s.name);
        if let Some(p) = prior_match {
            if p.start_line != s.start_line || p.end_line != s.end_line {
                modified.push(s.clone());
            }
        }
    }

    let mut removed: Vec<Symbol> = Vec::new();
    for s in prior {
        if !current_names.contains(s.name.as_str()) {
            removed.push(s.clone());
        }
    }

    SymbolDiff {
        added,
        modified,
        removed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, start: u32, end: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            start_line: start,
            end_line: end,
        }
    }

    #[test]
    fn diff_empty_when_identical() {
        let prior = vec![sym("foo", 0, 5), sym("bar", 6, 10)];
        let current = prior.clone();
        let d = compute_diff(&prior, &current);
        assert!(d.is_empty(), "got {d:?}");
    }

    #[test]
    fn diff_added() {
        let prior = vec![sym("foo", 0, 5)];
        let current = vec![sym("foo", 0, 5), sym("bar", 6, 10)];
        let d = compute_diff(&prior, &current);
        assert_eq!(d.added, vec![sym("bar", 6, 10)]);
        assert!(d.modified.is_empty());
        assert!(d.removed.is_empty());
    }

    #[test]
    fn diff_removed() {
        let prior = vec![sym("foo", 0, 5), sym("bar", 6, 10)];
        let current = vec![sym("foo", 0, 5)];
        let d = compute_diff(&prior, &current);
        assert_eq!(d.removed, vec![sym("bar", 6, 10)]);
    }

    #[test]
    fn diff_modified_by_start_line() {
        let prior = vec![sym("foo", 0, 5)];
        let current = vec![sym("foo", 2, 7)];
        let d = compute_diff(&prior, &current);
        assert_eq!(d.modified, vec![sym("foo", 2, 7)]);
    }

    #[test]
    fn diff_combined() {
        let prior = vec![sym("a", 0, 5), sym("b", 6, 10), sym("c", 11, 15)];
        let current = vec![sym("a", 0, 5), sym("b", 8, 12), sym("d", 13, 17)];
        let d = compute_diff(&prior, &current);
        assert_eq!(d.added, vec![sym("d", 13, 17)]);
        assert_eq!(d.modified, vec![sym("b", 8, 12)]);
        assert_eq!(d.removed, vec![sym("c", 11, 15)]);
    }

    #[test]
    fn memory_update_returns_diff_against_prior() {
        let mut m = SymbolMemory::new();
        let path = PathBuf::from("a.rs");

        // First update: everything is "added."
        let d1 = m.update(&path, vec![sym("foo", 0, 5)]);
        assert_eq!(d1.added, vec![sym("foo", 0, 5)]);

        // Second update: foo moved.
        let d2 = m.update(&path, vec![sym("foo", 2, 7)]);
        assert_eq!(d2.modified, vec![sym("foo", 2, 7)]);

        // Third update: foo removed, bar added.
        let d3 = m.update(&path, vec![sym("bar", 0, 5)]);
        assert_eq!(d3.added, vec![sym("bar", 0, 5)]);
        assert_eq!(d3.removed, vec![sym("foo", 2, 7)]);
    }

    #[test]
    fn forget_returns_prior_symbols() {
        let mut m = SymbolMemory::new();
        let path = PathBuf::from("a.rs");
        m.update(&path, vec![sym("foo", 0, 5)]);
        let prior = m.forget(&path);
        assert_eq!(prior, vec![sym("foo", 0, 5)]);
        assert!(m.known_files().is_empty());
    }
}
