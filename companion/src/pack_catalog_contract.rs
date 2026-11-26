//! Published packs exercised through Linux companion dialogs and presentation.

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use hypermesh_visor::companion_catalog::{read_download, CatalogSource, Preview};
use hypermesh_visor::companion_packs::{decode_wave, read_png, PcmWave, SPRITE_SIZE};

use super::{apply_removed_pack, load_appearance, load_saved, pick, settings, Selection, Voice};
use crate::{auth::Pose, mascot::Mascot, pack_catalog_dialog::browse_from, presentation};

#[path = "../../ci/fixtures/published_companion_catalog.rs"]
mod fixture;
use fixture::DownloadFault;

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

fn replace_rabbit(
    config: &Path,
    source: &CatalogSource<'_>,
    confirmed: bool,
) -> Result<(), String> {
    let mut replies = ["Clover Rabbit — ", "Replace local copy"].into_iter();
    let mut prompted = false;
    let result = browse_from(
        config,
        &mut |args, _| {
            if args.contains(&"--question".into()) {
                assert!(args.contains(&"--title=Replace local pack?".into()));
                prompted = true;
                return Ok(confirmed.then(String::new));
            }
            if args.contains(&"--info".into()) {
                assert!(confirmed && args.contains(&"--title=Pack installed".into()));
                return Ok(None);
            }
            Ok(replies.next().map(|choice| row(args, choice)))
        },
        source,
        false,
        |_, _| panic!("replacement unexpectedly opened a preview"),
    );
    assert!(prompted && replies.next().is_none());
    result
}

#[test]
#[ignore = "requires the published catalog fixture checked out by Linux CI"]
fn published_packs_linux_contract() {
    let root = std::env::var_os("HYPERMESH_CATALOG_FIXTURE").expect("CI catalog fixture path");
    let config = TempConfig(
        std::env::temp_dir().join(format!("companion-catalog-{}", uuid::Uuid::new_v4())),
    );
    hypermesh_local_storage::create_dir(&config.0).unwrap();
    let corrupt = Cell::new(Some("ember-cat"));
    let offline = Cell::new(false);
    let fault = Cell::new(None);
    let archive_reads = Cell::new(0);
    let read = |path: &str, limit| {
        assert!(!offline.get(), "offline companion tried to fetch {path}");
        if path.ends_with(".pack.json") {
            archive_reads.set(archive_reads.get() + 1);
        }
        let data = fixture::read_with_fault(Path::new(&root), path, limit, fault.get())?;
        if path != "catalog/catalog.json" {
            return Ok(data);
        }
        let mut index: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(index["source_revision"], fixture::SOURCE);
        assert_eq!(index["asset_revision"], fixture::ASSETS);
        let Some(corrupt_id) = corrupt.get() else {
            return Ok(data);
        };
        let entry = index["packs"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|pack| pack["id"] == corrupt_id)
            .unwrap();
        entry["archive"]["sha256"] = serde_json::json!("0".repeat(64));
        Ok(serde_json::to_vec(&index).unwrap())
    };
    let source = CatalogSource::new(&read);
    let rejected = browse_pack(&config.0, &source, "Ember Cat", None)
        .err()
        .unwrap();
    assert!(rejected.contains("SHA-256"), "{rejected}");
    assert!(!config.0.join("companion-packs/ember-cat").exists());
    corrupt.set(None);
    for case in DownloadFault::CASES {
        fault.set(Some(("ember-cat.pack.json", case)));
        let rejected = browse_pack(&config.0, &source, "Ember Cat", None)
            .err()
            .unwrap();
        assert!(rejected.contains(case.expected_error()), "{rejected}");
        assert!(!config.0.join("companion-packs/ember-cat").exists());
    }
    fault.set(None);
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

    offline.set(false);
    let rabbit_dir = config.0.join("companion-packs/clover-rabbit");
    let working = rabbit_dir.join("working.wav");
    let original = fs::read(&working).unwrap();
    let edited = fs::read(rabbit_dir.join("sleeping.wav")).unwrap();
    assert_ne!(edited, original);
    fs::write(&working, &edited).unwrap();
    let before = archive_reads.get();
    replace_rabbit(&config.0, &source, false).unwrap();
    assert_eq!(archive_reads.get(), before);
    assert_eq!(fs::read(&working).unwrap(), edited);
    corrupt.set(Some("clover-rabbit"));
    let rejected = replace_rabbit(&config.0, &source, true).unwrap_err();
    assert!(rejected.contains("SHA-256"), "{rejected}");
    assert_eq!(fs::read(&working).unwrap(), edited);
    corrupt.set(None);

    let signals = presentation::signals(Mascot::Automation, false, &Pose::Idle, None, None);
    let edited_wave = decode_wave(&edited).unwrap();
    let assert_last_working_pair = || {
        let saved = settings::presentation(&config.0);
        assert_eq!(saved.appearance.as_deref(), Some("visor-beetle"));
        assert_eq!(saved.voice.as_deref(), Some("clover-rabbit"));
        let (appearance, voice) = load_saved(
            &config.0,
            saved.appearance.as_deref(),
            saved.voice.as_deref(),
        );
        assert_eq!(appearance.unwrap().manifest.name, "Visor Beetle");
        let voice = Voice::load(&voice.unwrap());
        let cue = voice
            .cue_for(&signals)
            .expect("last working Clover Rabbit cue");
        assert_eq!(cue.rate, edited_wave.rate);
        assert_eq!(cue.pcm, edited_wave.pcm);
        assert_eq!(fs::read(&working).unwrap(), edited);
    };
    for case in DownloadFault::CASES {
        fault.set(Some(("clover-rabbit.pack.json", case)));
        let rejected = replace_rabbit(&config.0, &source, true).unwrap_err();
        assert!(rejected.contains(case.expected_error()), "{rejected}");
        offline.set(true);
        assert_last_working_pair();
        offline.set(false);
    }
    fault.set(None);

    let pack_root = config.0.join("companion-packs");
    let stage = pack_root.join(format!(".catalog-{}", uuid::Uuid::new_v4()));
    hypermesh_local_storage::create_dir(&stage).unwrap();
    browse_pack(&stage, &source, "Clover Rabbit", None).unwrap();
    let staged_working = stage.join("companion-packs/clover-rabbit/working.wav");
    assert_eq!(fs::read(&staged_working).unwrap(), original);
    let backup = pack_root.join(".previous-clover-rabbit");
    fs::rename(&rabbit_dir, &backup).unwrap();
    assert!(!rabbit_dir.exists());
    offline.set(true);
    assert_last_working_pair();
    assert_eq!(fs::read(staged_working).unwrap(), original);
    assert!(!backup.exists());

    offline.set(false);
    replace_rabbit(&config.0, &source, true).unwrap();
    assert_eq!(fs::read(&working).unwrap(), original);
    assert!(!config
        .0
        .join("companion-packs/.previous-clover-rabbit")
        .exists());

    offline.set(true);
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
    let (mut sprites, mut cursor) = load_appearance(Some(&beetle));
    assert!(
        sprites.select(&signals),
        "expected a custom clip for Visor Beetle"
    );
    assert_eq!(sprites.timing(Mascot::Automation), (90, 8, true));
    assert_eq!(
        cursor.frame("pointer").rgba,
        beetle.cursor("pointer_32.png").unwrap().rgba
    );
    let mut voice = Voice::load(&rabbit);
    let cue = voice.cue_for(&signals).expect("Clover Rabbit working cue");
    assert_eq!(cue.rate, rabbit_preview.rate);
    assert_eq!(cue.pcm, rabbit_preview.pcm);
    drop((beetle, rabbit));

    let remove = |pack: &str, confirm: bool| {
        let mut choices = ["Remove installed pack", pack].into_iter();
        let mut prompted = false;
        let result = pick(&config.0, &mut |args, _| {
            if args.contains(&"--question".into()) {
                assert!(args.contains(&"--title=Remove local pack?".into()));
                prompted = true;
                return Ok(confirm.then(String::new));
            }
            Ok(Some(row(
                args,
                choices.next().expect("expected remove dialog"),
            )))
        })
        .unwrap();
        assert!(prompted && choices.next().is_none());
        result
    };
    assert!(remove("Visor Beetle [visor-beetle]", false).is_none());
    assert!(config
        .0
        .join("companion-packs/visor-beetle/pack.json")
        .is_file());
    assert_eq!(
        settings::presentation(&config.0).appearance,
        saved.appearance
    );

    let bundled = load_appearance(None);
    for (id, label, resets_appearance) in [
        ("visor-beetle", "Visor Beetle [visor-beetle]", true),
        ("clover-rabbit", "Clover Rabbit [clover-rabbit]", false),
    ] {
        let Some(Selection::Removed { id: removed }) = remove(label, true) else {
            panic!("expected confirmed removal of {id}");
        };
        assert_eq!(removed, id);
        assert!(!config.0.join("companion-packs").join(id).exists());
        assert_eq!(
            apply_removed_pack(&config.0, id, &mut sprites, &mut cursor, &mut voice),
            (resets_appearance, None)
        );
        let saved = settings::presentation(&config.0);
        assert!(saved.appearance.is_none());
        assert!(
            !sprites.select(&signals),
            "bundled art should have no custom cue"
        );
        assert_eq!(
            sprites.timing(Mascot::Automation),
            bundled.0.timing(Mascot::Automation)
        );
        assert_eq!(
            cursor.frame("pointer").rgba,
            bundled.1.frame("pointer").rgba
        );
        if resets_appearance {
            assert_eq!(saved.voice.as_deref(), Some("clover-rabbit"));
            assert_eq!(voice.cue_for(&signals).unwrap().pcm, rabbit_preview.pcm);
        } else {
            assert!(saved.voice.is_none());
            assert!(voice.cue_for(&signals).is_none());
        }
    }
    let saved = settings::presentation(&config.0);
    let (appearance, voice) = load_saved(
        &config.0,
        saved.appearance.as_deref(),
        saved.voice.as_deref(),
    );
    assert!(appearance.is_none() && voice.is_none());
}
