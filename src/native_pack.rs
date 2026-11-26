//! Companion presentation requests for the in-process native apps.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::companion_packs::cursor::{self, Motion};
use crate::companion_packs::{self as packs, ClipSpec, Medium, Pack, PcmWave, Png, Signals, State};
use hypermesh_local_storage as storage;

const CURSORS: [&str; 12] = [
    "pointer", "crawl", "fly", "reach", "click1", "click2", "type1", "type2", "busy1", "busy2",
    "busy3", "busy4",
];
const SETTINGS_LIMIT: usize = 256 * 1024;
pub(crate) const MAX_RESPONSE_BYTES: usize = 12 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(tag = "action")]
enum Request {
    #[serde(rename = "pack-list")]
    List,
    #[serde(rename = "pack-load")]
    Load { medium: Medium },
    #[serde(rename = "pack-select")]
    Select { medium: Medium, id: String },
    #[serde(rename = "pack-cues")]
    Cues {
        state: State,
        verb: Option<String>,
        mode: Option<String>,
        mood: Option<String>,
        vibe: Option<String>,
        cursor: Option<CursorSample>,
    },
}

#[derive(Deserialize)]
struct CursorSample {
    clock_ms: u64,
    reduced_motion: bool,
    motion: Option<Motion>,
}

pub(crate) fn request(config: &Path, input: Value) -> Value {
    let Ok(input) = serde_json::from_value::<Request>(input) else {
        return json!({"status":400,"body":null});
    };
    match run(config, input) {
        Ok(body) => json!({"status":200,"body":body}),
        Err(()) => json!({"status":409,"body":null}),
    }
}

fn run(config: &Path, input: Request) -> Result<Value, ()> {
    match input {
        Request::List => {
            let table = settings(config).map_err(|_| ())?;
            let appearance = packs::installed(config, Medium::Appearance).map_err(|_| ())?;
            let voice = packs::installed(config, Medium::Voice).map_err(|_| ())?;
            Ok(json!({ "appearance":appearance, "voice":voice, "selected":{
                "appearance": value(&table, "appearance_pack").unwrap_or_default(),
                "voice": value(&table, "voice_pack").unwrap_or_default(),
            }}))
        }
        Request::Load { medium } => {
            let table = settings(config).map_err(|_| ())?;
            let id = value(&table, setting(medium)).unwrap_or_default();
            // Saved packs can be removed or replaced while the app is closed.
            let loaded = load(config, medium, id).or_else(|()| load(config, medium, ""))?;
            Ok(with_style(loaded, &table))
        }
        Request::Select { medium, id } => {
            let loaded = load(config, medium, &id)?;
            let table = save(config, setting(medium), &id).map_err(|_| ())?;
            Ok(with_style(loaded, &table))
        }
        Request::Cues {
            state,
            verb,
            mode,
            mood,
            vibe,
            cursor,
        } => {
            let signals = Signals::new(
                state,
                verb.as_deref(),
                mode.as_deref(),
                mood.as_deref(),
                vibe.as_deref(),
            );
            let mut body = json!({"label":signals.label, "keys":signals.ordered_selectors()});
            if let Some(sample) = cursor.filter(|_| state == State::Automation) {
                let pose = cursor::pose(true, true, sample.motion);
                let age = sample
                    .motion
                    .map_or(sample.clock_ms, |motion| motion.age_ms);
                let frame = if sample.reduced_motion {
                    0
                } else {
                    cursor::cursor_frame(pose, age)
                };
                body["cursor"] = json!(cursor::cursor_frame_name(pose, frame));
            }
            Ok(body)
        }
    }
}

fn with_style(mut loaded: Value, table: &toml::Table) -> Value {
    loaded["mood"] = json!(value(table, "mood"));
    loaded["vibe"] = json!(value(table, "vibe"));
    let sleep_after_ms = table
        .get("sleep_after_secs")
        .and_then(toml::Value::as_integer)
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(900)
        .saturating_mul(1000);
    loaded["sleep_after_ms"] = json!(sleep_after_ms);
    loaded
}

fn setting(medium: Medium) -> &'static str {
    match medium {
        Medium::Appearance => "appearance_pack",
        Medium::Voice => "voice_pack",
    }
}

fn value<'a>(table: &'a toml::Table, key: &str) -> Option<&'a str> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .filter(|value| value.len() <= 128)
}

fn settings(config: &Path) -> io::Result<toml::Table> {
    let file = match storage::open(
        &config.join("companion.toml"),
        storage::Open::Read { private: false },
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(toml::Table::new()),
        Err(error) => return Err(error),
    };
    let mut body = String::new();
    file.take(SETTINGS_LIMIT as u64 + 1)
        .read_to_string(&mut body)?;
    if body.len() > SETTINGS_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "companion settings too large",
        ));
    }
    toml::from_str(&body).map_err(io::Error::other)
}

fn save(config: &Path, key: &str, id: &str) -> io::Result<toml::Table> {
    fs::create_dir_all(config)?;
    let lock = storage::open(&config.join("companion-packs.lock"), storage::Open::Lock)?;
    storage::lock(&lock)?;
    let _held = storage::UnlockOnDrop(&lock);
    let mut table = settings(config)?;
    if id.is_empty() {
        table.remove(key);
    } else {
        table.insert(key.to_owned(), toml::Value::String(id.to_owned()));
    }
    let body = toml::to_string(&table).map_err(io::Error::other)?;
    if body.len() > SETTINGS_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "companion settings too large",
        ));
    }
    let temporary = config.join(format!(".companion-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = storage::open(&temporary, storage::Open::New)?;
    let result = file
        .write_all(body.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| storage::replace(&temporary, &config.join("companion.toml")));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result.map(|()| table)
}

fn load(config: &Path, medium: Medium, id: &str) -> Result<Value, ()> {
    if id.is_empty() {
        return Ok(json!({"id":id}));
    }
    let pack = Pack::load(config, id).map_err(|_| ())?;
    Ok(match medium {
        Medium::Appearance => appearance(&pack, id),
        Medium::Voice => voice(&pack, id),
    })
}

fn appearance(pack: &Pack, id: &str) -> Value {
    let mut frames = BTreeMap::new();
    let mut clips = Vec::new();
    for (key, entry) in &pack.manifest.sprites {
        if !packs::valid_key(key) {
            continue;
        }
        let Some(spec) = entry
            .as_ref()
            .and_then(|value| ClipSpec::parse(value).ok().flatten())
        else {
            continue;
        };
        let usable = spec.frames.iter().all(|name| {
            if !frames.contains_key(name) && frames.len() >= packs::MAX_DISTINCT_SPRITES {
                return false;
            }
            frames
                .entry(name.clone())
                .or_insert_with(|| {
                    pack.sprite(name)
                        .ok()
                        .and_then(|image| png(image, (128, 166)))
                })
                .is_some()
        });
        if usable {
            clips.push(json!({"key":key,"frames":spec.frames,"frame_ms":spec.frame_ms(),"loop":spec.looped}));
        }
    }
    let cursor: BTreeMap<_, _> = CURSORS
        .iter()
        .filter_map(|name| {
            let asset = pack.manifest.cursor.get(*name)?.as_ref()?.as_str()?;
            png(pack.cursor(asset).ok()?, packs::CURSOR_SIZE).map(|image| (*name, image))
        })
        .collect();
    frames.retain(|_, image| image.is_some());
    json!({ "id":id, "clips":clips, "frames":frames, "cursor":cursor })
}

fn voice(pack: &Pack, id: &str) -> Value {
    let mut files = BTreeMap::new();
    let mut cues = Vec::new();
    for (key, entry) in &pack.manifest.voice {
        let Some(name) = entry.as_ref().and_then(Value::as_str) else {
            continue;
        };
        if !packs::valid_key(key)
            || (!files.contains_key(name) && files.len() >= packs::MAX_DISTINCT_VOICES)
        {
            continue;
        }
        if files
            .entry(name)
            .or_insert_with(|| pack.voice(name).map(wav))
            .is_some()
        {
            cues.push(json!([key, name]));
        }
    }
    files.retain(|_, data| data.is_some());
    json!({ "id":id, "cues":cues, "audio":files })
}

fn png(image: Png, size: (u32, u32)) -> Option<String> {
    let (width, height) = size;
    let mut scaled = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        let row = y * image.height / height * image.width;
        for x in 0..width {
            let offset = ((row + x * image.width / width) * 4) as usize;
            scaled.extend_from_slice(&image.rgba[offset..offset + 4]);
        }
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .ok()?
            .write_image_data(&scaled)
            .ok()?;
    }
    Some(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn wav(wave: PcmWave) -> String {
    let size = wave.pcm.len() as u32;
    let mut bytes = Vec::with_capacity(44 + wave.pcm.len());
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + size).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0");
    bytes.extend_from_slice(&wave.rate.to_le_bytes());
    bytes.extend_from_slice(&(wave.rate * 2).to_le_bytes());
    bytes.extend_from_slice(b"\x02\0\x10\0data");
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(&wave.pcm);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(path: &Path, bytes: &[u8]) {
        storage::open(path, storage::Open::New)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    #[test]
    fn native_pack_selection_keeps_independent_settings_and_rejects_invalid_ids() {
        let config = std::env::temp_dir().join(format!("native-pack-{}", uuid::Uuid::new_v4()));
        let dir = config.join("companion-packs/rabbit");
        fs::create_dir_all(&dir).unwrap();
        write_fixture(
            &config.join("companion.toml"),
            b"mood = 'calm'\nvoice_pack = 'separate'\n",
        );
        write_fixture(&dir.join("pack.json"), br#"{"format":1,"name":"Rabbit","sprites":{"default":{"frames":["rest.png"]},"state:idle":{"frames":["missing.png"]},"mood:caf\u00e9":{"frames":["rest.png"]},"mood:cafe\u0301":{"frames":["rest.png"]}},"voice":{"state:paused":"line.wav"}}"#);
        write_fixture(
            &dir.join("rest.png"),
            include_bytes!("../companion/assets/states/sleeping_0_400x520.png"),
        );
        let wave = wav(PcmWave {
            rate: 8_000,
            pcm: vec![0; 80],
        });
        write_fixture(
            &dir.join("line.wav"),
            &base64::engine::general_purpose::STANDARD
                .decode(&wave)
                .unwrap(),
        );

        let select = |id| {
            request(
                &config,
                json!({"action":"pack-select","medium":"appearance","id":id}),
            )
        };
        assert_eq!(select("../rabbit")["status"], 409);
        let loaded = select("rabbit");
        assert_eq!(loaded["status"], 200);
        assert_eq!(loaded["body"]["mood"], "calm");
        assert!(loaded["body"]["frames"]["rest.png"].is_string());
        let clips = loaded["body"]["clips"].as_array().unwrap();
        assert_eq!(clips.len(), 3);
        assert!(clips.iter().any(|entry| entry["key"] == "mood:café"));
        assert!(clips
            .iter()
            .any(|entry| entry["key"] == "mood:cafe\u{0301}"));
        assert!(clips.iter().all(|entry| entry["key"] != "state:idle"));
        assert!(serde_json::to_vec(&loaded).unwrap().len() < MAX_RESPONSE_BYTES);
        let prefs = settings(&config).unwrap();
        assert_eq!(value(&prefs, "appearance_pack"), Some("rabbit"));
        assert_eq!(value(&prefs, "voice_pack"), Some("separate"));
        assert_eq!(value(&prefs, "mood"), Some("calm"));
        let voice = |id| {
            request(
                &config,
                json!({"action":"pack-select","medium":"voice","id":id}),
            )
        };
        let selected_voice = voice("rabbit");
        assert_eq!(selected_voice["body"]["audio"]["line.wav"], wave);
        assert_eq!(
            selected_voice["body"]["cues"],
            json!([["state:paused", "line.wav"]])
        );
        assert_eq!(voice("")["status"], 200);
        assert_eq!(value(&settings(&config).unwrap(), "voice_pack"), None);
        assert_eq!(
            value(&settings(&config).unwrap(), "appearance_pack"),
            Some("rabbit")
        );
        assert_eq!(select("")["status"], 200);
        assert_eq!(value(&settings(&config).unwrap(), "appearance_pack"), None);
        fs::remove_dir_all(&config).unwrap();
    }

    #[test]
    fn native_cues_use_the_shared_priority_and_hide_stale_activity_on_pause() {
        let config = Path::new("");
        let cues = |state| {
            request(
                config,
                json!({
                    "action":"pack-cues", "state":state, "mode":"mcp:editor", "verb":"type",
                    "mood":"calm", "vibe":"quiet",
                }),
            )
        };
        assert_eq!(
            cues("automation")["body"]["keys"],
            json!([
                "mode:mcp:editor",
                "mode:mcp:*",
                "verb:type",
                "state:automation",
                "mood:calm",
                "vibe:quiet",
                "default",
            ])
        );
        assert_eq!(
            cues("paused")["body"],
            json!({
                "label":"Paused","keys":["state:paused","default"],
            })
        );
    }

    #[test]
    fn native_cursor_uses_the_shared_click_frames_and_reduced_motion() {
        let cues = |state, reduced| {
            request(
                Path::new(""),
                json!({
                    "action":"pack-cues", "state":state,
                    "cursor":{
                        "clock_ms":1000, "reduced_motion":reduced,
                        "motion":{"kind":"click","age_ms":150,"distance_px":0,"elapsed_ms":0,"clickable":false},
                    },
                }),
            )
        };
        assert_eq!(cues("automation", false)["body"]["cursor"], "click2");
        assert_eq!(cues("automation", true)["body"]["cursor"], "click1");
        assert!(cues("paused", false)["body"]["cursor"].is_null());
    }
}
