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

/// Host memory, commit and swap, all in bytes. `None` on any field whose probe
/// could not be read — absence is UNKNOWN, never zero.
///
/// **`commit_available_bytes` is the field the pre-permit memory guard enforces
/// its floor against** ([`crate::build_monitor::available_commit_bytes`]), and
/// the field `cargo-guard.sh` reads through
/// `Win32_OperatingSystem.FreeVirtualMemory`. It is deliberately NOT
/// `mem_available_bytes`: the binding constraint for a big rustc here is the
/// commit limit, and builds have died at ~90% commit while free-physical looked
/// healthy. Publishing both, named for what they are, is what makes a lane's
/// drift onto a different quantity visible instead of silent.
///
/// **Swap, and what it means per lane.** On a saturated *Linux/WSL* box
/// `mem_available` is pinned by the kernel reserve and reads as an all-clear
/// (measured on this fleet at −13.5 ± 11.2 M/day, indistinguishable from zero)
/// while `swap_used` moves (+138.6 ± 41.7 M/day) — which is why a consumer must
/// lead on swap there rather than on mem-available.
///
/// That guidance does NOT transfer verbatim to the Windows **host** lane this
/// module publishes. sysinfo derives Windows swap from the commit counters
/// (`swap_total = CommitLimit − PhysicalTotal`,
/// `swap_used = CommitTotal − PhysicalTotal`, both saturating), so on this lane
/// `swap_total − swap_used` is *identical* to `commit_available_bytes` in the
/// same row, and `swap_used` pins to 0 whenever commit charge sits below
/// physical RAM. A ranker that leads on swap here is double-weighting a
/// quantity it already reads. The fields are still published — they are the §A1
/// columns and they are the right signal on the WSL/Linux lane — but the
/// commit pair is the honest saturation signal for `lane='host'`.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct MemorySnapshot {
    pub mem_total_bytes: Option<u64>,
    pub mem_available_bytes: Option<u64>,
    pub commit_total_bytes: Option<u64>,
    pub commit_available_bytes: Option<u64>,
    pub swap_total_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
}

/// Build-pool occupancy at snapshot time: how many cargo slots exist, how many
/// are executing a build, and how many callers are queued on the permit
/// semaphore.
///
/// Populated by [`crate::state::BuildPool::occupancy`], which derives every
/// field from the same per-slot `busy` scan and the same `queue_depth` counter
/// `GET /builds` renders — there is exactly one accounting of the pool.
/// All-`None` means the occupancy was not supplied (e.g. a bare
/// [`compute_snapshot`] call in a test), which is UNKNOWN, not "idle".
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct BuildPoolOccupancy {
    pub build_slots_total: Option<u32>,
    pub build_slots_busy: Option<u32>,
    pub build_queue_depth: Option<u32>,
}

/// A full footprint snapshot. `computed_at` makes staleness explicit to every
/// reader. `disk_free_bytes` is the free space on the volume containing the
/// build-pool root (`None` when sysinfo could not resolve a containing disk);
/// `disk_total_bytes` / `disk_mount` describe that same volume, so a free-byte
/// figure can be read against its own capacity and attributed to a mount.
///
/// The `memory` and `build_pool` groups are **flattened** into the serialized
/// form, so `GET /builds`'s `footprint` object carries `mem_total_bytes`,
/// `commit_available_bytes`, `swap_used_bytes`, `build_slots_busy`, … as
/// top-level keys — the §A1 column names of `coord.device_resource_samples`,
/// so the published sample and the local surface cannot drift apart.
#[derive(Debug, Clone, Serialize)]
pub struct FootprintSnapshot {
    pub computed_at: String,
    pub disk_free_bytes: Option<u64>,
    pub disk_total_bytes: Option<u64>,
    pub disk_mount: Option<String>,
    #[serde(flatten)]
    pub memory: MemorySnapshot,
    #[serde(flatten)]
    pub build_pool: BuildPoolOccupancy,
    pub slots: Vec<SlotFootprint>,
    pub lkg_bytes: u64,
    pub spawn_containers: SpawnContainersFootprint,
    pub exe_copies: ExeCopiesFootprint,
}

/// Sample host memory, commit and swap.
///
/// Commit comes from [`crate::build_monitor::available_commit_bytes`] /
/// [`crate::build_monitor::total_commit_bytes`] — the guard's own probe, called
/// rather than reimplemented, so the published number and the enforced floor
/// can never measure different things. Physical memory and swap come from
/// sysinfo's single refreshed `System`.
///
/// A zero `mem_total_bytes` is reported as `None`: a box with no memory is not
/// a state that exists, so zero there means the probe failed. **Every other
/// field keeps a genuine `Some(0)`** — a zero `mem_available` or
/// `commit_available` is the extreme-pressure reading this telemetry exists to
/// catch, and mapping it to UNKNOWN would erase the signal at exactly the
/// moment it matters (the mirror image of the false-safe this plan is fixing).
/// Likewise `Some(0)` swap means "no swap configured", which is real.
pub fn memory_snapshot() -> MemorySnapshot {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    MemorySnapshot {
        mem_total_bytes: Some(sys.total_memory()).filter(|b| *b > 0),
        mem_available_bytes: Some(sys.available_memory()),
        commit_total_bytes: crate::build_monitor::total_commit_bytes(),
        commit_available_bytes: crate::build_monitor::available_commit_bytes(),
        swap_total_bytes: Some(sys.total_swap()),
        swap_used_bytes: Some(sys.used_swap()),
    }
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

/// Free + total space and mount point for the volume containing `path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiskUsage {
    pub free_bytes: u64,
    pub total_bytes: u64,
    /// Mount point of the resolved volume (`D:\` on Windows, `/` or a mount
    /// path elsewhere). Attributing a free-byte figure to a mount is what stops
    /// a multi-volume host's numbers being read as one pool.
    pub mount: String,
}

/// Free/total/mount for the volume containing `path`, via sysinfo's `Disks`
/// enumerator. Picks the disk whose mount point is the longest prefix of
/// `path` (most-specific mount wins). `None` when no disk contains the path or
/// the path can't be canonicalized.
pub fn disk_usage_for(path: &Path) -> Option<DiskUsage> {
    use sysinfo::Disks;
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    // On Windows `canonicalize` returns a verbatim path (`\\?\D:\...`) whose
    // `starts_with` never matches sysinfo's plain `D:\` mount point, which would
    // make the guard silently fail-open on every real path. Strip the `\\?\`
    // (and `\\?\UNC\`) verbatim prefix before prefix-matching. No-op on non-UNC
    // / non-Windows paths.
    let target = strip_verbatim_prefix(&canon);
    let disks = Disks::new_with_refreshed_list();
    let mut best: Option<(usize, DiskUsage)> = None;
    for d in disks.list() {
        let mp = d.mount_point();
        if target.starts_with(mp) {
            let len = mp.as_os_str().len();
            if best.as_ref().map(|(blen, _)| len > *blen).unwrap_or(true) {
                best = Some((
                    len,
                    DiskUsage {
                        free_bytes: d.available_space(),
                        total_bytes: d.total_space(),
                        mount: mp.to_string_lossy().into_owned(),
                    },
                ));
            }
        }
    }
    best.map(|(_, usage)| usage)
}

/// Free bytes on the volume containing `path`. Thin projection of
/// [`disk_usage_for`] — the pre-permit disk guard reads this, so the guard and
/// the published sample resolve the same volume by the same rule.
pub fn disk_free_bytes_for(path: &Path) -> Option<u64> {
    disk_usage_for(path).map(|u| u.free_bytes)
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
///
/// Leaves `build_pool` UNKNOWN (all-`None`).
/// [`crate::state::SupervisorState::refresh_footprint`] fills it in **after**
/// this returns: occupancy lives behind async locks (this function is sync +
/// `spawn_blocking`-hosted), and reading it *before* a minutes-long walk would
/// stamp `computed_at` on a slot count from minutes earlier — advertising slots
/// that filled while the walk ran, which is exactly the confidently-wrong
/// number this telemetry exists to remove.
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
    let disk = disk_usage_for(&disk_probe);

    FootprintSnapshot {
        computed_at: chrono::Utc::now().to_rfc3339(),
        disk_free_bytes: disk.as_ref().map(|d| d.free_bytes),
        disk_total_bytes: disk.as_ref().map(|d| d.total_bytes),
        disk_mount: disk.map(|d| d.mount),
        memory: memory_snapshot(),
        build_pool: BuildPoolOccupancy::default(),
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
    fn disk_usage_reports_total_and_mount_for_the_same_volume() {
        let tmp = tempfile::tempdir().unwrap();
        let usage = disk_usage_for(tmp.path()).expect("a real tempdir resolves to a disk");
        assert!(
            usage.total_bytes >= usage.free_bytes,
            "free ({}) cannot exceed total ({})",
            usage.free_bytes,
            usage.total_bytes
        );
        assert!(
            !usage.mount.is_empty(),
            "resolved volume must name its mount"
        );
        // The guard's projection must resolve the SAME volume by the same rule
        // — one accounting, not two. Compared on the mount, not on free bytes:
        // free space moves between two reads of a live disk, so a byte-equality
        // assertion here would flake without pinning anything extra.
        let again = disk_usage_for(tmp.path()).expect("second probe resolves too");
        assert_eq!(again.mount, usage.mount);
        assert_eq!(again.total_bytes, usage.total_bytes);
        assert!(disk_free_bytes_for(tmp.path()).is_some());
    }

    #[test]
    fn memory_snapshot_reports_the_guards_own_commit_probe() {
        let snap = memory_snapshot();
        // The published commit field IS the pre-permit guard's probe — not a
        // second memory quantity sampled alongside it. If this ever diverges,
        // the dashboard would show a headroom number the guard does not enforce.
        assert_eq!(
            snap.commit_available_bytes.is_some(),
            crate::build_monitor::available_commit_bytes().is_some(),
            "commit_available_bytes must come from build_monitor::available_commit_bytes"
        );
        if let (Some(avail), Some(total)) = (snap.commit_available_bytes, snap.commit_total_bytes) {
            assert!(
                avail <= total,
                "available commit ({avail}) cannot exceed the commit ceiling ({total})"
            );
        }
        // Physical memory must be readable on any host this runs on, and a
        // zero total is reported as unknown rather than as "no memory".
        assert!(snap.mem_total_bytes.is_some(), "mem_total_bytes must probe");
        assert_ne!(snap.mem_total_bytes, Some(0));
        if let (Some(avail), Some(total)) = (snap.mem_available_bytes, snap.mem_total_bytes) {
            assert!(avail <= total);
        }
        // Swap keeps a genuine Some(0) — "no swap configured" is a real state,
        // and swap is the metric that keeps moving under saturation.
        assert!(
            snap.swap_total_bytes.is_some(),
            "swap_total must be sampled"
        );
        assert!(snap.swap_used_bytes.is_some(), "swap_used must be sampled");
        if let (Some(used), Some(total)) = (snap.swap_used_bytes, snap.swap_total_bytes) {
            assert!(
                used <= total,
                "swap used ({used}) cannot exceed total ({total})"
            );
        }
    }

    #[test]
    fn snapshot_serializes_the_a1_column_names_flat() {
        // The published sample and `GET /builds`'s footprint object must carry
        // the §A1 column names of `coord.device_resource_samples`. A rename on
        // either side has to fail here rather than silently produce a column
        // coord drops on the floor.
        let snap = FootprintSnapshot {
            computed_at: "2026-08-06T00:00:00Z".to_string(),
            disk_free_bytes: Some(1),
            disk_total_bytes: Some(2),
            disk_mount: Some("D:\\".to_string()),
            memory: MemorySnapshot {
                mem_total_bytes: Some(3),
                mem_available_bytes: Some(4),
                commit_total_bytes: Some(5),
                commit_available_bytes: Some(6),
                swap_total_bytes: Some(7),
                swap_used_bytes: Some(8),
            },
            build_pool: BuildPoolOccupancy {
                build_slots_total: Some(3),
                build_slots_busy: Some(1),
                build_queue_depth: Some(2),
            },
            slots: vec![],
            lkg_bytes: 0,
            spawn_containers: SpawnContainersFootprint::default(),
            exe_copies: ExeCopiesFootprint::default(),
        };
        let v = serde_json::to_value(&snap).unwrap();
        for key in [
            "disk_free_bytes",
            "disk_total_bytes",
            "disk_mount",
            "mem_total_bytes",
            "mem_available_bytes",
            "commit_total_bytes",
            "commit_available_bytes",
            "swap_total_bytes",
            "swap_used_bytes",
            "build_slots_total",
            "build_slots_busy",
            "build_queue_depth",
        ] {
            assert!(
                v.get(key).is_some(),
                "footprint snapshot must expose `{key}` as a top-level key"
            );
        }
        assert_eq!(v["commit_available_bytes"], 6);
        assert_eq!(v["swap_used_bytes"], 8);
        assert_eq!(v["build_slots_busy"], 1);
    }

    #[test]
    fn unsupplied_build_pool_occupancy_is_unknown_not_idle() {
        // All-None must NOT read as "0 slots busy" — absence of signal is not
        // an idle pool, and a consumer ranking on it would prefer a machine it
        // knows nothing about.
        let occ = BuildPoolOccupancy::default();
        assert_eq!(occ.build_slots_total, None);
        assert_eq!(occ.build_slots_busy, None);
        assert_eq!(occ.build_queue_depth, None);
        let v = serde_json::to_value(occ).unwrap();
        assert!(v["build_slots_busy"].is_null());
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
