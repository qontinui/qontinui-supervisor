//! Tests for the supervisor's Spec API.
//!
//! Coverage (mirrors the runner's spec_api test suite):
//! - Projection determinism + key sorting on a small fixture
//! - Projection shape matches the worked example
//! - Storage round-trip on a tempdir
//! - Path traversal rejected
//! - `/spec/health` returns the reason envelope
//! - SSE: emit -> subscriber receives a `spec.changed` event

use serde_json::{json, Value};

use super::projection::project_ir_to_bundled_page;
use super::storage::{self, ReadWithinRootError};
use super::types::IrPageSpec;

fn small_doc() -> IrPageSpec {
    let raw = json!({
        "version": "1.0",
        "id": "active",
        "name": "Active Dashboard",
        "description": "Real-time monitoring hub",
        "metadata": { "tags": ["monitoring", "tier-1"] },
        "states": [
            {
                "id": "running",
                "name": "Running",
                "description": "Workflow executing",
                "assertions": [
                    {
                        "id": "running::req[0]",
                        "description": "Required element 0 for state Running",
                        "category": "element-presence",
                        "severity": "critical",
                        "assertionType": "exists",
                        "target": {
                            "type": "search",
                            "criteria": { "role": "button", "text": "Stop" },
                            "label": "Required element for Running"
                        },
                        "source": "test-fixture",
                        "reviewed": false,
                        "enabled": true
                    }
                ],
                "isInitial": false
            }
        ],
        "transitions": [
            {
                "id": "running-to-idle",
                "name": "Stop",
                "fromStates": ["running"],
                "activateStates": ["idle"],
                "exitStates": ["running"],
                "actions": [
                    {
                        "type": "click",
                        "target": { "role": "button", "text": "Stop" },
                        "waitAfter": { "type": "idle", "timeout": 3000 }
                    }
                ]
            }
        ],
        "initialState": "idle"
    });
    serde_json::from_value(raw).unwrap()
}

#[test]
fn projection_known_fixture_is_byte_stable() {
    let doc = small_doc();
    let v1 = project_ir_to_bundled_page(&doc, Some("Notes"));
    let v2 = project_ir_to_bundled_page(&doc, Some("Notes"));
    let s1 = serde_json::to_string(&v1).unwrap();
    let s2 = serde_json::to_string(&v2).unwrap();
    assert_eq!(s1, s2, "projection must be deterministic");
}

#[test]
fn projection_matches_known_shape() {
    let doc = small_doc();
    let v = project_ir_to_bundled_page(&doc, Some("Notes"));
    assert_eq!(v["version"], "1.0.0");
    assert_eq!(v["description"], "Real-time monitoring hub\n\nNotes");
    assert_eq!(v["metadata"]["component"], "active");
    assert_eq!(v["metadata"]["tags"][0], "monitoring");

    let groups = v["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["id"], "running");
    let assertions = groups[0]["assertions"].as_array().unwrap();
    assert_eq!(assertions.len(), 1);
    assert_eq!(assertions[0]["id"], "running-elem-0");
    assert_eq!(assertions[0]["target"]["criteria"]["role"], "button");
    assert_eq!(assertions[0]["target"]["criteria"]["textContent"], "Stop");

    let sm_states = v["stateMachine"]["states"].as_array().unwrap();
    assert_eq!(sm_states.len(), 1);
    assert_eq!(sm_states[0]["id"], "running");
    assert_eq!(sm_states[0]["isInitial"], false);
    let transitions = sm_states[0]["transitions"].as_array().unwrap();
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0]["id"], "running-to-idle");
    assert_eq!(transitions[0]["activateStates"][0], "idle");
    assert_eq!(transitions[0]["deactivateStates"][0], "running");
    assert_eq!(transitions[0]["staysVisible"], false);
    assert_eq!(transitions[0]["process"][0]["action"], "click");
    assert_eq!(
        transitions[0]["process"][0]["target"]["textContent"],
        "Stop"
    );
}

#[test]
fn projection_keys_are_sorted() {
    fn walk(v: &Value) {
        match v {
            Value::Object(map) => {
                let keys: Vec<&String> = map.keys().collect();
                let mut sorted = keys.clone();
                sorted.sort();
                assert_eq!(keys, sorted, "object keys not sorted: {:?}", keys);
                for child in map.values() {
                    walk(child);
                }
            }
            Value::Array(arr) => {
                for child in arr {
                    walk(child);
                }
            }
            _ => {}
        }
    }
    let doc = small_doc();
    let v = project_ir_to_bundled_page(&doc, None);
    let s = serde_json::to_string(&v).unwrap();
    let reparsed: Value = serde_json::from_str(&s).unwrap();
    walk(&reparsed);
}

#[test]
fn storage_round_trip_in_tempdir() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let doc = small_doc();
    let projection_path = storage::write_ir_and_regenerate(root, &doc).unwrap();
    assert!(projection_path.exists());
    let read_back = storage::read_ir(root, "active").unwrap().unwrap();
    assert_eq!(read_back.id, "active");
    assert_eq!(read_back.states.len(), 1);
    let proj = storage::read_projection(root, "active").unwrap().unwrap();
    assert_eq!(proj["version"], "1.0.0");
    let pages = storage::list_pages(root).unwrap();
    assert_eq!(pages, vec!["active".to_string()]);
}

#[test]
fn storage_path_traversal_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("inside.txt"), b"ok").unwrap();
    let outer_dir = tmp.path().parent().unwrap();
    let secret = outer_dir.join("supervisor_spec_secret_outside.txt");
    std::fs::write(&secret, b"nope").unwrap();
    let inside = storage::read_within_root(root, "inside.txt").unwrap();
    assert_eq!(inside, b"ok");
    let traversed = storage::read_within_root(root, "../supervisor_spec_secret_outside.txt");
    assert!(matches!(
        traversed,
        Err(ReadWithinRootError::OutsideRoot) | Err(ReadWithinRootError::FileNotFound)
    ));
    if outer_dir.canonicalize().is_ok() {
        let traversed_explicit =
            storage::read_within_root(root, "../supervisor_spec_secret_outside.txt").unwrap_err();
        assert_eq!(traversed_explicit, ReadWithinRootError::OutsideRoot);
    }
    let _ = std::fs::remove_file(&secret);
}

// =========================================================================
// Handler tests via tower::ServiceExt::oneshot.
// =========================================================================

#[cfg(test)]
mod handler_tests {
    use super::super::events;
    use super::super::handlers;

    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use serde_json::Value;
    use tower::ServiceExt;

    fn router_for_tests() -> Router {
        // Only the `/spec/health` handler is stateless. The other handlers
        // take `State<SharedState>` but their actual logic doesn't read
        // state contents — they read the filesystem. To exercise them in
        // tests we'd have to construct a real SupervisorState (which is
        // heavy). Instead, like the runner's port, we test storage paths
        // directly and exercise health via the tower service harness.
        Router::new().route("/spec/health", get(handlers::get_health))
    }

    #[tokio::test]
    async fn health_returns_reason() {
        let app = router_for_tests();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/spec/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["reason"], "spec-api-mounted");
    }

    #[tokio::test]
    async fn sse_receives_emitted_event() {
        let mut rx = events::subscribe();
        events::emit(events::SpecChanged {
            page_id: "active".to_string(),
            kind: "ir-and-projection".to_string(),
            at_ms: events::now_ms(),
        });
        let received = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("subscriber should receive within timeout")
            .expect("event must arrive");
        assert_eq!(received.page_id, "active");
        assert_eq!(received.kind, "ir-and-projection");
    }

    #[tokio::test]
    async fn projection_via_handler_storage_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var(
            "QONTINUI_SPECS_ROOT",
            tmp.path().to_string_lossy().to_string(),
        );
        let _guard = scopeguard::guard((), |_| {
            std::env::remove_var("QONTINUI_SPECS_ROOT");
        });
        let root = super::super::storage::resolve_specs_root();
        assert!(root.starts_with(tmp.path()) || root == tmp.path());

        let doc = super::small_doc();
        super::super::storage::write_ir_and_regenerate(&root, &doc).unwrap();

        let projection = super::super::storage::read_projection(&root, "active")
            .unwrap()
            .expect("projection must exist");
        assert_eq!(projection["version"], "1.0.0");

        let missing = super::super::storage::read_projection(&root, "nonexistent").unwrap();
        assert!(missing.is_none());
    }
}
