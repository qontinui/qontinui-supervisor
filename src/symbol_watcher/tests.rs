//! Integration-level tests that exercise [`SymbolWatcher`] end-to-end
//! via the `MockTransport` (no coord process required).

use super::coord_client::{ClaimRequestWire, MockTransport};
use super::file_watch::SaveEvent;
use super::{find_repo_root, make_resource_key, SymbolWatcher, WatcherConfig};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn write_file(dir: &tempfile::TempDir, name: &str, contents: &str) -> PathBuf {
    let path = dir.path().join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, contents).unwrap();
    path
}

fn make_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    dir
}

fn make_watcher(mock: MockTransport) -> Arc<SymbolWatcher> {
    let config = WatcherConfig::build(vec![PathBuf::from("/tmp")], Some(1));
    let watcher = SymbolWatcher::with_transport(config, "test-machine".to_string(), Arc::new(mock));
    Arc::new(watcher)
}

#[tokio::test]
async fn first_save_emits_acquire_for_every_symbol() {
    let mock = MockTransport::default();
    let acquires = mock.acquires.clone();
    let watcher = make_watcher(mock);

    let repo = make_repo();
    let path = write_file(&repo, "src/main.rs", "fn foo() {}\nfn bar() {}\n");

    watcher
        .handle_event(SaveEvent {
            path: path.clone(),
            extension: "rs".to_string(),
        })
        .await;

    let acquires = acquires.lock().unwrap().clone();
    assert_eq!(acquires.len(), 2, "expected 2 acquires, got {acquires:?}");
    // The kind is always "symbol".
    for r in &acquires {
        assert_eq!(r.kind, "symbol");
        assert_eq!(r.machine_id, "test-machine");
        assert!(
            r.resource_key.contains(":src/main.rs:"),
            "got {}",
            r.resource_key
        );
    }
    let keys: Vec<&str> = acquires.iter().map(|r| r.resource_key.as_str()).collect();
    assert!(keys.iter().any(|k| k.ends_with(":foo")));
    assert!(keys.iter().any(|k| k.ends_with(":bar")));
}

#[tokio::test]
async fn modified_symbol_re_acquires() {
    let mock = MockTransport::default();
    let acquires = mock.acquires.clone();
    let watcher = make_watcher(mock);

    let repo = make_repo();
    let path = write_file(&repo, "src/main.rs", "fn foo() {}\n");

    watcher
        .handle_event(SaveEvent {
            path: path.clone(),
            extension: "rs".to_string(),
        })
        .await;
    let count_after_first = acquires.lock().unwrap().len();

    // Bump `foo` down two lines — same name, new start_line, so it
    // counts as `modified`.
    std::fs::write(&path, "\n\nfn foo() {}\n").unwrap();
    watcher
        .handle_event(SaveEvent {
            path: path.clone(),
            extension: "rs".to_string(),
        })
        .await;
    let count_after_second = acquires.lock().unwrap().len();

    assert_eq!(count_after_first, 1);
    assert_eq!(count_after_second, 2);
    // Both acquires target the same resource key.
    let keys: Vec<String> = acquires
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.resource_key.clone())
        .collect();
    assert!(keys[0].ends_with(":foo"), "got {:?}", keys);
    assert_eq!(keys[0], keys[1], "second acquire same key as first");
}

#[tokio::test]
async fn unchanged_symbol_no_traffic() {
    let mock = MockTransport::default();
    let acquires = mock.acquires.clone();
    let releases = mock.releases.clone();
    let watcher = make_watcher(mock);

    let repo = make_repo();
    let path = write_file(&repo, "src/main.rs", "fn foo() {}\n");

    watcher
        .handle_event(SaveEvent {
            path: path.clone(),
            extension: "rs".to_string(),
        })
        .await;
    // Second event with identical content → no diff, no calls.
    watcher
        .handle_event(SaveEvent {
            path: path.clone(),
            extension: "rs".to_string(),
        })
        .await;

    assert_eq!(acquires.lock().unwrap().len(), 1);
    assert_eq!(releases.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn removed_symbol_emits_release() {
    let mock = MockTransport::default();
    let releases = mock.releases.clone();
    let watcher = make_watcher(mock);

    let repo = make_repo();
    let path = write_file(&repo, "src/main.rs", "fn foo() {}\nfn bar() {}\n");

    watcher
        .handle_event(SaveEvent {
            path: path.clone(),
            extension: "rs".to_string(),
        })
        .await;

    // Remove `bar`.
    std::fs::write(&path, "fn foo() {}\n").unwrap();
    watcher
        .handle_event(SaveEvent {
            path: path.clone(),
            extension: "rs".to_string(),
        })
        .await;

    let rels = releases.lock().unwrap().clone();
    assert_eq!(rels.len(), 1, "expected 1 release for bar, got {rels:?}");
    assert!(rels[0].resource_key.ends_with(":bar"));
}

#[tokio::test]
async fn file_deletion_releases_all_prior_claims() {
    let mock = MockTransport::default();
    let releases = mock.releases.clone();
    let watcher = make_watcher(mock);

    let repo = make_repo();
    let path = write_file(&repo, "src/main.rs", "fn foo() {}\nfn bar() {}\n");

    watcher
        .handle_event(SaveEvent {
            path: path.clone(),
            extension: "rs".to_string(),
        })
        .await;

    // Delete the file. handle_event should detect the read failure and
    // release the prior claims.
    std::fs::remove_file(&path).unwrap();
    watcher
        .handle_event(SaveEvent {
            path: path.clone(),
            extension: "rs".to_string(),
        })
        .await;

    let rels = releases.lock().unwrap().clone();
    assert_eq!(rels.len(), 2, "got {rels:?}");
}

#[tokio::test]
async fn idle_sweeper_releases_stale_claims() {
    let mock = MockTransport::default();
    let releases = mock.releases.clone();
    // 100ms idle TTL so the test doesn't drag.
    let mut config = WatcherConfig::build(vec![PathBuf::from("/tmp")], Some(1));
    config.idle_ttl = Duration::from_millis(100);
    let watcher = Arc::new(SymbolWatcher::with_transport(
        config,
        "test-machine".to_string(),
        Arc::new(mock),
    ));

    let repo = make_repo();
    let path = write_file(&repo, "src/main.rs", "fn foo() {}\n");

    watcher
        .handle_event(SaveEvent {
            path: path.clone(),
            extension: "rs".to_string(),
        })
        .await;
    assert_eq!(watcher.active_keys().await.len(), 1);

    // Wait past the TTL and run the sweeper directly.
    tokio::time::sleep(Duration::from_millis(200)).await;
    watcher.sweep_idle().await;

    assert_eq!(watcher.active_keys().await.len(), 0);
    assert_eq!(releases.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn typescript_path_only_fires_for_exported() {
    let mock = MockTransport::default();
    let acquires = mock.acquires.clone();
    let watcher = make_watcher(mock);

    let repo = make_repo();
    let path = write_file(
        &repo,
        "src/index.ts",
        "export function foo() {}\nfunction bar() {}\n",
    );

    watcher
        .handle_event(SaveEvent {
            path: path.clone(),
            extension: "ts".to_string(),
        })
        .await;

    let acquires = acquires.lock().unwrap().clone();
    assert_eq!(
        acquires.len(),
        1,
        "expected 1 (exported foo only), got {acquires:?}"
    );
    assert!(acquires[0].resource_key.ends_with(":foo"));
}

#[tokio::test]
async fn python_path_extracts_class_methods() {
    let mock = MockTransport::default();
    let acquires = mock.acquires.clone();
    let watcher = make_watcher(mock);

    let repo = make_repo();
    let path = write_file(
        &repo,
        "pkg/m.py",
        "class A:\n    def x(self): pass\n    def y(self): pass\n",
    );

    watcher
        .handle_event(SaveEvent {
            path: path.clone(),
            extension: "py".to_string(),
        })
        .await;

    let keys: Vec<String> = acquires
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.resource_key.clone())
        .collect();
    assert!(keys.iter().any(|k| k.ends_with(":A")), "got {keys:?}");
    assert!(keys.iter().any(|k| k.ends_with(":A.x")), "got {keys:?}");
    assert!(keys.iter().any(|k| k.ends_with(":A.y")), "got {keys:?}");
}

#[tokio::test]
async fn resource_key_uses_forward_slashes_on_all_platforms() {
    // Direct unit test of the helper — sanity check for cross-platform
    // resource_key stability.
    let key = make_resource_key("qontinui-coord", "src/sub/main.rs", "foo");
    assert_eq!(key, "qontinui-coord:src/sub/main.rs:foo");
}

#[test]
fn find_repo_root_walks_to_dot_git() {
    let dir = make_repo();
    let nested = dir.path().join("a").join("b");
    std::fs::create_dir_all(&nested).unwrap();
    let leaf = nested.join("c.rs");
    std::fs::write(&leaf, "fn f() {}\n").unwrap();
    let found = find_repo_root(&leaf).unwrap();
    // Compare by tail filename rather than canonical path because
    // Windows canonicalize() returns `\\?\` verbatim paths that diverge
    // from `dir.path()` even though they reference the same FS node.
    assert_eq!(found.file_name(), dir.path().file_name());
    // And verify .git is reachable from the discovered root.
    assert!(found.join(".git").exists(), "got {found:?}");
}

#[test]
fn watcher_config_env_override() {
    // Use a process-wide guard pattern (no other tests touch this env var
    // — these only ever set it to test values during the test body).
    std::env::set_var("QONTINUI_COORD_URL", "http://example.test:1234");
    let cfg = WatcherConfig::build(vec![PathBuf::from(".")], None);
    assert_eq!(cfg.coord_url, "http://example.test:1234");
    std::env::remove_var("QONTINUI_COORD_URL");
}

#[test]
fn claim_request_for_symbol_serializes_to_coord_wire() {
    let req = ClaimRequestWire::for_symbol(
        "abc",
        "qontinui-supervisor:src/main.rs:foo",
        serde_json::json!({"file": "src/main.rs"}),
    );
    let s = serde_json::to_value(&req).unwrap();
    // Coord rejects non-snake_case `kind`.
    assert_eq!(s["kind"], "symbol");
    assert_eq!(s["machine_id"], "abc");
    assert_eq!(s["resource_key"], "qontinui-supervisor:src/main.rs:foo");
    assert_eq!(s["ttl_seconds"], 300);
}
