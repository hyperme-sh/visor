//! Owner-only installed publications. An atomic record binds metadata to its package.
use super::{
    publication::{Error, Metadata, Publication, MAX_ARTIFACT},
    valid_id, Package, Staged,
};
use hypermesh_local_storage as storage;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

const MAX_ENTRIES: usize = 16;
const MAX_HEADER: u64 = 128 * 1024;
const SUFFIX: &str = ".package";
const BUNDLE_PATH: &str = "/usr/local/share/hypermesh-visor/plugins/github-issue.visor-plugin";

pub(crate) struct Store {
    dir: PathBuf,
    bundled: Option<(Entry, Vec<u8>)>,
    // A single running Visor owns this store, including across different socket paths.
    lock: File,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    record_version: u32,
    pub(crate) revision: Uuid,
    pub(crate) enabled: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) bundled: bool,
    pub(crate) metadata: Metadata,
}

impl Store {
    pub(crate) fn open(config: &Path) -> Result<Self, Error> {
        fs::create_dir_all(config).map_err(|_| Error::Storage)?;
        let dir = config.join("plugins-installed");
        storage::create_dir(&dir).map_err(|_| Error::Storage)?;
        let lock =
            storage::open(&dir.join("lock"), storage::Open::Lock).map_err(|_| Error::Storage)?;
        storage::lock(&lock).map_err(|_| Error::Storage)?;
        Ok(Self {
            dir,
            bundled: load_bundle(Path::new(BUNDLE_PATH))?,
            lock,
        })
    }

    fn path(&self, id: &str) -> Result<PathBuf, Error> {
        if !valid_id(id) {
            return Err(Error::Missing);
        }
        Ok(self.dir.join(format!("{id}{SUFFIX}")))
    }

    fn open_entry(&self, id: &str) -> Result<Option<(Entry, BufReader<File>)>, Error> {
        let file = match storage::open(&self.path(id)?, storage::Open::Read { private: true }) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Error::Storage),
        };
        if file.metadata().map_err(|_| Error::Storage)?.len() > MAX_HEADER + MAX_ARTIFACT {
            return Err(Error::Storage);
        }
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        (&mut reader)
            .take(MAX_HEADER + 1)
            .read_until(b'\n', &mut line)
            .map_err(|_| Error::Storage)?;
        if line.len() as u64 > MAX_HEADER || line.last() != Some(&b'\n') {
            return Err(Error::Storage);
        }
        let entry: Entry = serde_json::from_slice(&line).map_err(|_| Error::Storage)?;
        if entry.record_version != 1
            || entry.metadata.metadata_version != 1
            || entry.revision.is_nil()
            || entry.bundled
            || entry.metadata.id != id
        {
            return Err(Error::Storage);
        }
        Ok(Some((entry, reader)))
    }

    pub(crate) fn get(&self, id: &str) -> Result<Option<Entry>, Error> {
        Ok(self
            .open_entry(id)?
            .map(|(entry, _)| entry)
            .or_else(|| self.bundle(id).map(|(entry, _)| entry.clone())))
    }

    fn bundle(&self, id: &str) -> Option<&(Entry, Vec<u8>)> {
        self.bundled
            .as_ref()
            .filter(|(entry, _)| entry.metadata.id == id)
    }

    pub(crate) fn list(&self) -> Result<Vec<Entry>, Error> {
        let items = fs::read_dir(&self.dir).map_err(|_| Error::Storage)?;
        let mut entries = Vec::new();
        for item in items {
            let name = item.map_err(|_| Error::Storage)?.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(SUFFIX)) else {
                continue;
            };
            entries.push(self.get(id)?.ok_or(Error::Storage)?);
            if entries.len() > MAX_ENTRIES {
                return Err(Error::Storage);
            }
        }
        if let Some((entry, _)) = &self.bundled {
            if !entries
                .iter()
                .any(|item| item.metadata.id == entry.metadata.id)
            {
                entries.push(entry.clone());
            }
        }
        entries.sort_unstable_by(|a, b| a.metadata.id.cmp(&b.metadata.id));
        Ok(entries)
    }

    pub(crate) fn install(
        &self,
        package: Publication,
        expected: Option<Uuid>,
        invalidate: impl FnOnce(),
    ) -> Result<Entry, Error> {
        let before = self.get(&package.metadata.id)?;
        if before.as_ref().is_some_and(|entry| entry.bundled) {
            return Err(Error::Bundled);
        }
        if before.as_ref().map(|entry| entry.revision) != expected {
            return Err(Error::Changed);
        }
        if let Some(before) = &before {
            if before.metadata.publisher != package.metadata.publisher {
                return Err(Error::Publisher);
            }
            if before.metadata.package_version == package.metadata.package_version {
                return Err(Error::Changed);
            }
        } else if self.list()?.iter().filter(|entry| !entry.bundled).count() >= MAX_ENTRIES {
            return Err(Error::Storage);
        }
        let next = Entry {
            record_version: 1,
            revision: Uuid::new_v4(),
            enabled: before.is_none_or(|entry| entry.enabled),
            bundled: false,
            metadata: package.metadata,
        };
        self.write(&next, &mut package.archive.as_slice(), invalidate)?;
        Ok(next)
    }

    pub(crate) fn activate(&self, id: &str, revision: Uuid, tool: &str) -> Result<Package, Error> {
        let Some((entry, reader)) = self.open_entry(id)? else {
            let (entry, archive) = self.bundle(id).ok_or(Error::Missing)?;
            if entry.revision != revision {
                return Err(Error::Changed);
            }
            let mut package = Publication::installed(archive.clone(), &entry.metadata, tool)?;
            package.bundled = true;
            return Ok(package);
        };
        if entry.revision != revision {
            return Err(Error::Changed);
        }
        if !entry.enabled {
            return Err(Error::Disabled);
        }
        let mut bytes = Vec::new();
        reader
            .take(MAX_ARTIFACT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Storage)?;
        Publication::installed(bytes, &entry.metadata, tool)
    }

    pub(crate) fn set_enabled(
        &self,
        id: &str,
        expected: Uuid,
        enabled: bool,
        invalidate: impl FnOnce(),
    ) -> Result<Entry, Error> {
        let (mut entry, mut archive) = self.current(id, expected)?;
        if entry.enabled != enabled {
            entry.enabled = enabled;
            entry.revision = Uuid::new_v4();
            self.write(&entry, &mut archive, invalidate)?;
        }
        Ok(entry)
    }

    pub(crate) fn remove(
        &self,
        id: &str,
        expected: Uuid,
        invalidate: impl FnOnce(),
    ) -> Result<(), Error> {
        self.current(id, expected)?;
        fs::remove_file(self.path(id)?).map_err(|_| Error::Storage)?;
        invalidate();
        storage::sync_dir(&self.dir).map_err(|_| Error::Storage)
    }

    fn current(&self, id: &str, expected: Uuid) -> Result<(Entry, BufReader<File>), Error> {
        let Some((entry, file)) = self.open_entry(id)? else {
            return Err(if self.bundle(id).is_some() {
                Error::Bundled
            } else {
                Error::Missing
            });
        };
        if entry.revision != expected {
            return Err(Error::Changed);
        }
        Ok((entry, file))
    }

    fn write(
        &self,
        entry: &Entry,
        archive: &mut impl Read,
        invalidate: impl FnOnce(),
    ) -> Result<(), Error> {
        let header = serde_json::to_vec(entry).map_err(|_| Error::Storage)?;
        if header.len() as u64 >= MAX_HEADER {
            return Err(Error::Storage);
        }
        let path = self.dir.join(format!(".write-{}", Uuid::new_v4()));
        let mut file = storage::open(&path, storage::Open::New).map_err(|_| Error::Storage)?;
        let pending = Staged(path);
        file.write_all(&header)
            .and_then(|_| file.write_all(b"\n"))
            .map_err(|_| Error::Storage)?;
        let size =
            io::copy(&mut archive.take(MAX_ARTIFACT + 1), &mut file).map_err(|_| Error::Storage)?;
        if size == 0 || size > MAX_ARTIFACT {
            return Err(Error::Storage);
        }
        file.sync_all().map_err(|_| Error::Storage)?;
        storage::replace(&pending.0, &self.path(&entry.metadata.id)?)
            .map_err(|_| Error::Storage)?;
        invalidate();
        storage::sync_dir(&self.dir).map_err(|_| Error::Storage)
    }
}

fn load_bundle(path: &Path) -> Result<Option<(Entry, Vec<u8>)>, Error> {
    // Only the image's fixed, root-owned location supplies a built-in package.
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Error::Storage),
    };
    let metadata = file.metadata().map_err(|_| Error::Storage)?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(Error::Storage);
    }
    for parent in path.ancestors().skip(1) {
        let metadata = fs::symlink_metadata(parent).map_err(|_| Error::Storage)?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(Error::Storage);
        }
    }
    let mut archive = Vec::new();
    file.take(MAX_ARTIFACT + 1)
        .read_to_end(&mut archive)
        .map_err(|_| Error::Storage)?;
    let mut revision = [0; 16];
    revision.copy_from_slice(&Sha256::digest(&archive)[..16]);
    let bundle = Publication::bundled(archive)?;
    Ok(Some((
        Entry {
            record_version: 1,
            revision: Uuid::from_bytes(revision),
            enabled: true,
            bundled: true,
            metadata: bundle.metadata,
        },
        bundle.archive,
    )))
}

impl Drop for Store {
    fn drop(&mut self) {
        // A concurrent fork can hold the same open file description until exec.
        drop(storage::UnlockOnDrop(&self.lock));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::publication::{digest, fixtures::sign};
    use serde_json::json;

    #[test]
    fn installed_revision_and_disabled_state_survive_reopening() {
        let dir = std::env::temp_dir().join(format!("visor-installed-{}", Uuid::new_v4()));
        storage::create_dir(&dir).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let id = "org.example.fixture";
        let executable = b"\x7fELFfixture";
        let manifest = json!({
            "manifest_version":1,"id":id,"package_version":"1.0.0","api_versions":[1],
            "profile":"local-stdio","executable_sha256":digest(executable),
            "tools":[{"name":"inspect","scope":"workspace.read","input_schema":{"type":"object"},"output_schema":{"type":"object"}}]
        });
        let (archive, _) = sign(&manifest, executable, None, 15);
        let publication = dir.join("installed.visor-plugin");
        storage::open(&publication, storage::Open::New)
            .unwrap()
            .write_all(&archive)
            .unwrap();

        let store = Store::open(&dir).unwrap();
        assert!(matches!(Store::open(&dir), Err(Error::Storage)));
        let entry = store
            .install(
                Publication::read(publication.to_str().unwrap(), None).unwrap(),
                None,
                || {},
            )
            .unwrap();
        let disabled = store.set_enabled(id, entry.revision, false, || {}).unwrap();
        let inherited = store.lock.try_clone().unwrap();
        drop(store);

        let reopened = Store::open(&dir).unwrap();
        drop(inherited);
        let current = reopened.get(id).unwrap().unwrap();
        assert_eq!(current.revision, disabled.revision);
        assert!(!current.enabled);
        assert!(matches!(
            reopened.activate(id, entry.revision, "inspect"),
            Err(Error::Changed)
        ));
        assert!(matches!(
            reopened.activate(id, disabled.revision, "inspect"),
            Err(Error::Disabled)
        ));
        reopened.remove(id, disabled.revision, || {}).unwrap();
        assert!(reopened.list().unwrap().is_empty());
    }
}
