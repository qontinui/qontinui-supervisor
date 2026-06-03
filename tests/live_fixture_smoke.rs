//! Live-fixture smoke test for the `is_primary` -> `kind` migrator.
//!
//! Phase 5 of the runnerkind-settings-migration plan requires verifying that
//! the developer's actual on-disk `D:\qontinui-root\.dev-logs\supervisor-settings.json`
//! migrates cleanly. Rather than mutating the live file, we copy it to a
//! tempfile and exercise `load_settings` there.
//!
//! Skipped if the live fixture isn't present (e.g. CI) or is already in
//! post-migration shape (any real supervisor start since the migrator
//! shipped rewrites the live file).

use qontinui_supervisor::settings::load_settings;
use qontinui_types::wire::runner_kind::RunnerKind;
use std::path::Path;

#[test]
fn live_fixture_migrates_and_is_idempotent() {
    let live_path = Path::new(r"D:\qontinui-root\.dev-logs\supervisor-settings.json");
    if !live_path.exists() {
        eprintln!("live fixture missing at {live_path:?}; skipping smoke test");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let temp_path = dir.path().join("supervisor-settings.json");
    std::fs::copy(live_path, &temp_path).unwrap();

    // Snapshot the original raw bytes so we can verify the migrator only
    // rewrites once.
    let before = std::fs::read_to_string(&temp_path).unwrap();

    // One-time Phase 5 smoke: only meaningful while the live file is still
    // in pre-migration shape. Any real supervisor start since the migrator
    // shipped rewrites the live file to post-shape — that's the migrator
    // working, not a regression — so skip (like the missing-file case
    // above) instead of failing every local `cargo test` forever. Re-seed
    // the live file from git history to re-exercise the migration path.
    if !before.contains("\"is_primary\"") {
        eprintln!(
            "live fixture at {live_path:?} is already post-migration shape \
             (no is_primary keys); skipping smoke test"
        );
        return;
    }

    // First load: triggers the migrator.
    let settings = load_settings(&temp_path);

    // The on-disk file must now be migrated.
    let after_first = std::fs::read_to_string(&temp_path).unwrap();
    assert!(
        !after_first.contains("\"is_primary\""),
        "after first load_settings the file should have no is_primary keys, got:\n{after_first}"
    );
    assert!(
        after_first.contains("\"kind\""),
        "after first load_settings every runner entry should have a `kind` field, got:\n{after_first}"
    );

    // Validate variants on the deserialized settings.
    assert_eq!(
        settings.runners.len(),
        2,
        "live fixture is expected to have 2 runners (primary + named-9879)"
    );
    let primary = settings
        .runners
        .iter()
        .find(|r| r.id == "primary")
        .expect("primary runner present");
    assert!(
        matches!(primary.kind(), RunnerKind::Primary),
        "primary entry should classify as RunnerKind::Primary, got {:?}",
        primary.kind()
    );

    let named = settings
        .runners
        .iter()
        .find(|r| r.id.starts_with("named-9879"))
        .expect("named-9879 runner present");
    match named.kind() {
        RunnerKind::Named { name } => assert_eq!(name, "target", "named runner display name"),
        other => panic!("named-9879 entry should classify as Named, got {:?}", other),
    }

    // Idempotency check: second load must not rewrite the file.
    let mtime_first = std::fs::metadata(&temp_path).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let _settings_again = load_settings(&temp_path);
    let after_second = std::fs::read_to_string(&temp_path).unwrap();
    let mtime_second = std::fs::metadata(&temp_path).unwrap().modified().unwrap();
    assert_eq!(
        after_first, after_second,
        "second load must not change file contents"
    );
    assert_eq!(
        mtime_first, mtime_second,
        "second load must not touch the file's mtime (idempotent no-op path)"
    );
}
