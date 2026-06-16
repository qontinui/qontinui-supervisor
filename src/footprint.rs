//! Build-artifact footprint accounting (plan
//! `2026-06-05-supervisor-build-artifact-footprint`).
//!
//! The supervisor accumulates large build artifacts on disk: per-slot
//! `target-pool/slot-*/` trees (GBs each), the `target-pool/lkg/` copy,
//! `.spawn-*` scratch-worktree containers under the workspace root, and one
//! `qontinui-runner-<id>.exe` copy per started runner under
//! `<npm_dir>/target/debug/`. Two days before this module landed a build died
//! with `os error 112` (disk full) at 1.9 GB free while a single slot held
//! ~369.8 GB.
//!
//! This module computes a cheap-to-serialize **snapshot** of that footprint so
//! it can be surfaced on `GET /builds`, embedded in the pre-permit disk-guard
//! refusal, and reported by the prune endpoints. Walking real trees is
//! minutes-slow, so the snapshot is cached on [`crate::state::SupervisorState`]
//! and refreshed on a timer (or on demand via `?refresh_footprint=1`); it is
//! NEVER walked inline on an unparameterized GET.

use serde::Serialize;
use std::path::Path;
use std::time::SystemTime;

use crate::config::SupervisorConfig;

/// Per-slot byte total.
#[derive(Debug, Clone, Serialize)]
pub struct SlotFootprint {
    pub id: usize,
    pub bytes: u64,
}

/// `.spawn-*` scratch-worktree container accounting (count + total bytes +
/// oldest mtime). `oldest_mtime` is an RFC3339 string (None when there are no
/// containers or every mtime was unreadable).
#[derive(Debug, Clone, Serialize, Default)]
pub struct SpawnContainersFootprint {
    pub count: usize,
    pub bytes: u64,
    pub oldest_mtime: Option<String>,
}

/// Per-runner exe copies under `<npm_dir>/target/debug/qontinui-runner-*.exe`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ExeCopiesFootprint {
    pub count: usize,
    pub bytes: u64,
}

/// A full footprint snapshot. `computed_at` makes staleness explicit to every
/// reader. `disk_free_bytes` is the free space on the volume containing the
/// build-pool root (`None` when sysinfo could not resolve a containing disk).
#[derive(Debug, Clone, Serialize)]
pub struct FootprintSnapshot {
    pub computed_at: String,
    pub disk_free_bytes: Option<u64>,
    pub slots: Vec<SlotFootprint>,
    pub lkg_bytes: u64,
    pub spawn_containers: SpawnContainersFootprint,
    pub exe_copies: ExeCopiesFootprint,
}

/// Recursively sum the byte size of every regular file under `dir`.
///
/// Best-effort: unreadable entries are skipped (counted as 0), symlinks are
/// NOT followed (we sum `metadata` via `read_dir`'s entry, which does not
/// traverse links), and a missing directory yields 0. Pure + synchronous so it
/// can be unit-tested against a tempdir fixture and reused by the prune
/// endpoints to measure containers pre-delete.
pub fn dir_size_bytes(dir: &Path) -> u64 {
    fn walk(dir: &Path, acc: &mut u64) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            // `symlink_metadata` does not traverse symlinks, so a symlinked
            // dir is counted as the (tiny) link itself, never walked into.
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                walk(&entry.path(), acc);
            } else if file_type.is_file() {
                *acc = acc.saturating_add(meta.len());
            }
        }
    }
    let mut acc = 0u64;
    walk(dir, &mut acc);
    acc
}

/// Strip a Windows verbatim (`\\?\`) path prefix so a canonicalized path can be
/// prefix-matched against a plain drive-letter mount point. `\\?\D:\x` becomes
/// `D:\x`; `\\?\UNC\server\share` becomes `\\server\share`. Returns the path
/// unchanged when there is no verbatim prefix (always the case off Windows).
fn strip_verbatim_prefix(path: &Path) -> std::path::PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return std::path::PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return std::path::PathBuf::from(rest);
    }
    path.to_path_buf()
}

/// Free bytes on the volume containing `path`, via sysinfo's `Disks`
/// enumerator. Picks the disk whose mount point is the longest prefix of
/// `path` (most-specific mount wins). `None` when no disk contains the path or
/// the path can't be canonicalized.
pub fn disk_free_bytes_for(path: &Path) -> Option<u64> {
    use sysinfo::Disks;
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    // On Windows `canonicalize` returns a verbatim path (`\\?\D:\...`) whose
    // `starts_with` never matches sysinfo's plain `D:\` mount point, which would
    // make the guard silently fail-open on every real path. Strip the `\\?\`
    // (and `\\?\UNC\`) verbatim prefix before prefix-matching. No-op on non-UNC
    // / non-Windows paths.
    let target = strip_verbatim_prefix(&canon);
    let disks = Disks::new_with_refreshed_list();
    let mut best: Option<(usize, u64)> = None;
    for d in disks.list() {
        let mp = d.mount_point();
        if target.starts_with(mp) {
            let len = mp.as_os_str().len();
            if best.map(|(blen, _)| len > blen).unwrap_or(true) {
                best = Some((len, d.available_space()));
            }
        }
    }
    best.map(|(_, free)| free)
}

/// Enumerate `<npm_dir>/target/debug/qontinui-runner-*.exe` per-runner exe
/// copies (NOT the bare `qontinui-runner.exe`, which is the legacy single
/// build output). Returns count + total bytes.
fn exe_copies_footprint(npm_dir: &Path) -> ExeCopiesFootprint {
    let debug_dir = npm_dir.join("target").join("debug");
    let mut out = ExeCopiesFootprint::default();
    let entries = match std::fs::read_dir(&debug_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Match the per-runner copies `qontinui-runner-<id>[.exe]`, excluding the
        // bare build output (`qontinui-runner[.exe]`, no `-<id>`). Windows copies
        // carry a `.exe` extension; on macOS/Linux the copy is a bare Mach-O/ELF
        // with no extension — so require `.exe` on Windows and no extension
        // elsewhere (a `.json`/`.d`/`.pdb` sidecar is never an exe copy).
        let is_exe_copy = name.starts_with("qontinui-runner-")
            && std::path::Path::new(name.as_ref())
                .extension()
                .map_or(cfg!(not(windows)), |ext| ext == "exe");
        if is_exe_copy {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    out.count += 1;
                    out.bytes = out.bytes.saturating_add(meta.len());
                }
            }
        }
    }
    out
}

/// Enumerate `.spawn-*` containers directly under `workspace_root`, summing
/// their sizes and tracking the oldest mtime. Mirrors the prune engine's
/// prefix-under-root filter ([`crate::spawn_worktree::SPAWN_DIR_PREFIX`]) so
/// the two views agree on which dirs are scratch containers.
fn spawn_containers_footprint(workspace_root: &Path) -> SpawnContainersFootprint {
    use crate::spawn_worktree::SPAWN_DIR_PREFIX;
    let mut out = SpawnContainersFootprint::default();
    let mut oldest: Option<SystemTime> = None;
    let entries = match std::fs::read_dir(workspace_root) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(SPAWN_DIR_PREFIX) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_dir() {
            continue;
        }
        let path = entry.path();
        out.count += 1;
        out.bytes = out.bytes.saturating_add(dir_size_bytes(&path));
        if let Ok(mtime) = meta.modified() {
            oldest = Some(match oldest {
                Some(prev) if prev <= mtime => prev,
                _ => mtime,
            });
        }
    }
    out.oldest_mtime = oldest.map(|t| {
        let dt: chrono::DateTime<chrono::Utc> = t.into();
        dt.to_rfc3339()
    });
    out
}

/// Compute a fresh footprint snapshot from the configured paths.
///
/// Synchronous + best-effort: every individual size walk swallows IO errors
/// (yielding 0 for that component) so a permission glitch on one slot never
/// poisons the whole snapshot. Slow (walks GB-scale trees) — callers run it
/// off a timer or inside `spawn_blocking`, never inline on a hot path.
pub fn compute_snapshot(config: &SupervisorConfig) -> FootprintSnapshot {
    let npm_dir = config.runner_npm_dir();
    let pool_root = npm_dir.join("target-pool");

    let mut slots: Vec<SlotFootprint> = Vec::with_capacity(config.build_pool.pool_size);
    for id in 0..config.build_pool.pool_size {
        let dir = config.runner_slot_target_dir(id);
        slots.push(SlotFootprint {
            id,
            bytes: dir_size_bytes(&dir),
        });
    }

    let lkg_bytes = dir_size_bytes(&config.lkg_dir());

    // Derive the workspace root exactly as the prune engine does so the
    // `.spawn-*` accounting and the pruner look at the same dir.
    let spawn_containers = match crate::spawn_worktree::derive_workspace_root(&config.project_dir) {
        Ok(ws) => spawn_containers_footprint(&ws),
        Err(_) => SpawnContainersFootprint::default(),
    };

    let exe_copies = exe_copies_footprint(&npm_dir);

    // Disk-free for the volume holding the pool root (the thing that fills up).
    // Fall back to the npm dir if the pool root doesn't exist yet.
    let disk_probe = if pool_root.exists() {
        pool_root
    } else {
        npm_dir
    };
    let disk_free_bytes = disk_free_bytes_for(&disk_probe);

    FootprintSnapshot {
        computed_at: chrono::Utc::now().to_rfc3339(),
        disk_free_bytes,
        slots,
        lkg_bytes,
        spawn_containers,
        exe_copies,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn write_file(path: &Path, bytes: usize) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(&vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn dir_size_sums_nested_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join("a.bin"), 1000);
        write_file(&root.join("sub/b.bin"), 2000);
        write_file(&root.join("sub/deep/c.bin"), 500);
        assert_eq!(dir_size_bytes(root), 3500);
    }

    #[test]
    fn strip_verbatim_prefix_handles_windows_paths() {
        // Verbatim drive path → plain drive path (the case that broke the guard).
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\D:\qontinui-root\x")),
            std::path::PathBuf::from(r"D:\qontinui-root\x")
        );
        // Verbatim UNC → plain UNC.
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\UNC\server\share")),
            std::path::PathBuf::from(r"\\server\share")
        );
        // No verbatim prefix → unchanged.
        assert_eq!(
            strip_verbatim_prefix(Path::new("/tmp/x")),
            std::path::PathBuf::from("/tmp/x")
        );
    }

    #[test]
    fn disk_free_resolves_a_real_path() {
        // A real existing dir must resolve to a containing disk (the guard
        // depends on this — a None here is a silent fail-open).
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            disk_free_bytes_for(tmp.path()).is_some(),
            "disk_free_bytes_for must resolve a real tempdir to a disk"
        );
    }

    #[test]
    fn dir_size_missing_dir_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(dir_size_bytes(&tmp.path().join("does-not-exist")), 0);
    }

    #[test]
    fn exe_copies_matches_only_per_runner_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let npm = tmp.path();
        let debug = npm.join("target").join("debug");
        // Per-runner copies: counted.
        write_file(&debug.join("qontinui-runner-primary.exe"), 100);
        write_file(&debug.join("qontinui-runner-test-abc.exe"), 200);
        // Bare build output: NOT counted.
        write_file(&debug.join("qontinui-runner.exe"), 9999);
        // Unrelated: NOT counted.
        write_file(&debug.join("other.exe"), 9999);
        let fp = exe_copies_footprint(npm);
        assert_eq!(fp.count, 2);
        assert_eq!(fp.bytes, 300);
    }

    #[test]
    fn spawn_containers_counts_prefix_dirs_and_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        write_file(&ws.join(".spawn-aaa/file"), 1000);
        write_file(&ws.join(".spawn-bbb/file"), 2000);
        // Not a spawn container: ignored.
        write_file(&ws.join("regular-dir/file"), 5000);
        // A file (not dir) with the prefix: ignored.
        write_file(&ws.join(".spawn-notadir"), 7000);
        let fp = spawn_containers_footprint(ws);
        assert_eq!(fp.count, 2);
        assert_eq!(fp.bytes, 3000);
        assert!(fp.oldest_mtime.is_some());
    }
}
