//! Exact published pack bytes shared by the desktop catalog contracts.

use std::path::Path;
use std::process::Command;

pub const SOURCE: &str = "12e6bc4dcf8a7daa8a079f409865a7e03510040a";
pub const ASSETS: &str = "16e0a37d2e287a8766452355fad1742c2389d2b1";
const FIXTURE: &str = "8940a44fb026224b563b0a732fbc69920f54dc4b";

pub fn read_fixture(root: &Path, path: &str, limit: usize) -> Result<Vec<u8>, String> {
    let git_path = if path == "catalog/catalog.json" {
        "catalog.json"
    } else {
        path.strip_prefix(ASSETS)
            .and_then(|path| path.strip_prefix('/'))
            .filter(|path| path.starts_with("packs/"))
            .ok_or_else(|| format!("unexpected fixture request: {path}"))?
    };
    // The HTTP publisher serves Git blobs. Checkout filters can add a CR on
    // Windows and change the size and digest of the JSON archives.
    let blob = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "blob", &format!("{FIXTURE}:{git_path}")])
        .output()
        .map_err(|error| format!("reading catalog blob {git_path}: {error}"))?;
    if !blob.status.success() {
        return Err(format!(
            "catalog blob {git_path}: {}",
            String::from_utf8_lossy(&blob.stderr)
        ));
    }
    if blob.stdout.len() > limit {
        return Err(format!("fixture response exceeds the limit: {path}"));
    }
    Ok(blob.stdout)
}
