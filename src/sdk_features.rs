//! SDK feature inventory baked at compile time.
//!
//! Lists the `@qontinui/ui-bridge` primitives this supervisor binary's
//! embedded frontend is known to support. **Bump when a new SDK feature
//! lands** — the staleness detection only works if this list mirrors the
//! SDK's actual capabilities at build time.
//!
//! Surfaced on `/health` and `/supervisor-bridge/health` as the
//! `sdkFeatures` array. Test drivers compare against the features they
//! need; an absent feature means the binary predates that feature's SDK
//! release.
//!
//! **Mixed-category flags.** Entries here cover both transport-level
//! primitives (e.g. `softNavigate`, `tabActivation`, `flatErrorEnvelope`)
//! AND data-shape contracts the host emits in its responses
//! (e.g. `snapshotF3`, `snapshotCanonicalElements`). Test drivers can
//! `sdkFeatures.includes("snapshotF3")` to feature-detect the snapshot
//! shape instead of probing field presence. See [`SDK_FEATURE_DOC_URL`]
//! for the canonical reference of every flag.

pub const SDK_FEATURES: &[&str] = &[
    // F1 (2026-04-25) — soft vs hard navigate, snapshot activeTab field
    "softNavigate",
    "snapshotActiveTab",
    // F3 (2026-04-25) — snapshot registration metadata
    "snapshotRegistration",
    // F4 (2026-04-23) — runner tab activation (runner-only, listed for symmetry)
    "tabActivation",
    // F2 (2026-04-22 + sweep 2026-04-25) — flat HTTP 400 envelope on soft failures
    "flatErrorEnvelope",
    // B1+M2 (2026-04-25) — action-driven registry value overlay
    "actionOverlay",
    // B2 (2026-04-25) — module-level bookmark singleton
    "bookmarksSingleton",
    // B3 (2026-04-25) — ai/find broadened scoring (label/aria/placeholder/name)
    "findBroadened",
    // M1 (2026-04-25) — /ai/wait-for-element with state predicates
    "waitForElement",
    // F2 stubs (2026-04-23) — fetch stub registry + verify
    "stubRegistry",
    "stubVerify",
    // Discoverability (2026-04-25, commit 7e3172928)
    "pagePlaybook",
    "snapshotAvailableTabs",
    "componentTree",
    "errorClosestMatches",
    "frontendReadyFlag",
    // Snapshot-shape contracts (data-shape, not transport-level).
    // F3 metadata in snapshot envelope: registration{totalRegistered,
    // everHadRegistrations, byRoute} + route + snapshotTakenAtMs.
    // Added 2026-04-24 (ui-bridge commit d50ce72); full coverage
    // 2026-04-25 (a8a4bb4 patched the relay handler).
    "snapshotF3",
    // Snapshot elements use the canonical SDK serialization (bbox,
    // identifier, tagName, stableRef, kind, category, visible, origin,
    // route) rather than the legacy minimal {id, type, label, actions,
    // state} shape. Added 2026-04-26 via the Phase 1+6 audit fix.
    "snapshotCanonicalElements",
];

pub const SDK_FEATURE_DOC_URL: &str =
    "https://github.com/qontinui/ui-bridge/blob/main/docs-site/docs/api/runner-features.md";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_features_non_empty_and_contains_known_anchors() {
        assert!(!SDK_FEATURES.is_empty(), "SDK_FEATURES must not be empty");
        for required in ["softNavigate", "snapshotRegistration", "actionOverlay"] {
            assert!(
                SDK_FEATURES.contains(&required),
                "SDK_FEATURES must contain {required:?}"
            );
        }
    }

    #[test]
    fn sdk_feature_doc_url_is_https() {
        assert!(
            SDK_FEATURE_DOC_URL.starts_with("https://"),
            "SDK_FEATURE_DOC_URL must be HTTPS"
        );
    }
}
