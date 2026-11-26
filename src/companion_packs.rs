//! Local, data-only companion packs and the presentation signals they can read.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

use serde::Deserialize;

#[path = "companion_pack_cursor.rs"]
pub mod cursor;
#[path = "companion_pack_media.rs"]
mod media;
pub use media::{decode_wave, read_png, PcmWave, Png};

const MAX_MANIFEST_BYTES: usize = 32 * 1024;
const MAX_BINDINGS: usize = 64;
pub const SPRITE_SIZE: (u32, u32) = (400, 520);
pub const CURSOR_SIZE: (u32, u32) = (32, 32);
pub const MAX_DISTINCT_SPRITES: usize = 64;
pub const MAX_DISTINCT_VOICES: usize = 16;
pub const DEFAULT_FRAME_MS: u64 = 150;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Idle,
    Automation,
    Paused,
    Sleeping,
    Disconnected,
    Upgrading,
}

impl State {
    fn presentation(self) -> (&'static str, &'static str) {
        match self {
            Self::Idle => ("idle", "Idle"),
            Self::Automation => ("automation", "Automation running"),
            Self::Paused => ("paused", "Paused"),
            Self::Sleeping => ("sleeping", "Sleeping"),
            Self::Disconnected => ("disconnected", "Disconnected"),
            Self::Upgrading => ("upgrading", "Updating"),
        }
    }
}

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
        state: State,
        verb: Option<&'a str>,
        mode: Option<&'a str>,
        mood: Option<&'a str>,
        vibe: Option<&'a str>,
    ) -> Self {
        let (state, label) = state.presentation();
        let active = matches!(state, "idle" | "automation");
        Self {
            state,
            label,
            verb: verb.filter(|_| active),
            mode: mode.filter(|_| active),
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

    /// Keys to try in the same order used by `select`.
    pub fn ordered_selectors(&self) -> Vec<String> {
        let mut keys = Vec::with_capacity(7);
        if let Some(mode) = self.mode {
            keys.push(format!("mode:{mode}"));
            if mode.starts_with("mcp:") && mode != "mcp:*" {
                keys.push("mode:mcp:*".into());
            }
        }
        for (prefix, value) in [
            ("verb", self.verb),
            ("state", Some(self.state)),
            ("mood", self.mood),
            ("vibe", self.vibe),
        ] {
            if let Some(value) = value {
                keys.push(format!("{prefix}:{value}"));
            }
        }
        keys.push("default".into());
        keys.sort_by_key(|key| self.priority(key));
        keys
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

impl ClipSpec {
    pub fn parse(entry: &serde_json::Value) -> Result<Option<Self>, String> {
        let spec: Self =
            serde_json::from_value(entry.clone()).map_err(|error| error.to_string())?;
        if spec.frames.is_empty() {
            return Ok(None);
        }
        if spec.frames.len() > 16 || !(40..=10_000).contains(&spec.frame_ms()) {
            return Err("expected at most 16 frames and 40 to 10000 ms per frame".into());
        }
        Ok(Some(spec))
    }

    pub fn frame_ms(&self) -> u64 {
        self.ms_per_frame.unwrap_or(DEFAULT_FRAME_MS)
    }
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
    directory: platform::Directory,
    pub manifest: Manifest,
}

impl Pack {
    pub fn load(config: &Path, id: &str) -> Result<Self, String> {
        if !identifier(id) {
            return Err("expected a pack directory name".into());
        }
        let directory = platform::Directory::open(config, id).map_err(|error| error.to_string())?;
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

    pub fn sprite(&self, name: &str) -> Result<Png, String> {
        let bytes = self
            .asset(name, ".png", 1024 * 1024)
            .map_err(|err| err.to_string())?;
        read_png(&bytes, Some(SPRITE_SIZE))
    }

    pub fn cursor(&self, name: &str) -> Result<Png, String> {
        let bytes = self
            .asset(name, ".png", 32 * 1024)
            .map_err(|err| err.to_string())?;
        read_png(&bytes, Some(CURSOR_SIZE))
    }

    pub fn voice(&self, name: &str) -> Option<PcmWave> {
        let bytes = self.asset(name, ".wav", 300 * 1024).ok()?;
        decode_wave(&bytes)
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

#[cfg(unix)]
#[path = "companion_pack_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "companion_pack_windows.rs"]
mod platform;

pub(crate) use platform::open_root as open_pack_root;

fn read_asset(parent: &platform::Directory, name: &str, limit: usize) -> io::Result<Vec<u8>> {
    let file = parent.asset(name)?;
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

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Medium {
    Appearance,
    Voice,
}

pub fn installed(config: &Path, medium: Medium) -> Result<Vec<(String, String)>, String> {
    crate::companion_catalog::recover_installs(config)?;
    let directory = config.join("companion-packs");
    let _root = match platform::open_root(config) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let appearance = medium == Medium::Appearance;
    let mut choices = Vec::new();
    for entry in fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .flatten()
    {
        let Ok(id) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(pack) = Pack::load(config, &id) else {
            continue;
        };
        let manifest = pack.manifest;
        if (appearance && (!manifest.sprites.is_empty() || !manifest.cursor.is_empty()))
            || (!appearance && !manifest.voice.is_empty())
        {
            choices.push((id, manifest.name));
        }
        if choices.len() == 128 {
            break;
        }
    }
    choices.sort();
    Ok(choices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_packs_share_manifest_and_media_validation() {
        let config =
            std::env::temp_dir().join(format!("visor-shared-pack-{}", uuid::Uuid::new_v4()));
        let dir = config.join("companion-packs/rabbit");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("pack.json"),
            r#"{"format":1,"name":"Rabbit","sprites":{"default":{"frames":["rest.png"]},"state:paused":null}}"#,
        ).unwrap();
        fs::write(
            dir.join("rest.png"),
            include_bytes!("../companion/assets/states/sleeping_0_400x520.png"),
        )
        .unwrap();
        fs::write(dir.join("bad.png"), b"invalid PNG").unwrap();
        assert_eq!(
            installed(&config, Medium::Appearance).unwrap(),
            vec![("rabbit".into(), "Rabbit".into())],
        );
        assert!(installed(&config, Medium::Voice).unwrap().is_empty());
        {
            let pack = Pack::load(&config, "rabbit").unwrap();
            let sprite = pack.sprite("rest.png").unwrap();
            assert_eq!((sprite.width, sprite.height), SPRITE_SIZE);
            assert!(pack.cursor("rest.png").is_err());
            assert!(pack.sprite("bad.png").is_err());
            assert!(pack.sprite("../rabbit/rest.png").is_err());
            assert!(pack.asset("rest.png", ".png", 1).is_err());
            let valid = pack.manifest.sprites["default"].as_ref().unwrap();
            assert_eq!(ClipSpec::parse(valid).unwrap().unwrap().frame_ms(), 150);
            assert!(pack.manifest.sprites["state:paused"].is_none());
        }
        fs::remove_dir_all(config).unwrap();
    }

    #[test]
    fn partial_bindings_use_the_next_available_signal() {
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
            State::Automation,
            Some("type"),
            Some("mcp:editor"),
            Some("curious"),
            Some("soft"),
        );
        for len in 1..=bindings.len() {
            assert_eq!(signals.select(&bindings[..len]), Some(len - 1));
        }
        let ordered: Vec<_> = bindings.iter().rev().map(|(key, ())| key.clone()).collect();
        assert_eq!(signals.ordered_selectors(), ordered);
        let paused = Signals::new(
            State::Paused,
            Some("type"),
            Some("mcp:editor"),
            Some("curious"),
            Some("soft"),
        );
        assert_eq!(paused.select(&bindings), Some(0));
        assert_eq!(paused.select(&[("state:paused".into(), ())]), Some(0));
        let upgrading = Signals::new(
            State::Upgrading,
            Some("type"),
            Some("mcp:editor"),
            None,
            None,
        );
        assert_eq!(upgrading.select(&[("state:paused".into(), ())]), None);
        assert_eq!(upgrading.select::<()>(&[]), None);
    }
}
