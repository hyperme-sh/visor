//! Exact published pack bytes shared by the desktop catalog contracts.

use std::io::{self, Read};
use std::path::Path;
use std::process::Command;

use super::read_download;

pub const SOURCE: &str = "12e6bc4dcf8a7daa8a079f409865a7e03510040a";
pub const ASSETS: &str = "16e0a37d2e287a8766452355fad1742c2389d2b1";
const FIXTURE: &str = "8940a44fb026224b563b0a732fbc69920f54dc4b";

#[derive(Clone, Copy)]
pub enum DownloadFault {
    Reset,
    Truncated,
}

impl DownloadFault {
    pub const CASES: [Self; 2] = [Self::Reset, Self::Truncated];

    pub fn expected_error(self) -> &'static str {
        match self {
            Self::Reset => "download was interrupted",
            Self::Truncated => "size or SHA-256",
        }
    }

    pub fn read(self, bytes: &[u8], limit: usize) -> Result<Vec<u8>, String> {
        let prefix = &bytes[..bytes.len() / 2];
        match self {
            Self::Reset => read_download(prefix.chain(ConnectionReset), limit),
            Self::Truncated => read_download(prefix, limit),
        }
    }
}

struct ConnectionReset;

impl Read for ConnectionReset {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::ErrorKind::ConnectionReset.into())
    }
}

pub fn read_with_fault(
    root: &Path,
    path: &str,
    limit: usize,
    fault: Option<(&str, DownloadFault)>,
) -> Result<Vec<u8>, String> {
    let bytes = read_fixture(root, path, limit)?;
    match fault {
        Some((filename, fault)) if path.rsplit('/').next() == Some(filename) => {
            fault.read(&bytes, limit)
        }
        _ => Ok(bytes),
    }
}

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
