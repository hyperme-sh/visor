//! Native request contract against the immutable first animal-pack catalog.

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Map, Value};

use super::{catalog, packs, request_from, storage, MAX_RESPONSE_BYTES};

#[path = "../ci/fixtures/published_companion_catalog.rs"]
mod fixture;
use fixture::{read_fixture, ASSETS, SOURCE};

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
    let read = |path: &str, limit| {
        assert!(
            !offline.get(),
            "an offline pack request tried to fetch {path}"
        );
        read_fixture(Path::new(&fixture), path, limit)
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
    let load = |medium| request(json!({"action":"pack-load", "medium":medium}));
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
                           "voice":voice,"cues":cues,
                           "after_appearance_removal":{"appearance":bundled,"voice":remaining_voice},
                           "after_voice_removal":{"appearance":bundled,"voice":muted}});
    fs::write(output, serde_json::to_vec(&responses).unwrap()).unwrap();
}
