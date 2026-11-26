use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

pub(super) struct Directory {
    path: PathBuf,
    // Keep the validated directories open without FILE_SHARE_DELETE while
    // reading a manifest and its assets by name.
    _root: File,
    _directory: File,
}

pub(super) fn open_root(config: &Path) -> io::Result<File> {
    open_directory(&config.join("companion-packs"))
}

impl Directory {
    pub(super) fn open(config: &Path, id: &str) -> io::Result<Self> {
        check_name(id)?;
        let root = open_root(config)?;
        let path = config.join("companion-packs").join(id);
        let directory = open_directory(&path)?;
        Ok(Self {
            path,
            _root: root,
            _directory: directory,
        })
    }

    pub(super) fn asset(&self, name: &str) -> io::Result<File> {
        check_name(name)?;
        hypermesh_local_storage::open(
            &self.path.join(name),
            hypermesh_local_storage::Open::Read { private: false },
        )
    }
}

fn open_directory(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let info = file.metadata()?;
    if !info.is_dir() || info.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a local pack directory",
        ));
    }
    Ok(file)
}

fn check_name(name: &str) -> io::Result<()> {
    // DOS device aliases also apply when followed by an extension.
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let numbered_device = stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.as_bytes()[3].is_ascii_digit();
    if name.ends_with('.')
        || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || numbered_device
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Windows pack name",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check_name;

    #[test]
    fn refuses_windows_aliases() {
        for name in ["CON", "nul.png", "a.", "lpt9.wav", "com1"] {
            assert!(check_name(name).is_err());
        }
        assert!(check_name("moon-rabbit.png").is_ok());
    }
}
