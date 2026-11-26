//! Public companion pack catalog and local data-only installation.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use hypermesh_local_storage as storage;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::companion_packs::{self as packs, ClipSpec, Pack};

const ORIGIN: &str = "https://raw.githubusercontent.com/hyperme-sh/companion-packs";
const CATALOG_LIMIT: usize = 256 * 1024;
const ARCHIVE_LIMIT: usize = 100 * 1024 * 1024;
const IMAGE_LIMIT: usize = 1024 * 1024;
const VOICE_LIMIT: usize = 300 * 1024;
static RECOVERY: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    bytes: usize,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub author: String,
    pub license: String,
    pub media: Vec<String>,
    archive: Asset,
    pub art_preview: Option<Asset>,
    pub voice_preview: Option<Asset>,
    #[serde(default)]
    pub installed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    format: u32,
    pub source_revision: String,
    pub asset_revision: String,
    pub packs: Vec<Entry>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Preview {
    Art,
    Voice,
}

impl Catalog {
    pub fn fetch(config: &Path) -> Result<Self, String> {
        let bytes = get("catalog/catalog.json", CATALOG_LIMIT)?;
        let mut catalog = Self::parse(&bytes)?;
        // A committed install may have been interrupted between moving the old
        // directory aside and publishing the new one.
        recover_installs(config)?;
        for entry in &mut catalog.packs {
            entry.installed = installed(config, &entry.id)?;
        }
        Ok(catalog)
    }

    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let catalog: Self =
            serde_json::from_slice(bytes).map_err(|_| "Invalid catalog response.")?;
        if bytes.len() > CATALOG_LIMIT
            || catalog.format != 1
            || !hex(&catalog.source_revision, 40)
            || !hex(&catalog.asset_revision, 40)
            || catalog.packs.len() > 256
        {
            return Err("Unsupported catalog or invalid revision.".into());
        }
        let mut ids = HashSet::new();
        for entry in &catalog.packs {
            entry.validate()?;
            if !ids.insert(&entry.id) {
                return Err("The catalog contains a duplicate pack.".into());
            }
        }
        Ok(catalog)
    }
}

impl Entry {
    fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.id)
            || !label(&self.name, 128)
            || !label(&self.author, 128)
            || !label(&self.license, 64)
            || self.media.is_empty()
            || self.media.len() > 2
            || !self
                .media
                .iter()
                .all(|kind| matches!(kind.as_str(), "appearance" | "voice"))
            || self.media.len() == 2 && self.media[0] == self.media[1]
            || !self.archive.valid(ARCHIVE_LIMIT)
            || self.art_preview.as_ref().is_some_and(|asset| {
                !asset.valid(IMAGE_LIMIT) || !self.media.iter().any(|kind| kind == "appearance")
            })
            || self.voice_preview.as_ref().is_some_and(|asset| {
                !asset.valid(VOICE_LIMIT) || !self.media.iter().any(|kind| kind == "voice")
            })
        {
            return Err("Invalid companion pack listing.".into());
        }
        Ok(())
    }

    pub fn preview(&self, revision: &str, medium: Preview) -> Result<Vec<u8>, String> {
        self.validate()?;
        let (extension, asset) = match medium {
            Preview::Art => ("png", self.art_preview.as_ref()),
            Preview::Voice => ("wav", self.voice_preview.as_ref()),
        };
        let asset = asset.ok_or("This pack has no preview for that medium.")?;
        let bytes = self.download(revision, extension, asset)?;
        match medium {
            Preview::Art => {
                let image = packs::read_png(&bytes, Some(packs::SPRITE_SIZE))
                    .or_else(|_| packs::read_png(&bytes, Some(packs::CURSOR_SIZE)))
                    .map_err(|_| "The artwork preview is invalid.")?;
                crate::desktop::pixels::encode_rgba_png(image.width, image.height, &image.rgba)
                    .map_err(|_| "The artwork preview could not be rendered.".into())
            }
            Preview::Voice => packs::decode_wave(&bytes)
                .map(|wave| wave.wave_bytes())
                .ok_or_else(|| "The voice preview is invalid.".into()),
        }
    }

    pub fn install(&self, config: &Path, revision: &str, replace: bool) -> Result<(), String> {
        self.validate()?;
        if !replace && installed(config, &self.id)? {
            return Err("A local copy already exists. Confirm replacement to continue.".into());
        }
        let bytes = self.download(revision, "pack.json", &self.archive)?;
        self.install_bytes(config, &bytes, replace)
    }

    fn download(&self, revision: &str, extension: &str, asset: &Asset) -> Result<Vec<u8>, String> {
        if !hex(revision, 40) {
            return Err("Invalid pack revision.".into());
        }
        let bytes = get(
            &format!("{revision}/packs/{}.{extension}", self.id),
            asset.bytes,
        )?;
        asset.verify(&bytes)?;
        Ok(bytes)
    }

    fn install_bytes(&self, config: &Path, bytes: &[u8], replace: bool) -> Result<(), String> {
        self.archive.verify(bytes)?;
        let archive = Archive::parse(bytes)?;
        let root = InstallRoot::open(config)?;
        root.recover(&self.id)?;
        let target = root.path.join(&self.id);
        let existed = entry_kind(&target)?;
        if existed && !replace {
            return Err("A local copy already exists. Confirm replacement to continue.".into());
        }
        let stage = Staging::new(&root.path)?;
        let pack_dir = stage.0.join("companion-packs").join(&self.id);
        fs::create_dir_all(&pack_dir).map_err(local_error)?;
        archive.unpack(&pack_dir)?;
        validate_pack(&stage.0, &self.id, &self.name, &self.media)?;
        root.commit(&self.id, &pack_dir, existed)
    }
}

impl Asset {
    fn valid(&self, limit: usize) -> bool {
        self.bytes > 0 && self.bytes <= limit && hex(&self.sha256, 64)
    }

    fn verify(&self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() != self.bytes || format!("{:x}", Sha256::digest(bytes)) != self.sha256 {
            return Err("Download failed the size or SHA-256 check.".into());
        }
        Ok(())
    }
}

fn get(path: &str, limit: usize) -> Result<Vec<u8>, String> {
    #[cfg(test)]
    if let Some(result) = published_contract::read_fixture(path, limit) {
        return result;
    }
    let response = ureq::AgentBuilder::new()
        .redirects(0)
        .try_proxy_from_env(false)
        .timeout(Duration::from_secs(60))
        .timeout_connect(Duration::from_secs(8))
        .build()
        .get(&format!("{ORIGIN}/{path}"))
        .call()
        .map_err(|_| "The community catalog or pack could not be downloaded.")?;
    if response.status() != 200 {
        return Err("The community catalog or pack could not be downloaded.".into());
    }
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "The community download was interrupted.")?;
    if bytes.len() > limit {
        return Err("The community download exceeds its size limit.".into());
    }
    Ok(bytes)
}

#[cfg(test)]
#[path = "companion_catalog_contract.rs"]
mod published_contract;

fn hex(value: &str, size: usize) -> bool {
    value.len() == size
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn label(value: &str, limit: usize) -> bool {
    !value.trim().is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.as_bytes()[0] != b'-'
        && !id.ends_with('-')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && safe_filename(id)
}

fn safe_filename(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit();
    !reserved
        && !name.is_empty()
        && name.len() <= 128
        && !name.ends_with('.')
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

fn file_limit(name: &str) -> Option<usize> {
    if !safe_filename(name) {
        return None;
    }
    match name {
        "pack.json" => Some(32 * 1024),
        "LICENSE" | "ATTRIBUTION.md" => Some(64 * 1024),
        _ if name.ends_with(".png") => Some(IMAGE_LIMIT),
        _ if name.ends_with(".wav") => Some(VOICE_LIMIT),
        _ => None,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Archive<'a> {
    format: u32,
    #[serde(borrow)]
    files: Vec<(&'a str, &'a str)>,
}

impl<'a> Archive<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, String> {
        let archive: Self = serde_json::from_slice(bytes).map_err(|_| "Invalid pack archive.")?;
        let mut names = HashSet::new();
        if archive.format != 1 || archive.files.len() > 95 {
            return Err("Unsupported pack archive or too many files.".into());
        }
        for &(name, encoded) in &archive.files {
            let limit = file_limit(name).ok_or("The archive contains an unsupported filename.")?;
            if !names.insert(name.to_ascii_lowercase()) || encoded.len() > limit.div_ceil(3) * 4 {
                return Err("The archive contains a duplicate or oversized file.".into());
            }
        }
        for required in ["pack.json", "LICENSE", "ATTRIBUTION.md"] {
            if !archive.files.iter().any(|&(name, _)| name == required) {
                return Err("The archive is missing pack metadata or licensing.".into());
            }
        }
        Ok(archive)
    }

    fn unpack(&self, destination: &Path) -> Result<(), String> {
        for &(name, encoded) in &self.files {
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|_| "Invalid pack asset encoding.")?;
            if bytes.is_empty() || bytes.len() > file_limit(name).unwrap_or_default() {
                return Err("The archive contains an empty or oversized file.".into());
            }
            let path = destination.join(name);
            let mut file = storage::open(&path, storage::Open::New).map_err(local_error)?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(local_error)?;
        }
        Ok(())
    }
}

fn validate_pack(config: &Path, id: &str, name: &str, media: &[String]) -> Result<(), String> {
    let pack = Pack::load(config, id)?;
    if pack.manifest.name != name {
        return Err("The downloaded manifest does not match the catalog entry.".into());
    }
    let mut sprites = HashSet::new();
    let mut cursors = HashSet::new();
    let mut voices = HashSet::new();
    for (key, entry) in &pack.manifest.sprites {
        if !packs::valid_key(key) {
            return Err("Invalid sprite cue.".into());
        }
        if let Some(clip) = entry.as_ref().map(ClipSpec::parse).transpose()?.flatten() {
            sprites.extend(clip.frames);
        }
    }
    for entry in pack.manifest.cursor.values().flatten() {
        cursors.insert(
            entry
                .as_str()
                .ok_or("An invalid cursor asset was downloaded.")?,
        );
    }
    for (key, entry) in &pack.manifest.voice {
        if !packs::valid_key(key) {
            return Err("Invalid voice cue.".into());
        }
        if let Some(entry) = entry {
            voices.insert(
                entry
                    .as_str()
                    .ok_or("An invalid voice asset was downloaded.")?,
            );
        }
    }
    if sprites.len() > packs::MAX_DISTINCT_SPRITES
        || cursors.len() > 12
        || voices.len() > packs::MAX_DISTINCT_VOICES
        || sprites.is_empty() && cursors.is_empty() && voices.is_empty()
    {
        return Err("The pack has no media or exceeds its media limits.".into());
    }
    let has_art = !sprites.is_empty() || !cursors.is_empty();
    let has_voice = !voices.is_empty();
    if has_art != media.iter().any(|kind| kind == "appearance")
        || has_voice != media.iter().any(|kind| kind == "voice")
    {
        return Err("The downloaded media does not match the catalog entry.".into());
    }
    for name in sprites {
        pack.sprite(&name)?;
    }
    for name in cursors {
        pack.cursor(name)?;
    }
    for name in voices {
        pack.voice(name)
            .ok_or("An invalid voice asset was downloaded.")?;
    }
    Ok(())
}

fn installed(config: &Path, id: &str) -> Result<bool, String> {
    let root = config.join("companion-packs");
    if !entry_kind(&root)? {
        return Ok(false);
    }
    entry_kind(&root.join(id))
}

pub fn recover_installs(config: &Path) -> Result<(), String> {
    // Native startup loads the two media concurrently. Only one request may
    // scan and restore backups before the other tries the nonblocking file lock.
    let _recovery = RECOVERY.lock().map_err(|_| "Companion pack recovery failed.")?;
    let root = config.join("companion-packs");
    if !entry_kind(&root)? {
        return Ok(());
    }
    let mut missing = Vec::new();
    for entry in fs::read_dir(&root).map_err(local_error)? {
        let name = entry.map_err(local_error)?.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_prefix(".previous-"))
        else {
            continue;
        };
        if valid_id(id) && !entry_kind(&root.join(id))? {
            missing.push(id.to_owned());
        }
    }
    if !missing.is_empty() {
        let root = InstallRoot::open(config)?;
        for id in missing {
            root.recover(&id)?;
        }
    }
    Ok(())
}

fn entry_kind(path: &Path) -> Result<bool, String> {
    let info = match fs::symlink_metadata(path) {
        Ok(info) => info,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(local_error(error)),
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if info.file_attributes() & 0x400 != 0 {
            return Err("A local pack path is a reparse point.".into());
        }
    }
    if !info.is_dir() || info.file_type().is_symlink() {
        return Err("A local pack path is not a regular directory.".into());
    }
    Ok(true)
}

fn local_error(_error: io::Error) -> String {
    "The local companion packs directory could not be updated.".into()
}

struct InstallRoot {
    path: PathBuf,
    _directory: File,
    _lock: File,
}

impl InstallRoot {
    fn open(config: &Path) -> Result<Self, String> {
        fs::create_dir_all(config).map_err(local_error)?;
        let lock = storage::open(&config.join("companion-catalog.lock"), storage::Open::Lock)
            .map_err(local_error)?;
        storage::lock(&lock).map_err(|_| "Another companion pack operation is in progress.")?;
        let path = config.join("companion-packs");
        if !entry_kind(&path)? {
            fs::create_dir(&path).map_err(local_error)?;
        }
        let directory = packs::open_pack_root(config).map_err(local_error)?;
        Ok(Self {
            path,
            _directory: directory,
            _lock: lock,
        })
    }

    fn backup(&self, id: &str) -> PathBuf {
        self.path.join(format!(".previous-{id}"))
    }

    fn recover(&self, id: &str) -> Result<(), String> {
        let target = self.path.join(id);
        let backup = self.backup(id);
        if entry_kind(&backup)? && !entry_kind(&target)? {
            fs::rename(backup, target).map_err(local_error)?;
        }
        Ok(())
    }

    fn commit(&self, id: &str, staged: &Path, existed: bool) -> Result<(), String> {
        let target = self.path.join(id);
        let backup = self.backup(id);
        if entry_kind(&backup)? {
            // An earlier complete install still has its previous copy.
            fs::remove_dir_all(&backup).map_err(local_error)?;
        }
        if existed {
            fs::rename(&target, &backup).map_err(local_error)?;
        }
        if let Err(error) = fs::rename(staged, &target) {
            if existed {
                let _ = fs::rename(&backup, &target);
            }
            return Err(local_error(error));
        }
        if existed {
            let _ = fs::remove_dir_all(&backup);
        }
        Ok(())
    }
}

/// A removal moves the pack out of the lookup directory before reclaiming its files.
pub fn remove(config: &Path, id: &str) -> Result<(), String> {
    if !safe_filename(id) {
        return Err("Invalid local pack ID.".into());
    }
    let root = InstallRoot::open(config)?;
    root.recover(id)?;
    let target = root.path.join(id);
    if entry_kind(&target)? {
        let stage = Staging::new(&root.path)?;
        let backup = root.backup(id);
        if entry_kind(&backup)? {
            fs::remove_dir_all(backup).map_err(local_error)?;
        }
        fs::rename(target, stage.0.join("removed")).map_err(local_error)?;
    }
    Ok(())
}

struct Staging(PathBuf);

impl Staging {
    fn new(root: &Path) -> Result<Self, String> {
        let path = root.join(format!(".catalog-{}", Uuid::new_v4()));
        storage::create_dir(&path).map_err(local_error)?;
        Ok(Self(path))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(asset: &[u8]) -> (Vec<u8>, Entry) {
        let archive = serde_json::to_vec(&json!({"format":1,"files":[
            ["pack.json",STANDARD.encode(br#"{"format":1,"name":"Desk Bot","sprites":{"default":{"frames":["idle.png"]},"state:paused":null}}"#)],
            ["LICENSE",STANDARD.encode(b"CC0-1.0 fixture")],
            ["ATTRIBUTION.md",STANDARD.encode(b"Test author")],
            ["idle.png",STANDARD.encode(asset)],
        ]})).unwrap();
        let entry = Entry {
            id: "desk-bot".into(),
            name: "Desk Bot".into(),
            author: "Test author".into(),
            license: "CC0-1.0".into(),
            media: vec!["appearance".into()],
            archive: Asset {
                bytes: archive.len(),
                sha256: format!("{:x}", Sha256::digest(&archive)),
            },
            art_preview: None,
            voice_preview: None,
            installed: false,
        };
        (archive, entry)
    }

    #[test]
    fn catalog_constrains_revisions_and_pack_ids() {
        let (_, entry) = fixture(&[1]);
        let mut index = json!({"format":1,"source_revision":"1".repeat(40),
                               "asset_revision":"2".repeat(40),"packs":[entry]});
        let parse = |value: &serde_json::Value| Catalog::parse(&serde_json::to_vec(value).unwrap());
        assert_eq!(parse(&index).unwrap().packs[0].id, "desk-bot");
        index["packs"][0]["id"] = json!("../different");
        assert!(parse(&index).is_err());
        index["packs"][0]["id"] = json!("desk-bot");
        index["asset_revision"] = json!("https://invalid.example");
        assert!(parse(&index).is_err());
        index["asset_revision"] = json!("2".repeat(40));
        index["packs"].as_array_mut().unwrap().push(json!(entry));
        assert!(parse(&index).is_err());
    }

    #[test]
    fn archive_rejects_paths_aliases_duplicates_and_missing_metadata() {
        let (archive, _) = fixture(&[1]);
        let source: serde_json::Value = serde_json::from_slice(&archive).unwrap();
        for invalid in [
            "../else.png",
            "C:paint.png",
            "folder/frame.png",
            "aux.png",
            "idle.png.",
            "IDLE.png",
        ] {
            let mut archive = source.clone();
            archive["files"]
                .as_array_mut()
                .unwrap()
                .push(json!([invalid, "AAAA"]));
            assert!(
                Archive::parse(&serde_json::to_vec(&archive).unwrap()).is_err(),
                "{invalid}"
            );
        }
        let mut archive = source;
        archive["files"].as_array_mut().unwrap().remove(1);
        assert!(Archive::parse(&serde_json::to_vec(&archive).unwrap()).is_err());
    }

    #[test]
    fn concurrent_native_loads_can_all_recover_the_same_install() {
        let config = Staging::new(&std::env::temp_dir()).unwrap();
        let root = config.0.join("companion-packs");
        fs::create_dir_all(root.join(".previous-desk-bot")).unwrap();
        let readers = 8;
        let start = std::sync::Barrier::new(readers);
        std::thread::scope(|scope| {
            for _ in 0..readers {
                scope.spawn(|| {
                    start.wait();
                    recover_installs(&config.0).unwrap();
                    assert!(root.join("desk-bot").is_dir());
                });
            }
        });
    }

    #[test]
    fn install_preserves_local_edits_until_verified_replacement_and_recovers_previous_copy() {
        let config = Staging::new(&std::env::temp_dir()).unwrap();
        let root = config.0.join("companion-packs");
        let pack = root.join("desk-bot");
        let (archive, entry) = fixture(include_bytes!(
            "../companion/assets/states/idle_0_400x520.png"
        ));
        entry.install_bytes(&config.0, &archive, false).unwrap();
        fs::write(pack.join("local-edit.txt"), b"local art notes").unwrap();
        assert!(entry.install_bytes(&config.0, &archive, false).is_err());
        assert!(entry
            .install_bytes(&config.0, b"wrong digest", true)
            .is_err());
        let (bad_media, bad_entry) = fixture(b"not a PNG");
        assert!(bad_entry
            .install_bytes(&config.0, &bad_media, true)
            .is_err());
        assert!(pack.join("local-edit.txt").is_file());

        let backup = root.join(".previous-desk-bot");
        fs::rename(&pack, &backup).unwrap();
        assert_eq!(
            packs::installed(&config.0, packs::Medium::Appearance).unwrap(),
            vec![("desk-bot".into(), "Desk Bot".into())]
        );
        assert!(pack.join("local-edit.txt").is_file());
        entry.install_bytes(&config.0, &archive, true).unwrap();
        assert!(!pack.join("local-edit.txt").exists());
        assert!(!backup.exists());
        remove(&config.0, "desk-bot").unwrap();
        assert!(!pack.exists());
    }
}
