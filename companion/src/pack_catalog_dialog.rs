use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use hypermesh_visor::companion_catalog::{self as catalog, Catalog, CatalogSource, Preview};
use hypermesh_visor::companion_packs::{installed, Medium};

use crate::mcp_dialog::{args, choose};

pub fn browse(
    config: &Path,
    ui: &mut impl FnMut(&[String], Option<&str>) -> Result<Option<String>, String>,
) -> Result<(), String> {
    browse_from(
        config,
        ui,
        &CatalogSource::GITHUB,
        true,
        |medium, bytes| match medium {
            Preview::Art => artwork(config, bytes),
            Preview::Voice => voice(bytes),
        },
    )
}

pub(crate) fn browse_from(
    config: &Path,
    ui: &mut impl FnMut(&[String], Option<&str>) -> Result<Option<String>, String>,
    source: &CatalogSource<'_>,
    show_progress: bool,
    mut present: impl FnMut(Preview, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let catalog = progress(show_progress, "Loading community packs…", || {
        Catalog::fetch_from(config, source)
    })?;
    let rows: Vec<_> = catalog
        .packs
        .iter()
        .map(|entry| {
            format!(
                "{} — {} ({})",
                display(&entry.name),
                display(&entry.author),
                display(&entry.license)
            )
        })
        .collect();
    if rows.is_empty() {
        ui(
            &args("info", "Community packs", "No community packs are listed."),
            None,
        )?;
        return Ok(());
    }
    let Some(index) = choose(
        ui,
        "Community packs",
        "Choose a pack to preview or download",
        &rows,
    )?
    else {
        return Ok(());
    };
    let entry = &catalog.packs[index];
    let mut operations = Vec::new();
    let mut actions = Vec::new();
    for (medium, title, available) in [
        (Preview::Art, "Preview artwork", entry.art_preview.is_some()),
        (
            Preview::Voice,
            "Play voice preview",
            entry.voice_preview.is_some(),
        ),
    ] {
        if available {
            operations.push(Some(medium));
            actions.push(title.to_owned());
        }
    }
    operations.push(None);
    actions.push(
        if entry.installed {
            "Replace local copy"
        } else {
            "Download and install"
        }
        .into(),
    );
    while let Some(choice) = choose(ui, &display(&entry.name), &rows[index], &actions)? {
        match operations[choice] {
            Some(medium) => {
                let bytes = progress(show_progress, "Downloading preview…", || {
                    entry.preview_from(&catalog.asset_revision, medium, source)
                })?;
                present(medium, &bytes)?;
            }
            None => {
                if entry.installed
                    && ui(
                        &args(
                            "question",
                            "Replace local pack?",
                            "The installed copy and any edits to it will be replaced.",
                        ),
                        None,
                    )?
                    .is_none()
                {
                    continue;
                }
                progress(show_progress, "Downloading and verifying pack…", || {
                    entry.install_from(config, &catalog.asset_revision, entry.installed, source)
                })?;
                ui(
                    &args(
                        "info",
                        "Pack installed",
                        "Choose it in Appearance or Voice to start using it.",
                    ),
                    None,
                )?;
                return Ok(());
            }
        }
    }
    Ok(())
}

pub fn remove(
    config: &Path,
    ui: &mut impl FnMut(&[String], Option<&str>) -> Result<Option<String>, String>,
) -> Result<Option<String>, String> {
    let mut choices: BTreeMap<_, _> = installed(config, Medium::Appearance)?.into_iter().collect();
    choices.extend(installed(config, Medium::Voice)?);
    let items: Vec<_> = choices.into_iter().collect();
    if items.is_empty() {
        ui(
            &args("info", "Remove installed pack", "No packs are installed."),
            None,
        )?;
        return Ok(None);
    }
    let rows: Vec<_> = items
        .iter()
        .map(|(id, name)| format!("{} [{id}]", display(name)))
        .collect();
    let Some(choice) = choose(
        ui,
        "Remove installed pack",
        "Choose the local pack to remove",
        &rows,
    )?
    else {
        return Ok(None);
    };
    if ui(
        &args(
            "question",
            "Remove local pack?",
            &format!("Remove {}?", rows[choice]),
        ),
        None,
    )?
    .is_none()
    {
        return Ok(None);
    }
    let id = &items[choice].0;
    catalog::remove(config, id)?;
    Ok(Some(id.clone()))
}

fn display(value: &str) -> String {
    value.chars().filter(|c| !c.is_control() && !matches!(c, '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')).collect()
}

fn progress<T>(
    enabled: bool,
    message: &str,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    if !enabled {
        return operation();
    }
    let mut child = Command::new("zenity")
        .args([
            "--progress",
            "--pulsate",
            "--auto-close",
            "--no-cancel",
            "--no-markup",
            "--title=Community packs",
        ])
        .arg(format!("--text={message}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .ok();
    let result = operation();
    if let Some(child) = &mut child {
        if let Some(mut input) = child.stdin.take() {
            let _ = input.write_all(b"100\n");
        }
        let _ = child.wait();
    }
    result
}

fn artwork(config: &Path, bytes: &[u8]) -> Result<(), String> {
    fs::create_dir_all(config).map_err(|_| "The artwork preview could not be saved.")?;
    let path = config.join("companion-preview.png");
    let next = config.join(format!(".companion-preview-{}", uuid::Uuid::new_v4()));
    let mut file = hypermesh_local_storage::open(&next, hypermesh_local_storage::Open::New)
        .map_err(|_| "The artwork preview could not be saved.")?;
    let written = file.write_all(bytes);
    drop(file);
    if written.and_then(|()| fs::rename(&next, &path)).is_err() {
        let _ = fs::remove_file(next);
        return Err("The artwork preview could not be saved.".into());
    }
    if !Command::new("xdg-open")
        .arg(path)
        .status()
        .is_ok_and(|status| status.success())
    {
        return Err("Could not open the artwork preview.".into());
    }
    Ok(())
}

fn voice(bytes: &[u8]) -> Result<(), String> {
    let mut child = Command::new("/usr/bin/aplay")
        .arg("--quiet")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|_| {
            "Could not play the voice preview. Install aplay for Linux preview playback."
        })?;
    let written = child
        .stdin
        .take()
        .ok_or("Could not open voice playback.")?
        .write_all(bytes);
    let status = child
        .wait()
        .map_err(|_| "The voice preview did not finish.")?;
    if written.is_err() || !status.success() {
        return Err("The voice preview could not be played.".into());
    }
    Ok(())
}
