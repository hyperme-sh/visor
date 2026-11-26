//! Verify a signed publication without executing its code or granting a tool.
use super::{private_bytes, schema, slug, Descriptor, Error as PackageError, Package, Tool};
use base64::{engine::general_purpose::STANDARD, Engine};
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use url::{Host, Url};

pub(super) const MAX_ARTIFACT: u64 = 12 * 1024 * 1024;
const PROOF_PATH: &str = "/.well-known/visor-plugin-publisher.json";
pub(crate) const SIGNATURE_DOMAIN: &[u8] = b"visor-plugin-publication-v1\0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Package(PackageError),
    Source,
    Signature,
    Publisher,
    Changed,
    Missing,
    Disabled,
    Bundled,
    Storage,
}

impl Error {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Package(e) => e.code(),
            Self::Source => "publication_unavailable",
            Self::Signature => "invalid_signature",
            Self::Publisher => "publisher_not_authorized",
            Self::Changed => "publication_changed",
            Self::Missing => "plugin_missing",
            Self::Disabled => "plugin_disabled",
            Self::Bundled => "bundled_plugin_immutable",
            Self::Storage => "plugin_store_unavailable",
        }
    }

    pub(crate) fn message(self) -> String {
        match self {
            Self::Package(e) => e.to_string(),
            Self::Source => "Could not read the selected publication source".into(),
            Self::Signature => "The publication signature or contents could not be verified".into(),
            Self::Publisher => "Publisher identity could not be verified for this namespace".into(),
            Self::Changed => {
                "The publication or installed revision changed. Inspect it again.".into()
            }
            Self::Missing => "The plugin is not installed".into(),
            Self::Disabled => "The installed plugin is disabled".into(),
            Self::Bundled => "This plugin is supplied by the Visor image".into(),
            Self::Storage => "The private plugin store is unavailable".into(),
        }
    }
}

impl From<PackageError> for Error {
    fn from(value: PackageError) -> Self {
        Self::Package(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Verification {
    Community,
    AppOwnerVerified,
    FirstParty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Publisher {
    verification: Verification,
    domain: Option<String>,
    key_sha256: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Metadata {
    pub(super) metadata_version: u32,
    pub(crate) id: String,
    pub(crate) package_version: String,
    pub(crate) publication_sha256: String,
    pub(crate) manifest_sha256: String,
    pub(crate) publisher: Publisher,
    source: String,
    api_version: u32,
    scan_status: ScanStatus,
    requested_tools: Vec<Tool>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScanStatus {
    NotScanned,
}

impl Metadata {
    pub(crate) fn added_access(&self, previous: Option<&Metadata>) -> Vec<&str> {
        self.requested_tools
            .iter()
            .filter(|tool| previous.is_none_or(|before| !before.requested_tools.contains(tool)))
            .map(|tool| tool.name.as_str())
            .collect()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    statement: String,
    signature: String,
    manifest: String,
    executable: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Statement {
    version: u32,
    plugin_id: String,
    package_version: String,
    source: String,
    manifest_sha256: String,
    publisher_key: String,
    publisher_domain: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    version: u32,
    keys: Vec<String>,
}

pub(crate) struct Publication {
    pub(crate) metadata: Metadata,
    pub(super) archive: Vec<u8>,
    descriptor: Descriptor,
    executable: Vec<u8>,
}

fn decode(value: &str, limit: usize) -> Result<Vec<u8>, Error> {
    // The signed fields always use standard, padded base64.
    if value.len() > limit.div_ceil(3) * 4 {
        return Err(Error::Signature);
    }
    let bytes = STANDARD.decode(value).map_err(|_| Error::Signature)?;
    if bytes.is_empty() || bytes.len() > limit {
        return Err(Error::Signature);
    }
    Ok(bytes)
}

pub(crate) fn digest(raw: &[u8]) -> String {
    format!("{:x}", Sha256::digest(raw))
}

fn source_url(value: &str) -> Result<Url, Error> {
    if value.len() > 2048 || value.contains(['%', '\\']) {
        return Err(Error::Source);
    }
    let u = Url::parse(value).map_err(|_| Error::Source)?;
    if u.scheme() != "https"
        || !matches!(u.host(), Some(Host::Domain(_)))
        || !u.username().is_empty()
        || u.password().is_some()
        || u.port().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || u.as_str() != value
    {
        return Err(Error::Source);
    }
    Ok(u)
}

fn fetch(url: &str, limit: u64) -> Result<Vec<u8>, Error> {
    source_url(url)?;
    crate::access_actions::Network::prepare(url, "GET", Vec::new())
        .and_then(|network| network.fetch(limit, 10, || true))
        .map_err(|_| Error::Source)
}

fn namespace(domain: &str) -> Option<String> {
    let parts: Vec<_> = domain.split('.').collect();
    if domain.len() > 123 || parts.len() < 2 || !parts.iter().all(|p| slug(p)) {
        return None;
    }
    Some(parts.into_iter().rev().collect::<Vec<_>>().join(".") + ".")
}

fn verification(id: &str, domain: Option<&str>) -> Result<Verification, Error> {
    let reserved = id.starts_with("sh.hyperme.");
    match domain {
        Some(domain) if namespace(domain).is_some_and(|prefix| id.starts_with(&prefix)) => {
            if domain == "hyperme.sh" {
                Ok(Verification::FirstParty)
            } else if reserved {
                Err(Error::Publisher)
            } else {
                Ok(Verification::AppOwnerVerified)
            }
        }
        None if !reserved => Ok(Verification::Community),
        _ => Err(Error::Publisher),
    }
}

fn verify_authority(
    domain: &str,
    key: &[u8],
    lookup: impl FnOnce(&str, u64) -> Result<Vec<u8>, Error>,
) -> Result<(), Error> {
    let url = format!("https://{domain}{PROOF_PATH}");
    let response = lookup(&url, 16 * 1024).map_err(|_| Error::Publisher)?;
    let proof: Authority = serde_json::from_slice(&response).map_err(|_| Error::Publisher)?;
    if proof.version != 1
        || proof.keys.len() > 16
        || !proof
            .keys
            .iter()
            .any(|k| decode(k, 32).is_ok_and(|decoded| decoded == key))
    {
        return Err(Error::Publisher);
    }
    Ok(())
}

pub(super) fn sign_package(
    path: &Path,
    signing_key: &[u8],
    source: &str,
    publisher_domain: Option<&str>,
) -> Result<(Vec<u8>, String), Error> {
    hypermesh_local_storage::check_dir(path).map_err(|_| Error::Source)?;
    source_url(source)?;
    let key =
        Ed25519KeyPair::from_pkcs8_maybe_unchecked(signing_key).map_err(|_| Error::Signature)?;
    let manifest = private_bytes(&path.join("manifest.json"), 64 * 1024)?;
    let manifest_sha256 = digest(&manifest);
    let descriptor = Descriptor::parse(&manifest, &manifest_sha256)?;
    let executable = private_bytes(&path.join("plugin"), super::MAX_EXECUTABLE)?;
    descriptor.check_executable(&executable)?;
    verification(&descriptor.manifest.id, publisher_domain)?;
    let publisher_key = STANDARD.encode(key.public_key().as_ref());
    let statement = Statement {
        version: 1,
        plugin_id: descriptor.manifest.id,
        package_version: descriptor.manifest.package_version,
        source: source.into(),
        manifest_sha256,
        publisher_key: publisher_key.clone(),
        publisher_domain: publisher_domain.map(str::to_owned),
    };
    Ok((
        seal(&key, &statement, &manifest, &executable)?,
        publisher_key,
    ))
}

fn seal(
    key: &Ed25519KeyPair,
    statement: &Statement,
    manifest: &[u8],
    executable: &[u8],
) -> Result<Vec<u8>, Error> {
    let statement = serde_json::to_vec(statement).map_err(|_| Error::Signature)?;
    let mut message = SIGNATURE_DOMAIN.to_vec();
    message.extend_from_slice(&statement);
    serde_json::to_vec(&Envelope {
        statement: STANDARD.encode(statement),
        signature: STANDARD.encode(key.sign(&message).as_ref()),
        manifest: STANDARD.encode(manifest),
        executable: STANDARD.encode(executable),
    })
    .map_err(|_| Error::Signature)
}

impl Publication {
    pub(super) fn bundled(archive: Vec<u8>) -> Result<Self, Error> {
        let publication = Self::verify(archive, None, None, |_, _| Err(Error::Publisher))?;
        if publication.metadata.id != super::GITHUB_ISSUE_PLUGIN_ID {
            return Err(Error::Publisher);
        }
        Ok(publication)
    }

    pub(crate) fn read(source: &str, expected: Option<&str>) -> Result<Self, Error> {
        let source_path = Path::new(source);
        let (archive, selected_url) = if source.starts_with("https://") {
            (fetch(source, MAX_ARTIFACT)?, Some(source))
        } else if source_path.is_absolute() && source.len() <= 4096 {
            (
                private_bytes(source_path, MAX_ARTIFACT).map_err(|_| Error::Source)?,
                None,
            )
        } else {
            return Err(Error::Source);
        };
        Self::verify(archive, expected, selected_url, fetch)
    }

    fn verify(
        archive: Vec<u8>,
        expected: Option<&str>,
        url: Option<&str>,
        lookup: impl FnOnce(&str, u64) -> Result<Vec<u8>, Error>,
    ) -> Result<Self, Error> {
        let (publication, statement, key) = Self::parse(archive, expected, url)?;
        if let Some(domain) = &statement.publisher_domain {
            verify_authority(domain, &key, lookup)?;
        }
        for tool in &publication.descriptor.manifest.tools {
            schema::compile(&tool.input_schema)
                .map_err(|_| Error::Package(PackageError::Package))?;
            schema::compile(&tool.output_schema)
                .map_err(|_| Error::Package(PackageError::Package))?;
        }
        Ok(publication)
    }

    pub(super) fn installed(
        archive: Vec<u8>,
        expected: &Metadata,
        tool: &str,
    ) -> Result<Package, Error> {
        let (candidate, _, _) = Self::parse(archive, Some(&expected.publication_sha256), None)?;
        if candidate.metadata.id != expected.id
            || candidate.metadata.publisher != expected.publisher
            || candidate.metadata.manifest_sha256 != expected.manifest_sha256
        {
            return Err(Error::Storage);
        }
        candidate
            .descriptor
            .select(candidate.executable, tool)
            .map_err(Into::into)
    }

    fn parse(
        archive: Vec<u8>,
        expected: Option<&str>,
        url: Option<&str>,
    ) -> Result<(Self, Statement, Vec<u8>), Error> {
        if archive.len() as u64 > MAX_ARTIFACT {
            return Err(Error::Source);
        }
        let publication_sha256 = digest(&archive);
        if expected.is_some_and(|expected| expected != publication_sha256) {
            return Err(Error::Changed);
        }
        let envelope: Envelope = serde_json::from_slice(&archive).map_err(|_| Error::Signature)?;
        let signed = decode(&envelope.statement, 16 * 1024)?;
        let statement: Statement = serde_json::from_slice(&signed).map_err(|_| Error::Signature)?;
        if statement.version != 1 {
            return Err(Error::Signature);
        }
        source_url(&statement.source)?;
        if url.is_some_and(|url| url != statement.source) {
            return Err(Error::Source);
        }
        let key = decode(&statement.publisher_key, 32)?;
        let signature = decode(&envelope.signature, 64)?;
        let mut message = SIGNATURE_DOMAIN.to_vec();
        message.extend_from_slice(&signed);
        UnparsedPublicKey::new(&ED25519, &key)
            .verify(&message, &signature)
            .map_err(|_| Error::Signature)?;
        let manifest = decode(&envelope.manifest, 64 * 1024)?;
        let descriptor = Descriptor::parse(&manifest, &statement.manifest_sha256)?;
        let executable = decode(&envelope.executable, super::MAX_EXECUTABLE as usize)?;
        descriptor.check_executable(&executable)?;
        if descriptor.manifest.id != statement.plugin_id
            || descriptor.manifest.package_version != statement.package_version
        {
            return Err(Error::Signature);
        }
        let publisher = Publisher {
            verification: verification(
                &statement.plugin_id,
                statement.publisher_domain.as_deref(),
            )?,
            domain: statement.publisher_domain.clone(),
            key_sha256: digest(&key),
        };
        let metadata = Metadata {
            metadata_version: 1,
            id: statement.plugin_id.clone(),
            package_version: statement.package_version.clone(),
            source: statement.source.clone(),
            publication_sha256,
            manifest_sha256: statement.manifest_sha256.clone(),
            publisher,
            scan_status: ScanStatus::NotScanned,
            api_version: descriptor.version,
            requested_tools: descriptor.manifest.tools.clone(),
        };
        Ok((
            Self {
                metadata,
                archive,
                descriptor,
                executable,
            },
            statement,
            key,
        ))
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use serde_json::Value;

    pub(crate) fn sign(
        manifest: &Value,
        binary: &[u8],
        domain: Option<&str>,
        seed: u8,
    ) -> (Vec<u8>, String) {
        let key = Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap();
        let publisher_key = STANDARD.encode(key.public_key().as_ref());
        let manifest_bytes = serde_json::to_vec(manifest).unwrap();
        let statement = Statement {
            version: 1,
            plugin_id: manifest["id"].as_str().unwrap().into(),
            package_version: manifest["package_version"].as_str().unwrap().into(),
            source: "https://packages.example.org/byte-count.visor-plugin".into(),
            manifest_sha256: digest(&manifest_bytes),
            publisher_key: publisher_key.clone(),
            publisher_domain: domain.map(str::to_owned),
        };
        let package = seal(&key, &statement, &manifest_bytes, binary).unwrap();
        (package, publisher_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fixtures::sign;
    use serde_json::{json, Value};

    const ELF: &[u8] = b"\x7fELFfixture";

    fn manifest(id: &str, versions: &[u32]) -> Value {
        json!({"manifest_version":1, "id":id, "package_version":"1.0.0",
            "api_versions":versions,"profile":"local-stdio","executable_sha256":digest(ELF),
            "tools":[{"name":"count-bytes","scope":"workspace.read",
                      "input_schema":{"type":"object"},"output_schema":{"type":"object"}}]})
    }

    #[test]
    fn external_publishers_can_sign_without_claiming_verified_ownership() {
        let (archive, _) = sign(&manifest("org.example.author", &[1, 0]), ELF, None, 10);
        let expected = digest(&archive);
        let signed = Publication::verify(
            archive,
            Some(&expected),
            Some("https://packages.example.org/byte-count.visor-plugin"),
            |_, _| panic!("community publication requires no domain lookup"),
        )
        .unwrap();
        assert_eq!(
            signed.metadata.publisher.verification,
            Verification::Community
        );
        assert_eq!(signed.metadata.added_access(None), vec!["count-bytes"]);
        assert_eq!(
            serde_json::to_value(signed.metadata).unwrap()["scan_status"],
            "not_scanned"
        );
    }

    #[test]
    fn official_app_owner_and_first_party_require_the_exact_https_namespace_key() {
        for (id, domain, expected) in [
            (
                "com.app-owner.count",
                "app-owner.com",
                Verification::AppOwnerVerified,
            ),
            ("sh.hyperme.count", "hyperme.sh", Verification::FirstParty),
        ] {
            let (archive, key) = sign(&manifest(id, &[1]), ELF, Some(domain), 20);
            let lookup = |url: &str, limit| {
                assert_eq!(url, format!("https://{domain}{PROOF_PATH}"));
                assert_eq!(limit, 16 * 1024);
                Ok(serde_json::to_vec(&json!({"version":1, "keys":[key]})).unwrap())
            };
            let result = Publication::verify(archive.clone(), None, None, lookup).unwrap();
            assert_eq!(result.metadata.publisher.verification, expected);
            assert!(matches!(
                Publication::verify(archive, None, None, |_, _| Ok(
                    br#"{"version":1,"keys":[]}"#.to_vec()
                )),
                Err(Error::Publisher)
            ));
        }
        for domain in [None, Some("hyperme.sh.evil.example"), Some("evil.example")] {
            let (archive, _) = sign(&manifest("sh.hyperme.count", &[1]), ELF, domain, 99);
            assert!(matches!(
                Publication::verify(archive, None, None, |_, _| panic!("namespace fails first")),
                Err(Error::Publisher)
            ));
        }
    }

    #[test]
    fn review_refuses_changed_bytes_valid_signed_metadata_with_wrong_payload_and_unknown_versions()
    {
        let manifest = manifest("org.example.author", &[1]);
        let (archive, _) = sign(&manifest, ELF, None, 10);
        let expected = digest(&archive);
        let mut wrong: Value = serde_json::from_slice(&archive).unwrap();
        wrong["executable"] = json!(STANDARD.encode(b"\x7fELFchanged"));
        let wrong_archive = serde_json::to_vec(&wrong).unwrap();
        assert!(matches!(
            Publication::verify(wrong_archive.clone(), Some(&expected), None, fetch),
            Err(Error::Changed)
        ));
        assert!(matches!(
            Publication::verify(wrong_archive, None, None, fetch),
            Err(Error::Package(PackageError::Package))
        ));
        wrong["signature"] = json!(STANDARD.encode([0u8; 64]));
        assert!(matches!(
            Publication::verify(serde_json::to_vec(&wrong).unwrap(), None, None, fetch),
            Err(Error::Signature)
        ));
        let mut unsupported = manifest;
        unsupported["api_versions"] = json!([999]);
        let (unsupported, _) = sign(&unsupported, ELF, None, 10);
        assert!(matches!(
            Publication::verify(unsupported, None, None, fetch),
            Err(Error::Package(PackageError::Incompatible))
        ));
    }
}
