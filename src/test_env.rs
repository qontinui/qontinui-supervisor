//! Test-only: serializes the tests that set or read the pre-permit build
//! guards' env floors.
//!
//! `check_disk_guard` and `check_ram_guard` read
//! `QONTINUI_SUPERVISOR_MIN_FREE_DISK_GB`, `…_MIN_FREE_RAM_GB` and
//! `…_MIN_FREE_PHYS_GB` from the process environment on every build. A test
//! that drives a REAL build through those guards has to switch them off, or it
//! is refused `InsufficientDisk` (or deferred for the whole memory window) on a
//! small CI runner without ever taking a permit. The environment is shared by
//! every test thread, so the setter holds [`BUILD_GUARD_ENV`] for as long as
//! the floors are off, and a test that asserts what those variables resolve to
//! takes the same lock before reading them.

use crate::config::{MIN_FREE_DISK_GB_ENV, MIN_FREE_PHYS_GB_ENV, MIN_FREE_RAM_GB_ENV};

/// Held by every test that sets, or asserts the resolution of, the build-guard
/// floors. A tokio mutex so it may be held across a build's `.await`s.
pub(crate) static BUILD_GUARD_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const FLOORS: [&str; 3] = [
    MIN_FREE_DISK_GB_ENV,
    MIN_FREE_RAM_GB_ENV,
    MIN_FREE_PHYS_GB_ENV,
];

/// The build-guard floors set to `0` (each guard's documented "disabled"
/// value) for as long as this lives; the previous values come back on `Drop`.
pub(crate) struct BuildGuardFloorsOff {
    prior: Vec<(&'static str, Option<String>)>,
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

/// Switch the disk, commit and physical floors off under [`BUILD_GUARD_ENV`].
pub(crate) async fn build_guard_floors_off() -> BuildGuardFloorsOff {
    let lock = BUILD_GUARD_ENV.lock().await;
    let prior = FLOORS
        .iter()
        .map(|name| (*name, std::env::var(name).ok()))
        .collect();
    for name in FLOORS {
        std::env::set_var(name, "0");
    }
    BuildGuardFloorsOff { prior, _lock: lock }
}

impl Drop for BuildGuardFloorsOff {
    fn drop(&mut self) {
        for (name, value) in &self.prior {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }
}
