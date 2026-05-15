//! Row 10 Items 5-7 — synthetic verification.
//!
//! `#[ignore]` by default: these need `git`, `python`, the Wave-0
//! `canonical_diff_hash.py`, and a live `qontinui-canonical-bazel-remote`
//! at `$QONTINUI_BAZEL_REMOTE_URL` (default `http://localhost:9094`).
//! CI boxes without the stack would otherwise fail. Run explicitly:
//!
//! ```text
//! cargo test --test row10_cache_synthetic -- --ignored --nocapture
//! ```
//!
//! Coverage vs the Row 10 done-criteria:
//! * Item 5 — two git worktrees, identical content, different paths ⇒
//!   identical canonical-diff key.
//! * Item 6 — worker A populates AC; worker B gets a cache hit and
//!   materializes the byte-identical artifact in <2s.
//! * Item 7 — the AC body is a real REAPI `ActionResult` protobuf and
//!   round-trips through bazel-remote (with validation re-enabled, the
//!   same PUT must still 2xx — see the deployment doc).

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use qontinui_supervisor::bazel_remote::BazelRemoteClient;
use qontinui_supervisor::build_submissions::BuildKind;
use qontinui_supervisor::cache_key;
use qontinui_supervisor::reapi::{ActionResult, Digest, OutputFile};
use sha2::{Digest as _, Sha256};

fn git(cwd: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("git runs");
    assert!(st.success(), "git {args:?} failed in {cwd:?}");
}

fn runner_script() -> std::path::PathBuf {
    // Sibling of this worktree's parent: qontinui-runner/scripts/.
    // Falls back to the env override the supervisor itself honors.
    if let Ok(p) = std::env::var("QONTINUI_CANONICAL_DIFF_HASH_SCRIPT") {
        return p.into();
    }
    // tests run from the worktree root; the runner repo is a sibling.
    let here = std::env::current_dir().unwrap();
    let parent = here.parent().unwrap_or(&here);
    parent
        .join("qontinui-runner")
        .join("scripts")
        .join("canonical_diff_hash.py")
}

/// Item 5: identical content at two different worktree paths ⇒ one key.
#[tokio::test]
#[ignore]
async fn item5_identical_content_identical_key() {
    let script = runner_script();
    assert!(
        script.exists(),
        "canonical_diff_hash.py not found at {script:?} — set \
         QONTINUI_CANONICAL_DIFF_HASH_SCRIPT"
    );

    let tmp = tempfile::tempdir().unwrap();
    let make_wt = |name: &str| {
        let wt = tmp.path().join(name);
        std::fs::create_dir_all(&wt).unwrap();
        git(&wt, &["init", "-q"]);
        git(&wt, &["config", "user.email", "a@b.c"]);
        git(&wt, &["config", "user.name", "t"]);
        std::fs::write(wt.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join("src/main.rs"), "fn main(){}\n").unwrap();
        git(&wt, &["add", "-A"]);
        git(
            &wt,
            &[
                "-c",
                "user.email=a@b.c",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        // Identical dirty edit in both worktrees.
        std::fs::write(wt.join("src/main.rs"), "fn main(){ println!(\"hi\"); }\n").unwrap();
        wt
    };
    let wt_a = make_wt("agent_A_workspace");
    let wt_b = make_wt("agent_B_workspace");

    let ka = cache_key::compute(&script, &wt_a, None, BuildKind::Build)
        .await
        .expect("key A");
    let kb = cache_key::compute(&script, &wt_b, None, BuildKind::Build)
        .await
        .expect("key B");

    println!("Item 5: A.key={}", ka.key);
    println!("Item 5: B.key={}", kb.key);
    println!("Item 5: A.diff_sha={}", ka.diff_sha);
    assert_eq!(
        ka.diff_sha, kb.diff_sha,
        "identical content must yield identical diff_sha across worktree paths"
    );
    assert_eq!(
        ka.key, kb.key,
        "identical content + toolchain ⇒ identical composed cache key"
    );
    // A profile change must move the key (cross-toolchain guard).
    let ka_rel = cache_key::compute(&script, &wt_a, None, BuildKind::Release)
        .await
        .unwrap();
    assert_ne!(ka.key, ka_rel.key, "release vs dev must not collide");
}

/// Items 6 + 7: A populates CAS+AC (real protobuf); B gets a hit and
/// materializes the byte-identical artifact in <2s.
#[tokio::test]
#[ignore]
async fn item6_7_cache_hit_roundtrip_under_2s() {
    let client = BazelRemoteClient::from_env();
    assert!(client.enabled(), "client disabled via env");

    // Synthetic artifact (stands in for target/release/<bin>) — unique
    // per run so reruns don't false-hit a prior populate.
    let nonce = format!("{}", std::time::SystemTime::now().elapsed().unwrap_or_default().as_nanos());
    let artifact = format!("ROW10-SYNTHETIC-ARTIFACT-{nonce}").into_bytes();
    let artifact_sha = {
        let mut h = Sha256::new();
        h.update(&artifact);
        hex::encode(h.finalize())
    };
    let cache_key_val = {
        let mut h = Sha256::new();
        h.update(format!("row10-synthetic-key-{nonce}").as_bytes());
        hex::encode(h.finalize())
    };

    // ---- Worker A: populate CAS then AC (real ActionResult). -------
    assert!(
        client.cas_put(&artifact_sha, artifact.clone()).await,
        "CAS PUT failed — is bazel-remote up at the configured URL?"
    );
    let ar = ActionResult {
        output_files: vec![OutputFile {
            path: "target/release/synthetic.bin".into(),
            digest: Digest {
                hash: artifact_sha.clone(),
                size_bytes: artifact.len() as i64,
            },
            is_executable: true,
        }],
        exit_code: 0,
    };
    assert!(
        client.ac_put(&cache_key_val, &ar).await,
        "AC PUT failed — with validation ON this means the protobuf was \
         rejected (Item 7 regression)"
    );

    // ---- Worker B: independent lookup + materialize, timed. --------
    let t0 = Instant::now();
    let got = client
        .ac_get(&cache_key_val)
        .await
        .expect("worker B: AC miss on a key A just populated");
    assert_eq!(got.output_files.len(), 1);
    let of = &got.output_files[0];
    assert_eq!(of.digest.hash, artifact_sha);
    assert_eq!(of.digest.size_bytes, artifact.len() as i64);
    assert!(of.is_executable);
    let fetched = client
        .cas_get(&of.digest.hash)
        .await
        .expect("worker B: CAS miss for AC-referenced digest");
    let elapsed = t0.elapsed();

    assert_eq!(
        &fetched[..],
        &artifact[..],
        "worker B fetched a non-byte-identical artifact"
    );
    println!(
        "Item 6/7: cache-hit path (AC get + CAS get) = {:.3}s (budget 2s)",
        elapsed.as_secs_f64()
    );
    assert!(
        elapsed.as_secs_f64() < 2.0,
        "cache-hit path exceeded the 2s done-criteria budget: {:?}",
        elapsed
    );
}
