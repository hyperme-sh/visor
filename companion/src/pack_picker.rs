//! Offline picker for packs already in the local configuration directory.

use std::fs;
use std::io;
use std::path::Path;

use crate::draw::Sprites;
use crate::mcp_dialog::{choose, dialog};
use crate::pointer::Art;
use crate::presentation::Pack;
use crate::settings;
use crate::voice::Voice;

pub enum Selection {
    Appearance {
        id: String,
        sprites: Sprites,
        art: Art,
    },
    Voice {
        id: String,
        voice: Voice,
    },
}

pub fn load_appearance(pack: Option<&Pack>) -> (Sprites, Art) {
    let sprites = Sprites::load();
    let art = Art::load();
    match pack {
        Some(pack) => (sprites.with_pack(pack), art.with_pack(pack)),
        None => (sprites, art),
    }
}

pub fn run(config: &Path) -> Result<Option<Selection>, String> {
    pick(config, &mut dialog).map_err(|error| {
        if error.starts_with("Cannot open MCP controls") {
            "Cannot open the companion pack picker. Install zenity and run in your desktop session."
                .into()
        } else {
            error
        }
    })
}

fn pick(
    config: &Path,
    ui: &mut impl FnMut(&[String], Option<&str>) -> Result<Option<String>, String>,
) -> Result<Option<Selection>, String> {
    let tracks = ["Appearance".to_owned(), "Voice".to_owned()];
    let Some(track) = choose(ui, "Companion packs", "Choose what to change", &tracks)? else {
        return Ok(None);
    };
    let current = settings::presentation(config);
    let appearance = track == 0;
    let (default, selected) = if appearance {
        ("Bundled appearance", current.appearance)
    } else {
        ("Mute", current.voice)
    };
    let mut choices = installed(config, appearance)?;
    choices.insert(0, (String::new(), default.to_owned()));
    let rows: Vec<String> = choices
        .iter()
        .map(|(id, name)| {
            let suffix = if selected.as_deref().unwrap_or_default() == id {
                " (selected)"
            } else {
                ""
            };
            if id.is_empty() {
                format!("{name}{suffix}")
            } else {
                format!("{name} [{id}]{suffix}")
            }
        })
        .collect();
    let Some(index) = choose(ui, &tracks[track], "Choose an installed pack", &rows)? else {
        return Ok(None);
    };
    let (id, _) = choices.swap_remove(index);
    let pack = (!id.is_empty())
        .then(|| Pack::load(config, &id))
        .transpose()?;
    let selection = if appearance {
        let (sprites, art) = load_appearance(pack.as_ref());
        Selection::Appearance { id, sprites, art }
    } else {
        let voice = pack.as_ref().map_or_else(Voice::silent, Voice::load);
        Selection::Voice { id, voice }
    };
    Ok(Some(selection))
}

fn installed(config: &Path, appearance: bool) -> Result<Vec<(String, String)>, String> {
    let directory = config.join("companion-packs");
    match fs::symlink_metadata(&directory) {
        Ok(info) if info.is_dir() => {}
        Ok(_) => return Err("companion-packs must be a local directory".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    }
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
    fn choices_are_offline_and_independent() {
        let config = std::env::temp_dir().join(format!("visor-picker-{}", uuid::Uuid::new_v4()));
        for (id, body) in [
            (
                "rabbit",
                r#"{"format":1,"name":"Rabbit","sprites":{"default":{"frames":["rabbit.png"]}}}"#,
            ),
            (
                "robot",
                r#"{"format":1,"name":"Robot","voice":{"default":"hello.wav"}}"#,
            ),
        ] {
            let dir = config.join("companion-packs").join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("pack.json"), body).unwrap();
        }
        assert_eq!(
            installed(&config, true).unwrap(),
            vec![("rabbit".to_owned(), "Rabbit".to_owned())]
        );
        assert_eq!(
            installed(&config, false).unwrap(),
            vec![("robot".to_owned(), "Robot".to_owned())]
        );
        settings::set_appearance_pack(&config, "rabbit").unwrap();
        settings::set_voice_pack(&config, "robot").unwrap();
        let mut call = 0;
        let change = pick(&config, &mut |args, _| {
            call += 1;
            if call == 1 {
                Ok(Some("1".into()))
            } else {
                assert!(args.contains(&"Mute".into()));
                assert!(args.contains(&"Robot [robot] (selected)".into()));
                Ok(Some("0".into()))
            }
        })
        .unwrap();
        let Some(Selection::Voice { id, .. }) = change else {
            panic!("expected voice selection")
        };
        settings::set_voice_pack(&config, &id).unwrap();
        let selected = settings::presentation(&config);
        fs::remove_dir_all(config).unwrap();
        assert_eq!(selected.appearance.as_deref(), Some("rabbit"));
        assert_eq!(selected.voice, None);
    }
}
