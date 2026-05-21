//! Recursive `notify` wrapper for the symbol watcher.
//!
//! Maintains one notify::RecommendedWatcher per configured root and
//! funnels modify events into a tokio-mpsc channel of `SaveEvent`s.
//! Filtering by extension happens here so the receiver doesn't have to
//! re-check; this also avoids re-extracting symbols on every `.lock`,
//! `target/`, `.git/`, `node_modules/`, etc. write.

use std::path::{Path, PathBuf};

use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// A single file-save event surfaced to the daemon loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveEvent {
    /// Absolute path to the file. Daemon downstream may further
    /// canonicalize to make resource_keys stable across hosts.
    pub path: PathBuf,
    /// File extension (lowercase, no leading dot) — `rs`, `ts`, etc.
    pub extension: String,
}

/// Test-visible filter — returns true if a path should produce a
/// `SaveEvent` given the configured `extensions` list. Excludes common
/// generated/vendored directories so we don't extract symbols from
/// `target/`, `.git/`, `node_modules/`, `.venv/`.
pub fn should_watch(path: &Path, extensions: &[String]) -> Option<String> {
    // Exclude generated dirs.
    for component in path.components() {
        let c = component.as_os_str();
        if c == ".git" || c == "target" || c == "node_modules" || c == ".venv" {
            return None;
        }
        if let Some(s) = c.to_str() {
            // Hidden directories starting with `.` (except `.spawn-origin_main`
            // which is a real working tree the operator may want watched).
            if s.starts_with('.') && s != "." && s != ".." && !s.starts_with(".spawn-") {
                // Allow `.cargo`, `.dev-logs` to be skipped — they're noise.
                if s == ".cargo"
                    || s == ".dev-logs"
                    || s == ".pytest_cache"
                    || s == ".mypy_cache"
                    || s == ".ruff_cache"
                    || s == ".vscode"
                    || s == ".idea"
                {
                    return None;
                }
            }
        }
    }
    let ext = path.extension()?.to_str()?.to_lowercase();
    if extensions.iter().any(|e| e.eq_ignore_ascii_case(&ext)) {
        Some(ext)
    } else {
        None
    }
}

/// Spawn one watcher per `roots` entry, forwarding filtered modify
/// events to the returned mpsc receiver. Returns the receiver plus the
/// `Vec<Watcher>` — the caller MUST keep the `Vec` alive for the
/// duration of the daemon (dropping the watchers stops the OS event
/// hook). On failure (root doesn't exist, can't register a watch) the
/// root is skipped with a warning; other roots continue.
pub fn spawn_watchers(
    roots: &[PathBuf],
    extensions: Vec<String>,
) -> (mpsc::Receiver<SaveEvent>, Vec<RecommendedWatcher>) {
    let (tx, rx) = mpsc::channel::<SaveEvent>(1024);
    let mut watchers = Vec::new();

    for root in roots {
        if !root.exists() {
            warn!(
                "symbol_watcher: configured watch_dir {} does not exist — skipping",
                root.display()
            );
            continue;
        }
        let tx_clone = tx.clone();
        let extensions = extensions.clone();
        let root_clone = root.clone();
        let watcher_result =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let evt = match res {
                    Ok(e) => e,
                    Err(e) => {
                        warn!("symbol_watcher: notify error: {e}");
                        return;
                    }
                };
                // We only care about modify/create. Remove events get
                // synthesized in the receiver loop (it detects "file no
                // longer exists" on its own when extracting symbols).
                let interesting = matches!(
                    evt.kind,
                    EventKind::Modify(ModifyKind::Data(_))
                        | EventKind::Modify(ModifyKind::Any)
                        | EventKind::Create(_)
                );
                if !interesting {
                    return;
                }
                for path in evt.paths {
                    if let Some(ext) = should_watch(&path, &extensions) {
                        debug!(
                            "symbol_watcher: event {:?} for {} (ext={})",
                            evt.kind,
                            path.display(),
                            ext
                        );
                        let send_res = tx_clone.try_send(SaveEvent {
                            path: path.clone(),
                            extension: ext,
                        });
                        if let Err(e) = send_res {
                            warn!(
                            "symbol_watcher: dropped event for {} — channel full or closed: {e}",
                            path.display()
                        );
                        }
                    }
                }
            });
        let mut watcher = match watcher_result {
            Ok(w) => w,
            Err(e) => {
                warn!(
                    "symbol_watcher: failed to construct watcher for {}: {e}",
                    root_clone.display()
                );
                continue;
            }
        };
        if let Err(e) = watcher.watch(&root_clone, RecursiveMode::Recursive) {
            warn!(
                "symbol_watcher: failed to register recursive watch on {}: {e}",
                root_clone.display()
            );
            continue;
        }
        debug!("symbol_watcher: watching {}", root_clone.display());
        watchers.push(watcher);
    }
    (rx, watchers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_watch_filters_by_extension() {
        let exts = vec!["rs".to_string(), "ts".to_string()];
        assert_eq!(
            should_watch(Path::new("/tmp/foo.rs"), &exts),
            Some("rs".to_string())
        );
        assert_eq!(
            should_watch(Path::new("/tmp/foo.ts"), &exts),
            Some("ts".to_string())
        );
        // Case-insensitive on the extension.
        assert_eq!(
            should_watch(Path::new("/tmp/Foo.RS"), &exts),
            Some("rs".to_string())
        );
        assert_eq!(should_watch(Path::new("/tmp/foo.md"), &exts), None);
        assert_eq!(should_watch(Path::new("/tmp/foo"), &exts), None);
    }

    #[test]
    fn should_watch_excludes_target_dir() {
        let exts = vec!["rs".to_string()];
        assert_eq!(
            should_watch(Path::new("/repo/target/debug/foo.rs"), &exts),
            None
        );
        assert_eq!(
            should_watch(Path::new("/repo/src/foo.rs"), &exts),
            Some("rs".to_string())
        );
    }

    #[test]
    fn should_watch_excludes_git_dir() {
        let exts = vec!["rs".to_string()];
        assert_eq!(should_watch(Path::new("/repo/.git/HEAD.rs"), &exts), None);
    }

    #[test]
    fn should_watch_excludes_node_modules() {
        let exts = vec!["ts".to_string()];
        assert_eq!(
            should_watch(Path::new("/repo/node_modules/foo/bar.ts"), &exts),
            None
        );
    }

    #[test]
    fn should_watch_excludes_venv() {
        let exts = vec!["py".to_string()];
        assert_eq!(
            should_watch(Path::new("/repo/.venv/lib/site.py"), &exts),
            None
        );
    }
}
