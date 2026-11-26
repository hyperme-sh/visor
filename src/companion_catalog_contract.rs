//! Native request contract against the immutable first animal-pack catalog.

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Map, Value};

use super::{catalog, packs, request_from, storage, MAX_RESPONSE_BYTES};
use catalog::read_download;

#[path = "../ci/fixtures/published_companion_catalog.rs"]
mod fixture;
use fixture::{read_with_fault, CatalogFault, DownloadFault, ASSETS, SOURCE};

struct TempConfig(PathBuf);

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "requires the published catalog fixture checked out by native CI"]
fn published_packs_native_contract() {
    let fixture = std::env::var_os("HYPERMESH_CATALOG_FIXTURE").expect("CI catalog fixture path");
    let output = std::env::var_os("HYPERMESH_CATALOG_CONTRACT_OUTPUT").expect("CI response path");
    let config =
        TempConfig(std::env::temp_dir().join(format!("catalog-contract-{}", uuid::Uuid::new_v4())));
    storage::create_dir(&config.0).unwrap();
    let offline = Cell::new(false);
    let fault = Cell::new(None);
    let read = |path: &str, limit| {
        assert!(
            !offline.get(),
            "an offline pack request tried to fetch {path}"
        );
        read_with_fault(Path::new(&fixture), path, limit, fault.get())
    };
    let source = catalog::CatalogSource::new(&read);
    let send = |input: Value| {
        let response = request_from(&config.0, input.clone(), &source);
        assert_eq!(response["status"], 200, "{input}");
        assert!(serde_json::to_vec(&response).unwrap().len() <= MAX_RESPONSE_BYTES);
        response.get("body").unwrap().clone()
    };
    let request = |input| {
        let body = send(input);
        assert!(body.get("error").is_none(), "{body}");
        body
    };
    let load = |medium| request(json!({"action":"pack-load", "medium":medium}));
    let listing = request(json!({"action":"pack-catalog-list"}));
    assert_eq!(listing["source_revision"], SOURCE);
    assert_eq!(listing["asset_revision"], ASSETS);
    let entries = listing["packs"].as_array().unwrap();
    let mut previews = Map::new();
    for (id, name, license, voice_expected) in [
        ("ember-cat", "Ember Cat", "CC0-1.0", true),
        ("scout-dog", "Scout Dog", "CC0-1.0", true),
        ("clover-rabbit", "Clover Rabbit", "CC0-1.0", true),
        ("visor-beetle", "Visor Beetle", "MIT", false),
    ] {
        let entry = entries.iter().find(|entry| entry["id"] == id).expect(id);
        assert_eq!(entry["name"], name);
        assert_eq!(entry["license"], license);
        assert_eq!(entry["installed"], false);
        let preview = |medium| {
            request(json!({"action":"pack-catalog-preview", "entry":entry,
                                               "revision":ASSETS, "medium":medium}))
        };
        let art = preview("art")["media"].clone();
        let decode = |value: &Value| STANDARD.decode(value.as_str().unwrap()).unwrap();
        packs::read_png(&decode(&art), Some(packs::SPRITE_SIZE)).unwrap();
        let voice = if voice_expected {
            let wave = preview("voice")["media"].clone();
            let pcm = packs::decode_wave(&decode(&wave)).unwrap();
            assert!(
                pcm.pcm.iter().any(|&byte| byte != 0),
                "{id} preview is silent"
            );
            wave
        } else {
            assert!(entry["voice_preview"].is_null());
            Value::Null
        };
        previews.insert(id.into(), json!({"art":art,"voice":voice}));

        let install = |entry: &Value| {
            send(json!({"action":"pack-catalog-install", "entry":entry,
                                                   "revision":ASSETS, "replace":false}))
        };
        if id == "ember-cat" {
            for case in DownloadFault::CASES {
                fault.set(Some(("ember-cat.pack.json", case)));
                let rejected = install(entry);
                assert!(
                    rejected["error"]
                        .as_str()
                        .is_some_and(|error| error.contains(case.expected_error())),
                    "{rejected}"
                );
                assert!(!config.0.join("companion-packs/ember-cat").exists());
            }
            fault.set(None);
            let mut wrong_digest = entry.clone();
            wrong_digest["archive"]["sha256"] = json!("0".repeat(64));
            let rejected = install(&wrong_digest);
            assert!(
                rejected["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("SHA-256")),
                "{rejected}"
            );
            assert!(!config.0.join("companion-packs").join(id).exists());
        }
        assert_eq!(install(entry)["changed"], true);
    }
    let refreshed = request(json!({"action":"pack-catalog-list"}));
    for entry in refreshed["packs"].as_array().unwrap() {
        assert_eq!(
            entry["installed"],
            previews.contains_key(entry["id"].as_str().unwrap())
        );
    }

    offline.set(true);
    let installed = request(json!({"action":"pack-list"}));
    for (id, name) in [
        ("ember-cat", "Ember Cat"),
        ("scout-dog", "Scout Dog"),
        ("clover-rabbit", "Clover Rabbit"),
    ] {
        let choice = json!([id, name]);
        assert!(installed["appearance"]
            .as_array()
            .unwrap()
            .contains(&choice));
        assert!(installed["voice"].as_array().unwrap().contains(&choice));
    }
    let beetle = json!(["visor-beetle", "Visor Beetle"]);
    assert!(installed["appearance"]
        .as_array()
        .unwrap()
        .contains(&beetle));
    assert!(!installed["voice"].as_array().unwrap().contains(&beetle));
    for (medium, id) in [("appearance", "visor-beetle"), ("voice", "clover-rabbit")] {
        let selected = request(json!({"action":"pack-select", "medium":medium, "id":id}));
        assert_eq!(selected["id"], id);
    }

    let rabbit_entry = refreshed["packs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "clover-rabbit")
        .unwrap();
    let rabbit_dir = config.0.join("companion-packs/clover-rabbit");
    let working = rabbit_dir.join("working.wav");
    let original = fs::read(&working).unwrap();
    let edited = fs::read(rabbit_dir.join("sleeping.wav")).unwrap();
    assert_ne!(edited, original);
    fs::write(&working, &edited).unwrap();
    let known_good = load("voice");
    assert_eq!(known_good["id"], "clover-rabbit");
    assert_ne!(
        known_good["audio"]["working.wav"],
        previews["clover-rabbit"]["voice"]
    );
    let replace = |entry: &Value, confirmed| {
        send(json!({"action":"pack-catalog-install", "entry":entry,
                    "revision":ASSETS, "replace":confirmed}))
    };
    let rejected = replace(rabbit_entry, false);
    assert!(
        rejected["error"]
            .as_str()
            .is_some_and(|error| error.contains("Confirm replacement")),
        "{rejected}"
    );
    assert_eq!(fs::read(&working).unwrap(), edited);
    offline.set(false);
    let mut corrupt = rabbit_entry.clone();
    corrupt["archive"]["sha256"] = json!("0".repeat(64));
    let rejected = replace(&corrupt, true);
    assert!(
        rejected["error"]
            .as_str()
            .is_some_and(|error| error.contains("SHA-256")),
        "{rejected}"
    );
    assert_eq!(fs::read(&working).unwrap(), edited);
    for case in DownloadFault::CASES {
        fault.set(Some(("clover-rabbit.pack.json", case)));
        let rejected = replace(rabbit_entry, true);
        assert!(
            rejected["error"]
                .as_str()
                .is_some_and(|error| error.contains(case.expected_error())),
            "{rejected}"
        );
        offline.set(true);
        assert_eq!(load("voice"), known_good);
        assert_eq!(fs::read(&working).unwrap(), edited);
        offline.set(false);
    }
    fault.set(None);

    // Stage a valid published update, then stop after moving the last working
    // copy aside and before renaming the candidate into the live directory.
    let pack_root = config.0.join("companion-packs");
    let stage = pack_root.join(format!(".catalog-{}", uuid::Uuid::new_v4()));
    storage::create_dir(&stage).unwrap();
    let staged = request_from(
        &stage,
        json!({"action":"pack-catalog-install", "entry":rabbit_entry,
                              "revision":ASSETS, "replace":false}),
        &source,
    );
    assert_eq!(staged["body"]["changed"], true, "{staged}");
    let staged_working = stage.join("companion-packs/clover-rabbit/working.wav");
    assert_eq!(fs::read(&staged_working).unwrap(), original);
    let backup = pack_root.join(".previous-clover-rabbit");
    fs::rename(&rabbit_dir, &backup).unwrap();
    assert!(!rabbit_dir.exists());
    offline.set(true);
    assert_eq!(load("voice"), known_good);
    let recovered_appearance = load("appearance");
    assert_eq!(recovered_appearance["id"], "visor-beetle");
    assert_eq!(fs::read(&working).unwrap(), edited);
    assert_eq!(fs::read(staged_working).unwrap(), original);
    assert!(!backup.exists());

    let expected_choices = request(json!({"action":"pack-list"}));
    let mut catalog_failures = Vec::new();
    for case in CatalogFault::CASES {
        let read_fault = |path: &str, limit| case.read(Path::new(&fixture), path, limit);
        let response = request_from(
            &config.0,
            json!({"action":"pack-catalog-list"}),
            &catalog::CatalogSource::new(&read_fault),
        );
        assert_eq!(response["status"], 200);
        let body = &response["body"];
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|error| error.contains(case.expected_error())),
            "{case:?}: {body}"
        );
        assert!(body.get("packs").is_none());
        catalog_failures.push(body.clone());

        // The healthy transport remains disabled while installed choices and
        // their media are read or reselected through the native requests.
        assert_eq!(request(json!({"action":"pack-list"})), expected_choices);
        for (medium, expected) in [
            ("appearance", &recovered_appearance),
            ("voice", &known_good),
        ] {
            assert_eq!(&load(medium), expected);
            assert_eq!(
                &request(json!({"action":"pack-select", "medium":medium, "id":expected["id"]})),
                expected
            );
        }
        assert_eq!(fs::read(&working).unwrap(), edited);
    }
    let recovered_voice = load("voice");

    offline.set(false);
    assert_eq!(replace(rabbit_entry, true)["changed"], true);
    assert_eq!(fs::read(&working).unwrap(), original);
    assert!(!config
        .0
        .join("companion-packs/.previous-clover-rabbit")
        .exists());
    offline.set(true);
    assert_eq!(
        request(json!({"action":"pack-list"}))["selected"],
        json!({"appearance":"visor-beetle", "voice":"clover-rabbit"})
    );
    let appearance = request(json!({"action":"pack-load", "medium":"appearance"}));
    let voice = request(json!({"action":"pack-load", "medium":"voice"}));
    assert_eq!(appearance["id"], "visor-beetle");
    assert_eq!(appearance["frames"].as_object().unwrap().len(), 38);
    assert_eq!(appearance["cursor"].as_object().unwrap().len(), 12);
    assert_eq!(voice["id"], "clover-rabbit");
    assert_eq!(
        voice["audio"]["working.wav"],
        previews["clover-rabbit"]["voice"]
    );
    let cues = request(json!({"action":"pack-cues","state":"automation",
                              "cursor":{"clock_ms":0,"reduced_motion":true}}));
    assert_eq!(cues["keys"][0], "state:automation");

    let remove = |id| {
        assert_eq!(
            request(json!({"action":"pack-catalog-remove", "id":id}))["changed"],
            true
        );
        assert!(!config.0.join("companion-packs").join(id).exists());
    };
    remove("visor-beetle");
    let remaining = request(json!({"action":"pack-list"}));
    assert_eq!(
        remaining["selected"],
        json!({"appearance":"", "voice":"clover-rabbit"})
    );
    assert!(!remaining["appearance"]
        .as_array()
        .unwrap()
        .contains(&beetle));
    let bundled = load("appearance");
    assert_eq!(bundled["id"], "");
    assert!(bundled.get("frames").is_none());
    let remaining_voice = load("voice");
    assert_eq!(remaining_voice, voice);

    remove("clover-rabbit");
    let remaining = request(json!({"action":"pack-list"}));
    assert_eq!(remaining["selected"], json!({"appearance":"", "voice":""}));
    for medium in ["appearance", "voice"] {
        assert_eq!(
            remaining[medium],
            json!([["ember-cat", "Ember Cat"], ["scout-dog", "Scout Dog"]])
        );
    }
    let muted = load("voice");
    assert_eq!(muted["id"], "");
    assert!(muted.get("audio").is_none());

    for medium in ["appearance", "voice"] {
        assert_eq!(
            request(json!({"action":"pack-select", "medium":medium, "id":"ember-cat"}))["id"],
            "ember-cat"
        );
    }
    remove("ember-cat");
    assert_eq!(load("appearance"), bundled);
    assert_eq!(load("voice"), muted);
    let responses = json!({"catalog":listing,"previews":previews,"appearance":appearance,
                           "voice":voice,"cues":cues,"recovered_voice":recovered_voice,
                           "catalog_failures":catalog_failures,
                           "after_appearance_removal":{"appearance":bundled,"voice":remaining_voice},
                           "after_voice_removal":{"appearance":bundled,"voice":muted}});
    fs::write(output, serde_json::to_vec(&responses).unwrap()).unwrap();
}
