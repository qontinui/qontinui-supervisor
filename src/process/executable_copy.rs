//! The copies this process execs — a runner's exe copy, the pair route's
//! private CLI copy — written so that no fork of this process holds a write
//! descriptor on them when they are exec'd.
//!
//! **The race.** A start used to write the runner's exe copy with an in-process
//! `std::fs::copy` and then exec the copy. The copy's descriptor is
//! `O_CLOEXEC`, but close-on-exec closes it only when a child *execs*: a thread
//! that forks (or posix-spawns) while the copy is open hands its child a
//! duplicate, and until that child execs, the inode has a writer. On Linux,
//! `execve` of a file with a writer fails `ETXTBSY`, so the start failed
//! `Failed to spawn exe: Text file busy (os error 26)`. This process forks from
//! many threads (cargo, git, lsof/netstat, sccache), and its test suite flaked
//! on exactly this (coord finding `ed257f86`).
//!
//! **The remedy, on Unix.** A child `cp` writes the bytes to a temp name beside
//! the destination, and the temp file is renamed over the destination. This
//! process never opens either file for writing, so none of its forks can
//! inherit a writer, and `cp` forks nothing. The rename also gives the exec
//! target a fresh inode: a process still writing the inode the destination
//! named before (an inheritor of an earlier in-place write) holds nothing the
//! exec reads, and a process still running a prior copy keeps running its own,
//! now unlinked, inode.
//!
//! **Measured** on nilan (Linux 6.12.115), 2026-11-05, plan
//! `2026-11-05-supervisor-test-flakes-dead-ports-etxtbsy`. With a sibling parked
//! between fork and exec while the parent wrote the file, the exec failed
//! `ETXTBSY`; re-copying in place, onto the same inode, failed again; a child
//! `cp` to a temp name plus rename exec'd with the sibling still parked. With 8
//! threads forking through 300 copy-and-exec iterations, an in-process write to
//! a temp name plus rename was refused 82 to 116 times a run, red in 10 of 10
//! runs, and this copy 0 times in 10 of 10. qontinui-runner #2460 measured the
//! same race (8 to 22 per 300) and closed it the same way (0 in 12 000).
//!
//! **Why not something else** (the plan's D1):
//! - *Not a retry.* Re-copying in place writes the inode an inheritor already
//!   holds, so it fails for as long as that inheritor lives. A retry at the
//!   spawn that waits the inheritor out hides every regression, and cannot see
//!   a refusal that surfaces as a wrapper's exit (`systemd-run --scope`) rather
//!   than as a spawn error.
//! - *Not a lock.* A process-wide fork lock would have to cover every fork in
//!   this process and its dependencies, so it is incomplete by construction.
//! - *Not an in-process write to a temp name.* A fork during that write
//!   inherits the temp file's descriptor, and the rename then hands the exec
//!   that very inode.
//! - *Not a hard link.* It would share the build artifact's inode, so an
//!   in-place write to the artifact (a build-cache materialization) would fail
//!   `ETXTBSY` while a runner runs it. Windows needs a copy anyway.
//!
//! **Windows** keeps `std::fs::copy` into the destination. It has no `ETXTBSY`,
//! and a prior copy that a running instance still locks is what the caller's
//! remove-and-retry is for.
//!
//! **The temp name is fixed per destination**, `.{name}.copy-tmp`, as
//! `deploy_sidecar`'s and the last-known-good copy's are. A leftover from a
//! supervisor that died between the `cp` and the rename is removed by the next
//! copy to that destination, where a unique name would leak a whole runner
//! binary every time. A leftover that cannot be removed (a directory, which
//! `cp` would copy into) refuses the copy. Two copies to one destination at once
//! would share the name, so callers serialize them: a runner's starts hold its
//! lifecycle lock, and the pair route copies into a directory of its own.

use std::io;
use std::path::Path;

/// Copy `src` to `dst`, for `dst` to be exec'd, and return the bytes copied.
/// On Unix a child `cp` writes a temp file that is renamed over `dst` (see the
/// module docs); on Windows this is `std::fs::copy`.
///
/// Every error names `src`. A source that is not a regular file is refused, as
/// `std::fs::copy` refuses one: a `cp` would block on a FIFO, or copy a device
/// without end. A failed Unix copy removes its temp file, so it leaves nothing
/// new beside `dst`, and `dst` is replaced only by a complete copy.
pub(crate) fn copy_executable(src: &Path, dst: &Path) -> io::Result<u64> {
    #[cfg(unix)]
    {
        unix::copy_through_a_child(src, dst)
    }
    #[cfg(not(unix))]
    {
        std::fs::copy(src, dst)
            .map_err(|e| io::Error::new(e.kind(), format!("copy {src:?} to {dst:?}: {e}")))
    }
}

#[cfg(unix)]
mod unix {
    use std::ffi::OsString;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    /// The fixed temp name a copy to `dst` is written through:
    /// `.{name}.copy-tmp`, beside `dst`.
    fn temp_path_for(dst: &Path) -> io::Result<PathBuf> {
        let name = dst.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{dst:?} names no file to copy an executable to"),
            )
        })?;
        let mut tmp = OsString::from(".");
        tmp.push(name);
        tmp.push(".copy-tmp");
        Ok(dst.with_file_name(tmp))
    }

    pub(super) fn copy_through_a_child(src: &Path, dst: &Path) -> io::Result<u64> {
        let source = std::fs::metadata(src).map_err(|e| {
            io::Error::new(e.kind(), format!("cannot read the source {src:?}: {e}"))
        })?;
        if !source.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{src:?} is not a regular file, so it is not copied"),
            ));
        }
        let tmp = temp_path_for(dst)?;
        // A leftover of an earlier copy to this destination that never reached
        // its rename. It has to go first: `cp` would copy into a leftover
        // directory, and could not overwrite a read-only leftover file.
        match std::fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("cannot copy {src:?}: the leftover {tmp:?} cannot be removed: {e}"),
                ));
            }
        }
        let copied = cp_then_rename(src, &tmp, dst);
        if copied.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        copied
    }

    fn cp_then_rename(src: &Path, tmp: &Path, dst: &Path) -> io::Result<u64> {
        let output = Command::new("cp")
            .arg("--")
            .arg(src)
            .arg(tmp)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("could not run `cp` to copy {src:?} to {tmp:?}: {e}"),
                )
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = match stderr.trim() {
                "" => "nothing on stderr",
                said => said,
            };
            return Err(io::Error::other(format!(
                "`cp -- {src:?} {tmp:?}` failed ({}): {stderr}",
                output.status
            )));
        }
        let copied = std::fs::metadata(tmp)
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("`cp` copied {src:?} to {tmp:?}, which then could not be read: {e}"),
                )
            })?
            .len();
        std::fs::rename(tmp, dst).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("copied {src:?} to {tmp:?}, but could not rename it over {dst:?}: {e}"),
            )
        })?;
        Ok(copied)
    }
}

#[cfg(test)]
mod tests {
    use super::copy_executable;
    use std::path::Path;

    /// The names in `dir`, sorted, so a test can say exactly what a copy left.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read the dir")
            .map(|entry| {
                entry
                    .expect("a dir entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// A copy that fails names its source, and leaves nothing beside its
    /// destination, however it fails: a source that is missing or is not a
    /// regular file is refused before anything is written; a destination whose
    /// directory does not exist fails the copy itself; and a destination that
    /// is a directory lets the bytes be copied and then refuses their rename.
    #[test]
    fn a_failed_copy_names_its_source_and_leaves_no_temp_file() {
        let root = tempfile::tempdir().expect("tempdir");
        let dest_dir = root.path().join("dest");
        std::fs::create_dir(&dest_dir).expect("mkdir dest");
        let occupied = dest_dir.join("occupied");
        std::fs::create_dir(&occupied).expect("mkdir the occupied destination");
        let file = root.path().join("source");
        std::fs::write(&file, b"an executable").expect("write the source");
        let directory = root.path().join("a-directory");
        std::fs::create_dir(&directory).expect("mkdir the directory source");

        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut cases = vec![
            (
                "a missing source",
                root.path().join("no-such-source"),
                dest_dir.join("runner"),
            ),
            ("a directory source", directory, dest_dir.join("runner")),
            (
                "a destination whose directory does not exist",
                file.clone(),
                dest_dir.join("missing").join("runner"),
            ),
            ("a destination that is a directory", file, occupied.clone()),
        ];
        // A device, which a `cp` would copy as an empty file rather than refuse.
        #[cfg(unix)]
        cases.push((
            "a device source",
            std::path::PathBuf::from("/dev/null"),
            dest_dir.join("runner"),
        ));
        for (case, src, dst) in cases {
            let err =
                copy_executable(&src, &dst).expect_err(&format!("{case}: the copy must fail"));
            assert!(
                err.to_string().contains(&format!("{src:?}")),
                "{case}: the error must name its source: {err}"
            );
            assert_eq!(
                entries(&dest_dir),
                ["occupied"],
                "{case}: a failed copy left something behind"
            );
            assert_eq!(
                entries(&occupied),
                Vec::<String>::new(),
                "{case}: the occupied destination was written into"
            );
        }
    }

    /// A leftover of an interrupted copy, at the fixed temp name beside the
    /// destination, is cleared before the next copy. A leftover file is
    /// removed, so the copy succeeds even when it is read-only, which a `cp`
    /// could not overwrite (a root test process can, so it proves nothing
    /// there). A leftover directory, which a `cp` would copy into, cannot be
    /// removed that way, so the copy is refused and nothing is written.
    #[cfg(unix)]
    #[test]
    fn a_leftover_of_an_interrupted_copy_is_cleared_first() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("tempdir");
        let src = root.path().join("source");
        std::fs::write(&src, b"the new copy").expect("write the source");
        let dst = root.path().join("runner");
        let leftover = root.path().join(".runner.copy-tmp");

        std::fs::write(&leftover, b"half of an old copy").expect("write the leftover");
        std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o444))
            .expect("make the leftover read-only");
        let copied = copy_executable(&src, &dst).expect("a copy over a leftover file");
        assert_eq!(copied as usize, b"the new copy".len());
        assert_eq!(std::fs::read(&dst).expect("read the copy"), b"the new copy");
        assert_eq!(entries(root.path()), ["runner", "source"]);

        std::fs::create_dir(&leftover).expect("mkdir the leftover");
        let err =
            copy_executable(&src, &dst).expect_err("a leftover directory must refuse the copy");
        assert!(
            err.to_string().contains(&format!("{leftover:?}")),
            "the error must name the leftover: {err}"
        );
        assert_eq!(
            entries(&leftover),
            Vec::<String>::new(),
            "the refused copy wrote into the leftover directory"
        );
        assert_eq!(
            std::fs::read(&dst).expect("read the copy"),
            b"the new copy",
            "the refused copy replaced the destination"
        );
    }

    /// An empty source is copied, and reported as zero bytes, not refused: the
    /// pair route refuses a zero-byte copy itself, from this count.
    #[test]
    fn a_zero_length_source_copies_zero_bytes() {
        let root = tempfile::tempdir().expect("tempdir");
        let src = root.path().join("empty");
        std::fs::write(&src, b"").expect("write the empty source");
        let dst = root.path().join("copy");

        assert_eq!(copy_executable(&src, &dst).expect("the copy"), 0);
        assert_eq!(std::fs::metadata(&dst).expect("the copy exists").len(), 0);
        assert_eq!(entries(root.path()), ["copy", "empty"]);
    }

    /// A copy over an executable that is running replaces it: the running
    /// process carries on, on the inode it was started from, and the next exec
    /// of the destination runs the new content. An in-place write would be
    /// refused instead, `Text file busy` on open, because the running image's
    /// inode is the one written.
    #[cfg(unix)]
    #[test]
    fn a_copy_replaces_an_executable_that_is_running() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Stdio};
        let root = tempfile::tempdir().expect("tempdir");
        let dst = root.path().join("runner");
        copy_executable(Path::new("/bin/bash"), &dst).expect("copy bash");
        // The copy of bash waits for a line on its stdin, a builtin wait, so it
        // runs from the copy's inode until the test sends one.
        let mut running = Command::new(&dst)
            .args(["-c", "read -r _"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("run the copy");

        let replacement = root.path().join("replacement");
        let script = "#!/bin/sh\necho replaced\n";
        std::fs::write(&replacement, script).expect("write the replacement");
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o755))
            .expect("make the replacement executable");
        let copied = copy_executable(&replacement, &dst);

        let still_running = matches!(running.try_wait(), Ok(None));
        if let Some(mut stdin) = running.stdin.take() {
            // A process that died has closed the pipe; its exit says so below.
            let _ = stdin.write_all(b"done\n");
        }
        let ended = running.wait().expect("reap the running copy");
        assert_eq!(
            copied.expect("a copy over a running executable") as usize,
            script.len()
        );
        assert!(
            still_running,
            "the process running the old copy did not survive its replacement"
        );
        assert!(ended.success(), "the old copy ended {ended}");
        let replaced = Command::new(&dst)
            .stdin(Stdio::null())
            .output()
            .expect("exec the new copy");
        assert_eq!(String::from_utf8_lossy(&replaced.stdout), "replaced\n");
    }

    /// Phase 3's stress test, at the calibration Phase 1 recorded (K = 8 forking
    /// threads, N = 300 copy-and-exec iterations of `/bin/true`): while other
    /// threads fork, no exec of a fresh copy is refused `ETXTBSY`. An
    /// in-process write to the temp name plus rename was refused 82 to 116 of
    /// 300 times at this calibration, in 10 of 10 runs.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_copied_executable_execs_while_other_threads_fork() {
        use std::process::{Command, Stdio};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        const FORKING_THREADS: usize = 8;
        const ITERATIONS: usize = 300;

        let root = tempfile::tempdir().expect("tempdir");
        let dst = root.path().join("copy");
        let stop = Arc::new(AtomicBool::new(false));
        let forkers: Vec<_> = (0..FORKING_THREADS)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut spawned = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        let status = Command::new("/bin/true")
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status();
                        if status.is_ok() {
                            spawned += 1;
                        }
                    }
                    spawned
                })
            })
            .collect();

        let mut refused = 0usize;
        let mut failure = None;
        for _ in 0..ITERATIONS {
            if let Err(e) = copy_executable(Path::new("/bin/true"), &dst) {
                failure = Some(format!("the copy failed: {e}"));
                break;
            }
            match Command::new(&dst)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
            {
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => refused += 1,
                Err(e) => {
                    failure = Some(format!("the exec of the copy failed: {e}"));
                    break;
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        let spawned: u64 = forkers
            .into_iter()
            .map(|forker| forker.join().expect("a forking thread"))
            .sum();

        if let Some(failure) = failure {
            panic!("{failure}");
        }
        assert!(
            spawned >= ITERATIONS as u64,
            "only {spawned} forks overlapped {ITERATIONS} copies, so this run proves nothing"
        );
        assert_eq!(
            refused, 0,
            "{refused} of {ITERATIONS} execs of a fresh copy were refused ETXTBSY while \
             {FORKING_THREADS} threads forked ({spawned} forks)"
        );
    }
}
