//! Crash-safe atomic file writes used by both the spec_api storage layer
//! and the persistent settings migrator.
use std::fs;
use std::io::Write;
use std::path::Path;

/// Atomic write: write to `<target>.tmp` then rename. Cleans up the tmp
/// file on failure. Caller is responsible for parent dir existence.
pub(crate) fn atomic_write(target: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension(format!(
        "{}.tmp",
        target
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("write")
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, target).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}
