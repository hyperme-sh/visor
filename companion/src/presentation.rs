//! Local, data-only companion packs and the presentation signals they can read.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde::Deserialize;

use crate::auth::Pose;
use crate::mascot::Mascot;

const MAX_MANIFEST_BYTES: usize = 32 * 1024;
pub const MAX_BINDINGS: usize = 64;

pub struct Signals<'a> {
    state: &'static str,
    pub label: &'static str,
    verb: Option<&'a str>,
    mode: Option<&'a str>,
    mood: Option<&'a str>,
    vibe: Option<&'a str>,
}

impl<'a> Signals<'a> {
    pub fn new(
        mascot: Mascot,
        paused: bool,
        pose: &'a Pose,
        mood: Option<&'a str>,
        vibe: Option<&'a str>,
    ) -> Self {
        let (state, label) = match mascot {
            Mascot::Upgrading => ("upgrading", "Updating"),
            Mascot::Disconnected => ("disconnected", "Disconnected"),
            _ if paused => ("paused", "Paused"),
            Mascot::Automation => ("automation", "Automation running"),
            Mascot::Sleeping => ("sleeping", "Sleeping"),
            Mascot::Idle => ("idle", "Idle"),
        };
        let active = matches!(state, "idle" | "automation");
        let (verb, mode) = match pose {
            Pose::Active { verb, mode, .. } if active => (verb.as_deref(), mode.as_deref()),
            _ => (None, None),
        };
        Self {
            state,
            label,
            verb,
            mode,
            mood: mood.filter(|_| active),
            vibe: vibe.filter(|_| active),
        }
    }

    /// Resolve independently for each medium. An unavailable binding is omitted
    /// from the choices by the loader, so the same ordering covers partial packs.
    pub fn select<T>(&self, bindings: &[(String, T)]) -> Option<usize> {
        bindings
            .iter()
            .enumerate()
            .filter_map(|(index, (key, _))| self.priority(key).map(|rank| (rank, index)))
            .min()
            .map(|(_, index)| index)
    }

    fn priority(&self, key: &str) -> Option<u8> {
        match key.split_once(':') {
            Some(("mode", value)) if self.mode == Some(value) => Some(0),
            Some(("mode", "mcp:*")) if self.mode.is_some_and(|mode| mode.starts_with("mcp:")) => {
                Some(1)
            }
            Some(("verb", value)) if self.verb == Some(value) => Some(2),
            Some(("state", value)) if self.state == value => Some(3),
            Some(("mood", value)) if self.mood == Some(value) => Some(4),
            Some(("vibe", value)) if self.vibe == Some(value) => Some(5),
            None if key == "default" => Some(6),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipSpec {
    pub frames: Vec<String>,
    pub ms_per_frame: Option<u64>,
    #[serde(rename = "loop", default)]
    pub looped: bool,
}

#[derive(Deserialize)]
pub struct Manifest {
    format: u32,
    pub name: String,
    #[serde(default)]
    pub sprites: BTreeMap<String, Option<serde_json::Value>>,
    #[serde(default)]
    pub cursor: BTreeMap<String, Option<serde_json::Value>>,
    #[serde(default)]
    pub voice: BTreeMap<String, Option<serde_json::Value>>,
}

pub struct Pack {
    directory: File,
    pub manifest: Manifest,
}

impl Pack {
    pub fn load(config: &Path, id: &str) -> Result<Self, String> {
        if !identifier(id) {
            return Err("expected a pack directory name".into());
        }
        let flags = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(flags)
            .open(config.join("companion-packs"))
            .map_err(|error| format!("opening companion-packs: {error}"))?;
        let directory = open_child(&root, id, flags).map_err(|error| error.to_string())?;
        let bytes = read_asset(&directory, "pack.json", MAX_MANIFEST_BYTES)
            .map_err(|error| format!("reading pack.json: {error}"))?;
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid pack.json: {error}"))?;
        if manifest.format != 1 {
            return Err("unsupported pack format".into());
        }
        if manifest.name.trim().is_empty()
            || manifest.name.len() > 128
            || manifest.name.chars().any(char::is_control)
        {
            return Err("expected a display name of 1 to 128 UTF-8 bytes".into());
        }
        if [&manifest.sprites, &manifest.cursor, &manifest.voice]
            .iter()
            .any(|bindings| bindings.len() > MAX_BINDINGS)
        {
            return Err(format!("limit of {MAX_BINDINGS} bindings per medium"));
        }
        Ok(Self {
            directory,
            manifest,
        })
    }

    pub fn asset(&self, name: &str, extension: &str, limit: usize) -> io::Result<Vec<u8>> {
        if !name.ends_with(extension) || !identifier(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid asset name",
            ));
        }
        read_asset(&self.directory, name, limit)
    }
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

pub fn valid_key(key: &str) -> bool {
    key == "default"
        || match key.split_once(':') {
            Some(("mode" | "state" | "verb" | "mood" | "vibe", value)) => {
                !value.is_empty() && value.len() <= 260 && !value.chars().any(char::is_control)
            }
            _ => false,
        }
}

fn open_child(parent: &File, name: &str, flags: libc::c_int) -> io::Result<File> {
    let name = CString::new(name).map_err(io::Error::other)?;
    // SAFETY: the parent and NUL-terminated name stay live across openat. No
    // creation flags are passed. A successful descriptor is owned by the File.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags | libc::O_RDONLY) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn read_asset(parent: &File, name: &str, limit: usize) -> io::Result<Vec<u8>> {
    let file = open_child(
        parent,
        name,
        libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
    )?;
    let info = file.metadata()?;
    if !info.is_file() || info.len() > limit as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a bounded regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "asset exceeds its size limit",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_bindings_use_the_next_available_signal() {
        let pose = Pose::Active {
            purpose: "not a presentation input".into(),
            verb: Some("type".into()),
            mode: Some("mcp:editor".into()),
        };
        let bindings = [
            ("default".into(), ()),
            ("vibe:soft".into(), ()),
            ("mood:curious".into(), ()),
            ("state:automation".into(), ()),
            ("verb:type".into(), ()),
            ("mode:mcp:*".into(), ()),
            ("mode:mcp:editor".into(), ()),
        ];
        let signals = Signals::new(
            Mascot::Automation,
            false,
            &pose,
            Some("curious"),
            Some("soft"),
        );
        for len in 1..=bindings.len() {
            assert_eq!(signals.select(&bindings[..len]), Some(len - 1));
        }
        let paused = Signals::new(Mascot::Idle, true, &pose, Some("curious"), Some("soft"));
        assert_eq!(paused.select(&bindings), Some(0));
        assert_eq!(paused.select(&[("state:paused".into(), ())]), Some(0));
        let upgrading = Signals::new(Mascot::Upgrading, true, &pose, None, None);
        assert_eq!(upgrading.select(&[("state:paused".into(), ())]), None);
        assert_eq!(upgrading.select::<()>(&[]), None);
    }
}
