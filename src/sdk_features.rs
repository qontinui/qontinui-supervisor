//! SDK feature inventory — re-exported from `qontinui-types::sdk_features`.
//!
//! The constants below are defined in
//! `/d/qontinui-root/qontinui-schemas/rust/src/sdk_features.rs` and re-exported
//! here so existing call sites (`crate::sdk_features::SDK_FEATURES`,
//! `crate::sdk_features::SDK_FEATURE_DOC_URL`) keep compiling unchanged. Edit
//! the shared module in qontinui-schemas when adding a new feature flag —
//! both the runner and the supervisor pick up the change automatically on
//! rebuild.
//!
//! Surfaced on `/health` and `/supervisor-bridge/health` as the
//! `sdkFeatures` array. Test drivers compare against the features they
//! need; an absent feature means the binary predates that feature's SDK
//! release.

pub use qontinui_types::sdk_features::{SDK_FEATURES, SDK_FEATURE_DOC_URL};
