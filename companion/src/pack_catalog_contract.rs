//! Published packs exercised through Linux companion dialogs and presentation.

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use hypermesh_visor::companion_catalog::{CatalogSource, Preview};
use hypermesh_visor::companion_packs::{decode_wave, read_png, PcmWave, SPRITE_SIZE};

use super::{load_appearance, load_saved, pick, settings, Selection, Voice};
use crate::{auth::Pose, mascot::Mascot, pack_catalog_dialog::browse_from, presentation};

#[path = "../../ci/fixtures/published_companion_catalog.rs"]
mod fixture;

const PACKS: [(&str, &str, bool); 4] = [
    ("ember-cat", "Ember Cat", true),
    ("scout-dog", "Scout Dog", true),
    ("clover-rabbit", "Clover Rabbit", true),
    ("visor-beetle", "Visor Beetle", false),
];

struct TempConfig(PathBuf);

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn row(args: &[String], label: &str) -> String {
    let start = args.iter().position(|arg| arg == "--").unwrap();
    args[start + 1..]
        .chunks_exact(2)
        .find(|pair| pair[1].starts_with(label))
        .unwrap_or_else(|| panic!("missing {label:?} in {args:?}"))[0]
        .clone()
}

fn browse_pack(
    config: &Path,
    source: &CatalogSource<'_>,
    name: &str,
    previews: Option<bool>,
) -> Result<Option<PcmWave>, String> {
    let label = format!("{name} — ");
    let mut replies = vec![label.as_str()];
    if let Some(has_voice) = previews {
        replies.push("Preview artwork");
        if has_voice {
            replies.push("Play voice preview");
        }
    }
    replies.push("Download and install");
    let mut replies = replies.into_iter();
    let mut art_seen = false;
    let mut voice = None;
    browse_from(
        config,
        &mut |args, _| {
            if args.contains(&"--title=Community packs".into()) {
                for (_, name, has_voice) in PACKS {
                    let license = if has_voice { "(CC0-1.0)" } else { "(MIT)" };
                    let label = args
                        .iter()
                        .find(|item| item.starts_with(&format!("{name} — ")))
                        .unwrap();
                    assert!(label.ends_with(license), "{label}");
                }
            }
            if previews == Some(false) {
                assert!(!args.iter().any(|arg| arg == "Play voice preview"));
            }
            if args.contains(&"--info".into()) {
                assert!(args.contains(&"--title=Pack installed".into()));
                return Ok(None);
            }
            Ok(Some(row(
                args,
                replies.next().expect("expected catalog dialog"),
            )))
        },
        source,
        false,
        |medium, bytes| {
            match medium {
                Preview::Art => {
                    read_png(bytes, Some(SPRITE_SIZE)).unwrap();
                    art_seen = true;
                }
                Preview::Voice => {
                    let wave = decode_wave(bytes).expect("valid WAV preview");
                    assert!(wave.pcm.iter().any(|&byte| byte != 0));
                    voice = Some(wave);
                }
            }
            Ok(())
        },
    )?;
    assert!(replies.next().is_none());
    assert_eq!(art_seen, previews.is_some());
    assert_eq!(voice.is_some(), previews == Some(true));
    Ok(voice)
}

#[test]
#[ignore = "requires the published catalog fixture checked out by Linux CI"]
fn published_packs_linux_contract() {
    let root = std::env::var_os("HYPERMESH_CATALOG_FIXTURE").expect("CI catalog fixture path");
    let config = TempConfig(
        std::env::temp_dir().join(format!("companion-catalog-{}", uuid::Uuid::new_v4())),
    );
    hypermesh_local_storage::create_dir(&config.0).unwrap();
    let corrupt = Cell::new(true);
    let offline = Cell::new(false);
    let read = |path: &str, limit| {
        assert!(!offline.get(), "offline companion tried to fetch {path}");
        let data = fixture::read_fixture(Path::new(&root), path, limit)?;
        if path != "catalog/catalog.json" {
            return Ok(data);
        }
        let mut index: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(index["source_revision"], fixture::SOURCE);
        assert_eq!(index["asset_revision"], fixture::ASSETS);
        if !corrupt.get() {
            return Ok(data);
        }
        let cat = index["packs"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|pack| pack["id"] == "ember-cat")
            .unwrap();
        cat["archive"]["sha256"] = serde_json::json!("0".repeat(64));
        Ok(serde_json::to_vec(&index).unwrap())
    };
    let source = CatalogSource::new(&read);
    let rejected = browse_pack(&config.0, &source, "Ember Cat", None)
        .err()
        .unwrap();
    assert!(rejected.contains("SHA-256"), "{rejected}");
    assert!(!config.0.join("companion-packs/ember-cat").exists());
    corrupt.set(false);
    let mut rabbit_preview = None;
    for (id, name, has_voice) in PACKS {
        let preview = browse_pack(&config.0, &source, name, Some(has_voice)).unwrap();
        assert!(config
            .0
            .join("companion-packs")
            .join(id)
            .join("pack.json")
            .is_file());
        if id == "clover-rabbit" {
            rabbit_preview = preview;
        }
    }
    let rabbit_preview = rabbit_preview.unwrap();

    offline.set(true);
    let select = |medium, pack| {
        let mut choices = [medium, pack].into_iter();
        pick(&config.0, &mut |args, _| {
            Ok(Some(row(
                args,
                choices.next().expect("expected installed-pack dialog"),
            )))
        })
        .unwrap()
        .unwrap()
    };
    let Selection::Appearance { id, .. } = select("Appearance", "Visor Beetle [visor-beetle]")
    else {
        panic!("expected installed appearance");
    };
    settings::set_appearance_pack(&config.0, &id).unwrap();
    let Selection::Voice { id, .. } = select("Voice", "Clover Rabbit [clover-rabbit]") else {
        panic!("expected installed voice");
    };
    settings::set_voice_pack(&config.0, &id).unwrap();
    let saved = settings::presentation(&config.0);
    assert_eq!(saved.appearance.as_deref(), Some("visor-beetle"));
    assert_eq!(saved.voice.as_deref(), Some("clover-rabbit"));
    let (beetle, rabbit) = load_saved(
        &config.0,
        saved.appearance.as_deref(),
        saved.voice.as_deref(),
    );
    let beetle = beetle.unwrap();
    let rabbit = rabbit.unwrap();
    let signals = presentation::signals(Mascot::Automation, false, &Pose::Idle, None, None);
    let (mut sprites, cursor) = load_appearance(Some(&beetle));
    assert!(
        sprites.select(&signals),
        "expected a custom clip for Visor Beetle"
    );
    assert_eq!(sprites.timing(Mascot::Automation), (90, 8, true));
    assert_eq!(
        cursor.frame("pointer").rgba,
        beetle.cursor("pointer_32.png").unwrap().rgba
    );
    let voice = Voice::load(&rabbit);
    let cue = voice.cue_for(&signals).expect("Clover Rabbit working cue");
    assert_eq!(cue.rate, rabbit_preview.rate);
    assert_eq!(cue.pcm, rabbit_preview.pcm);
}
