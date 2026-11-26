//! Public plugin packages are immutable for the duration of a local grant.
pub(crate) mod github;
pub(crate) mod publication;
pub(crate) mod stdio;
pub(crate) mod store;

use crate::mcp::schema;
use hypermesh_local_storage as storage;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fmt, fs,
    io::Read,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use uuid::Uuid;

const HOST_VERSIONS: [u32; 2] = [1, 0];
pub(crate) const MAX_READ_BYTES: u64 = 32 * 1024;
const MAX_EXECUTABLE: u64 = 8 * 1024 * 1024;
const MAX_GRANTS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Package,
    Incompatible,
    InvalidInput,
    InvalidOutput,
    Unavailable,
    Timeout,
    Cancelled,
    ToolFailed,
    Denied,
}

impl Error {
    pub fn code(self) -> &'static str {
        match self {
            Self::Package => "invalid_package",
            Self::Incompatible => "incompatible_api",
            Self::InvalidInput => "invalid_input",
            Self::InvalidOutput => "invalid_output",
            Self::Unavailable => "plugin_unavailable",
            Self::Timeout => "plugin_timeout",
            Self::Cancelled => "plugin_cancelled",
            Self::ToolFailed => "tool_failed",
            Self::Denied => "read_refused",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::Package => "Invalid plugin package or digest",
            Self::Incompatible => "Plugin API incompatible. Use a package supporting API 1 or 0.",
            Self::InvalidInput => "Input does not satisfy the declared tool schema",
            Self::InvalidOutput => "Plugin response does not satisfy the public contract",
            Self::Unavailable => "Plugin sandbox or process unavailable",
            Self::Timeout => "Plugin exceeded its deadline",
            Self::Cancelled => "Plugin invocation was cancelled or its grant ended",
            Self::ToolFailed => "Plugin returned a tool error",
            Self::Denied => "Workspace read admission was unavailable or refused",
        })
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Profile {
    LocalStdio,
    LocalMcpStdio,
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Tool {
    name: String,
    scope: Scope,
    input_schema: Value,
    output_schema: Value,
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
pub(crate) enum Scope {
    #[serde(rename = "workspace.read")]
    WorkspaceRead,
    #[serde(rename = "github.issue.read")]
    GitHubIssueRead,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum Target {
    Workspace(String),
    GitHub(github::Selection),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    manifest_version: u32,
    id: String,
    package_version: String,
    api_versions: Vec<u32>,
    profile: Profile,
    executable_sha256: String,
    tools: Vec<Tool>,
}

pub(crate) struct Package {
    manifest: Manifest,
    digest: String,
    executable: Arc<[u8]>,
    tool: Tool,
    input: jsonschema::Validator,
    output: jsonschema::Validator,
    version: u32,
}

fn slug(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 63
        && text.as_bytes()[0].is_ascii_lowercase()
        && text
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_id(value: &str) -> bool {
    value.len() <= 127 && value.split('.').count() >= 3 && value.split('.').all(slug)
}

fn private_bytes(path: &Path, limit: u64) -> Result<Vec<u8>, Error> {
    let file =
        storage::open(path, storage::Open::Read { private: true }).map_err(|_| Error::Package)?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Package)?;
    if bytes.is_empty() || bytes.len() as u64 > limit {
        return Err(Error::Package);
    }
    Ok(bytes)
}

struct Descriptor {
    manifest: Manifest,
    digest: String,
    version: u32,
}

impl Descriptor {
    fn parse(raw: &[u8], expected: &str) -> Result<Self, Error> {
        if raw.len() > 64 * 1024 {
            return Err(Error::Package);
        }
        let digest = format!("{:x}", Sha256::digest(raw));
        if digest != expected {
            return Err(Error::Package);
        }
        let value: Value = serde_json::from_slice(raw).map_err(|_| Error::Package)?;
        let shape: Value = serde_json::from_str(include_str!("schemas/manifest-v1.json"))
            .map_err(|_| Error::Package)?;
        let shape = schema::compile(&shape).map_err(|_| Error::Package)?;
        schema::validate_compiled(&shape, &value).map_err(|_| Error::Package)?;
        // Deserializing the original bytes also refuses duplicate manifest fields.
        let manifest: Manifest = serde_json::from_slice(raw).map_err(|_| Error::Package)?;
        if manifest.manifest_version != 1 || !valid_id(&manifest.id) {
            return Err(Error::Package);
        }
        let version = HOST_VERSIONS
            .into_iter()
            .find(|v| manifest.api_versions.contains(v))
            .ok_or(Error::Incompatible)?;
        let mut names = HashSet::new();
        for entry in &manifest.tools {
            if !slug(&entry.name) || !names.insert(&entry.name) {
                return Err(Error::Package);
            }
            if entry.scope == Scope::GitHubIssueRead && version == 0 {
                return Err(Error::Incompatible);
            }
        }
        Ok(Self {
            manifest,
            digest,
            version,
        })
    }

    fn check_executable(&self, executable: &[u8]) -> Result<(), Error> {
        if executable.len() as u64 > MAX_EXECUTABLE
            || !executable.starts_with(b"\x7fELF")
            || format!("{:x}", Sha256::digest(executable)) != self.manifest.executable_sha256
        {
            return Err(Error::Package);
        }
        Ok(())
    }

    fn select(self, executable: Vec<u8>, tool: &str) -> Result<Package, Error> {
        let tool = self
            .manifest
            .tools
            .iter()
            .find(|entry| entry.name == tool)
            .ok_or(Error::Package)?
            .clone();
        let input = schema::compile(&tool.input_schema).map_err(|_| Error::Package)?;
        let output = schema::compile(&tool.output_schema).map_err(|_| Error::Package)?;
        Ok(Package {
            manifest: self.manifest,
            digest: self.digest,
            executable: executable.into(),
            tool,
            input,
            output,
            version: self.version,
        })
    }
}

impl Package {
    pub(crate) fn load(path: &Path, expected: &str, tool: &str) -> Result<Self, Error> {
        if !path.is_absolute()
            || path.file_name() != Some("manifest.json".as_ref())
            || !path
                .parent()
                .is_some_and(|parent| storage::check_dir(parent).is_ok())
        {
            return Err(Error::Package);
        }
        let raw = private_bytes(path, 64 * 1024)?;
        let descriptor = Descriptor::parse(&raw, expected)?;
        let executable = private_bytes(&path.with_file_name("plugin"), MAX_EXECUTABLE)?;
        descriptor.check_executable(&executable)?;
        descriptor.select(executable, tool)
    }

    pub(crate) fn key(&self) -> &str {
        &self.manifest.id
    }

    pub(crate) fn allows(&self, target: &Target) -> bool {
        match (&self.tool.scope, target) {
            (Scope::WorkspaceRead, Target::Workspace(path)) => {
                path.len() <= 4096 && Path::new(path).is_absolute()
            }
            (Scope::GitHubIssueRead, Target::GitHub(selection)) => selection.valid(),
            _ => false,
        }
    }

    fn metadata(&self) -> Value {
        json!({
            "id": self.manifest.id,
            "package_version": self.manifest.package_version,
            "manifest_sha256": self.digest,
            "profile": self.manifest.profile,
            "api_version": self.version,
            "tool": self.tool,
        })
    }

    pub(crate) async fn invoke(
        &self,
        staging: &Path,
        input: Value,
        limit: Duration,
        live: impl Fn() -> bool,
    ) -> Result<Value, Error> {
        schema::validate_compiled(&self.input, &input).map_err(|_| Error::InvalidInput)?;
        if !live() {
            return Err(Error::Cancelled);
        }
        let call = stdio::invoke(self, staging, input, limit, &live);
        tokio::pin!(call);
        let mut check = tokio::time::interval(Duration::from_millis(20));
        let deadline = tokio::time::sleep(limit);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                biased;
                _ = &mut deadline => return Err(Error::Timeout),
                _ = check.tick() => if !live() { return Err(Error::Cancelled); },
                value = &mut call => {
                    if !live() { return Err(Error::Cancelled); }
                    let value = value?;
                    schema::validate_compiled(&self.output, &value)
                        .map_err(|_| Error::InvalidOutput)?;
                    return Ok(value);
                }
            }
        }
    }
}

pub(crate) struct Grant {
    pub package: Package,
    pub target: Target,
    ended: AtomicBool,
    running: AtomicBool,
}

impl Grant {
    pub(crate) fn live(&self) -> bool {
        !self.ended.load(Ordering::Acquire)
    }

    pub(crate) fn begin(self: &Arc<Self>) -> Option<Invocation> {
        if !self.live() || self.running.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(Invocation(Arc::clone(self)))
    }
}

pub(crate) struct Invocation(Arc<Grant>);

impl Drop for Invocation {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Release);
    }
}

#[derive(Default)]
pub(crate) struct Grants(HashMap<(Uuid, String), Arc<Grant>>);

impl Grants {
    pub(crate) fn install(
        &mut self,
        session: Uuid,
        package: Package,
        target: Target,
    ) -> Option<Value> {
        let key = (session, package.key().to_owned());
        if self.0.len() >= MAX_GRANTS || self.0.contains_key(&key) {
            return None;
        }
        let metadata = package.metadata();
        self.0.insert(
            key,
            Arc::new(Grant {
                package,
                target,
                ended: AtomicBool::new(false),
                running: AtomicBool::new(false),
            }),
        );
        Some(metadata)
    }

    pub(crate) fn get(&self, session: Uuid, plugin: &str) -> Option<Arc<Grant>> {
        self.0.get(&(session, plugin.to_owned())).cloned()
    }

    pub(crate) fn list(&self, session: Uuid) -> Vec<Value> {
        let mut entries: Vec<_> = self
            .0
            .iter()
            .filter(|((id, _), _)| *id == session)
            .collect();
        entries.sort_unstable_by(|a, b| a.0 .1.cmp(&b.0 .1));
        entries
            .into_iter()
            .map(|(_, grant)| grant.package.metadata())
            .collect()
    }

    pub(crate) fn disable(&mut self, session: Uuid, plugin: &str) -> bool {
        if let Some(grant) = self.0.remove(&(session, plugin.to_owned())) {
            grant.ended.store(true, Ordering::Release);
            return true;
        }
        false
    }

    pub(crate) fn revoke_plugin(&mut self, plugin: &str) {
        self.0.retain(|(_, id), grant| {
            if id == plugin {
                grant.ended.store(true, Ordering::Release);
                return false;
            }
            true
        });
    }

    pub(crate) fn close(&mut self, session: Uuid) {
        self.0.retain(|(id, _), grant| {
            if *id == session {
                grant.ended.store(true, Ordering::Release);
                return false;
            }
            true
        });
    }
}

/// Sign the existing package format using an unencrypted Ed25519 PKCS#8 key.
pub fn build_publication(
    directory: &Path,
    signing_key: &[u8],
    source: &str,
    publisher_domain: Option<&str>,
) -> Result<(Vec<u8>, String), String> {
    publication::sign_package(directory, signing_key, source, publisher_domain)
        .map_err(publication::Error::message)
}

/// Test a package in the production sandbox with caller-supplied fixture data.
/// This entry point has no Visor session, workspace access, or policy credentials.
pub async fn conformance(
    manifest: &Path,
    digest: &str,
    tool: &str,
    staging: &Path,
    input: Value,
) -> Result<Value, Error> {
    storage::check_dir(staging).map_err(|_| Error::Package)?;
    let package = Package::load(manifest, digest, tool)?;
    let result = package
        .invoke(staging, input, Duration::from_secs(5), || true)
        .await?;
    Ok(json!({"api_version": package.version, "result": result}))
}

fn stage_file(staging: &Path, bytes: &[u8]) -> Result<Staged, Error> {
    use std::{io::Write, os::unix::fs::PermissionsExt};
    storage::check_dir(staging).map_err(|_| Error::Unavailable)?;
    let path = staging.join(format!("plugin-{}", Uuid::new_v4()));
    let mut file = storage::open(&path, storage::Open::New).map_err(|_| Error::Unavailable)?;
    let staged = Staged(path);
    file.write_all(bytes).map_err(|_| Error::Unavailable)?;
    file.set_permissions(fs::Permissions::from_mode(0o500))
        .map_err(|_| Error::Unavailable)?;
    Ok(staged)
}

struct Staged(std::path::PathBuf);
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_wire_schema_distinguishes_protocol_versions_and_failure_envelopes() {
        let shape = serde_json::from_str(include_str!("schemas/stdio-v1.json")).unwrap();
        let validator = schema::compile(&shape).unwrap();
        let mut call = json!({"jsonrpc":"2.0", "id":2, "method":"tools/call",
                              "params":{"name":"test","input":{}}});
        assert!(validator.is_valid(&call));
        call["params"]["timeout_ms"] = json!(1000);
        assert!(validator.is_valid(&call));
        call["params"]["timeout_ms"] = json!(0);
        assert!(!validator.is_valid(&call));
        assert!(validator.is_valid(&json!({
            "jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"Invalid input"}
        })));
        assert!(!validator.is_valid(&json!({
            "jsonrpc":"2.0","id":2,"result":{},"error":{"code":-32602,"message":"Invalid input"}
        })));
    }

    #[test]
    fn manifest_digest_and_declared_scope_are_checked_before_loading_executable() {
        struct Temp(std::path::PathBuf);
        impl Drop for Temp {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let dir = Temp(std::env::temp_dir().join(format!("visor-plugin-{}", Uuid::new_v4())));
        storage::create_dir(&dir.0).unwrap();
        let file = dir.0.join("manifest.json");
        let raw = json!({
            "manifest_version":1, "id":"org.example.byte-count", "package_version":"1.0.0",
            "api_versions":[1], "profile":"local-stdio", "executable_sha256":"0".repeat(64),
            "tools":[{"name":"count-bytes","scope":"workspace.write",
                      "input_schema":{"type":"object"},"output_schema":{"type":"object"}}]
        })
        .to_string();
        use std::io::Write;
        storage::open(&file, storage::Open::New)
            .unwrap()
            .write_all(raw.as_bytes())
            .unwrap();
        let digest = format!("{:x}", Sha256::digest(raw.as_bytes()));
        assert!(matches!(
            Package::load(&file, &digest, "count-bytes"),
            Err(Error::Package)
        ));
        assert!(matches!(
            Package::load(&file, "not a digest", "count-bytes"),
            Err(Error::Package)
        ));
    }
}
