//! Installed pack choices and the entry point for browsing the community catalog.

use std::path::Path;

use crate::draw::Sprites;
use crate::mcp_dialog::{choose, dialog};
use crate::pointer::Art;
use crate::settings;
use crate::voice::Voice;
use hypermesh_visor::companion_catalog;
use hypermesh_visor::companion_packs::{installed, Medium, Pack};

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
    Removed {
        id: String,
    },
}

pub fn load_saved(
    config: &Path,
    appearance: Option<&str>,
    voice: Option<&str>,
) -> (Option<Pack>, Option<Pack>) {
    if let Err(error) = companion_catalog::recover_installs(config) {
        eprintln!("hypermesh-companion: companion pack recovery: {error}");
    }
    let load = |setting, id: Option<&str>| match Pack::load(config, id?) {
        Ok(pack) => Some(pack),
        Err(error) => {
            eprintln!("hypermesh-companion: {setting}: {error}; using the default");
            None
        }
    };
    (load("appearance_pack", appearance), load("voice_pack", voice))
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
    let tracks = [
        "Appearance",
        "Voice",
        "Browse community packs",
        "Remove installed pack",
    ]
    .map(str::to_owned);
    let Some(track) = choose(ui, "Companion packs", "Choose what to change", &tracks)? else {
        return Ok(None);
    };
    if track == 2 {
        crate::pack_catalog_dialog::browse(config, ui)?;
        return Ok(None);
    }
    if track == 3 {
        return crate::pack_catalog_dialog::remove(config, ui)
            .map(|id| id.map(|id| Selection::Removed { id }));
    }
    let current = settings::presentation(config);
    let appearance = track == 0;
    let (default, selected) = if appearance {
        ("Bundled appearance", current.appearance)
    } else {
        ("Mute", current.voice)
    };
    let mut choices = installed(
        config,
        if appearance {
            Medium::Appearance
        } else {
            Medium::Voice
        },
    )?;
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

#[cfg(test)]
#[path = "pack_catalog_contract.rs"]
mod published_contract;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn startup_recovers_the_saved_appearance_and_voice_offline() {
        let config = std::env::temp_dir().join(format!("visor-picker-{}", uuid::Uuid::new_v4()));
        let root = config.join("companion-packs");
        for (id, name) in [("rabbit", "Rabbit"), ("robot", "Robot")] {
            let backup = root.join(format!(".previous-{id}"));
            fs::create_dir_all(&backup).unwrap();
            fs::write(
                backup.join("pack.json"),
                format!(r#"{{"format":1,"name":"{name}"}}"#),
            )
            .unwrap();
        }
        let (appearance, voice) = load_saved(&config, Some("rabbit"), Some("robot"));
        assert_eq!(appearance.unwrap().manifest.name, "Rabbit");
        assert_eq!(voice.unwrap().manifest.name, "Robot");
        assert!(root.join("rabbit").is_dir() && root.join("robot").is_dir());
        fs::remove_dir_all(config).unwrap();
    }

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
            installed(&config, Medium::Appearance).unwrap(),
            vec![("rabbit".to_owned(), "Rabbit".to_owned())]
        );
        assert_eq!(
            installed(&config, Medium::Voice).unwrap(),
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
