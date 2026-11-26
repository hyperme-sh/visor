//! Exact published pack bytes shared by the desktop catalog contracts.

use std::io::{self, Read};
use std::path::Path;
use std::process::Command;

use serde_json::json;

use super::read_download;

pub const SOURCE: &str = "12e6bc4dcf8a7daa8a079f409865a7e03510040a";
pub const ASSETS: &str = "16e0a37d2e287a8766452355fad1742c2389d2b1";
const FIXTURE: &str = "8940a44fb026224b563b0a732fbc69920f54dc4b";
const INDEX_PATH: &str = "catalog/catalog.json";

#[derive(Clone, Copy, Debug)]
pub enum CatalogFault {
    Unavailable,
    TruncatedJson,
    InvalidRevision,
    InvalidListing,
    DuplicateListing,
}

impl CatalogFault {
    pub const CASES: [Self; 5] = [
        Self::Unavailable,
        Self::TruncatedJson,
        Self::InvalidRevision,
        Self::InvalidListing,
        Self::DuplicateListing,
    ];

    pub fn expected_error(self) -> &'static str {
        match self {
            Self::Unavailable => "could not be downloaded",
            Self::TruncatedJson => "Invalid catalog response",
            Self::InvalidRevision => "Unsupported catalog or invalid revision",
            Self::InvalidListing => "Invalid companion pack listing",
            Self::DuplicateListing => "duplicate pack",
        }
    }

    pub fn read(self, root: &Path, path: &str, limit: usize) -> Result<Vec<u8>, String> {
        assert_eq!(
            path, INDEX_PATH,
            "catalog fault must not trigger an asset request"
        );
        if matches!(self, Self::Unavailable) {
            return Err("The community catalog or pack could not be downloaded.".into());
        }
        let body = read_fixture(root, path, limit)?;
        if matches!(self, Self::TruncatedJson) {
            return DownloadFault::Truncated.read(&body, limit);
        }
        let mut index: serde_json::Value = serde_json::from_slice(&body).unwrap();
        match self {
            Self::InvalidRevision => index["asset_revision"] = json!("bad-revision"),
            Self::InvalidListing => index["packs"][0]["archive"]["sha256"] = json!("bad-digest"),
            Self::DuplicateListing => {
                let packs = index["packs"].as_array_mut().unwrap();
                packs.push(packs[0].clone());
            }
            Self::Unavailable | Self::TruncatedJson => unreachable!(),
        }
        Ok(serde_json::to_vec(&index).unwrap())
    }
}

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
    let git_path = if path == INDEX_PATH {
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
