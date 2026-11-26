//! Static alternatives catalog and explicit, on-machine installation.
use serde_json::Value;
use std::io::Write;

#[cfg(target_os = "linux")]
mod linux;

const PACKS: &str = include_str!("packs.json");
const CATALOG: &str = include_str!("catalog.json");
const USAGE: &str = "Usage: hypermesh-visor catalog list | show ID | packs [PROFILE] | plan ID | install ID MACHINE PLAN_SHA256 | health ID | remove ID MACHINE INSTALLATION";

pub fn run(args: impl IntoIterator<Item = String>, mut output: impl Write) -> Result<(), String> {
    let args: Vec<_> = args.into_iter().collect();
    if matches!(args.as_slice(), [argument] if matches!(argument.as_str(), "-h" | "--help")) {
        return writeln!(output, "{USAGE}").map_err(|_| "Could not write help".into());
    }
    let catalog: Value = serde_json::from_str(CATALOG).map_err(|_| "Invalid bundled catalog")?;
    let tools = catalog["tools"]
        .as_array()
        .ok_or("Invalid bundled catalog")?;
    let entry = |id: &str| {
        tools
            .iter()
            .find(|entry| entry["id"] == id)
            .ok_or("Unknown catalog tool")
    };
    let value = match args.as_slice() {
        [command] if command == "list" => catalog,
        [command] if command == "packs" => pack_catalog(&catalog, None)?,
        [command, profile] if command == "packs" => pack_catalog(&catalog, Some(profile))?,
        [command, id] if command == "show" => entry(id)?.clone(),
        [command, id, rest @ ..]
            if matches!(command.as_str(), "plan" | "install" | "health" | "remove") =>
        {
            let tool = entry(id)?;
            if tool["availability"] != "visor_installable" {
                return Err(format!(
                    "Use the upstream install instructions at {}",
                    tool["install_url"].as_str().unwrap_or_default()
                ));
            }
            manage(command, id, rest)?
        }
        _ => return Err(USAGE.into()),
    };
    serde_json::to_writer_pretty(&mut output, &value)
        .map_err(|_| "Could not write catalog response")?;
    writeln!(output).map_err(|_| "Could not finish catalog response".into())
}

fn pack_catalog(catalog: &Value, profile: Option<&String>) -> Result<Value, &'static str> {
    let mut packs: Value = serde_json::from_str(PACKS).map_err(|_| "Invalid bundled tool packs")?;
    let profiles = packs["profiles"]
        .as_array_mut()
        .ok_or("Invalid bundled tool packs")?;
    for entry in profiles.iter_mut() {
        for tool in entry["tools"].as_array_mut().ok_or("Invalid tool pack")? {
            if tool["route"] == "visor_catalog" {
                let listed = catalog["tools"]
                    .as_array()
                    .and_then(|items| items.iter().find(|item| item["id"] == tool["id"]))
                    .ok_or("Tool pack refers to an unknown catalog entry")?;
                tool["catalog"] = listed.clone();
            }
        }
    }
    match profile {
        Some(id) => profiles
            .iter()
            .find(|entry| entry["id"] == *id)
            .cloned()
            .ok_or("Unknown tool pack"),
        None => Ok(packs),
    }
}

#[cfg(target_os = "linux")]
fn manage(command: &str, id: &str, arguments: &[String]) -> Result<Value, String> {
    linux::manage(command, id, arguments, &crate::mcp::config_dir()).map_err(str::to_owned)
}

#[cfg(not(target_os = "linux"))]
fn manage(_: &str, _: &str, _: &[String]) -> Result<Value, String> {
    Err("Managed catalog installation requires a supported Linux target".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn optional_packs_resolve_existing_catalog_metadata() {
        let mut buffer = Vec::new();
        run(["packs".into()], &mut buffer).unwrap();
        let packs: Value = serde_json::from_slice(&buffer).unwrap();
        let profiles = packs["profiles"].as_array().unwrap();
        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles[0]["image_target"], "cloud-coding");
        assert_eq!(profiles[1]["tools"][0]["catalog"]["id"], "llama2-c");
        assert_eq!(profiles[2]["tools"][1]["catalog"]["id"], "vikunja");
        assert!(run(["packs".into(), "unknown".into()], &mut Vec::new()).is_err());
    }

    #[test]
    fn catalog_has_all_alternatives_and_only_one_managed_install_route() {
        let mut buffer = Vec::new();
        run(["list".into()], &mut buffer).unwrap();
        let catalog: Value = serde_json::from_slice(&buffer).unwrap();
        let tools = catalog["tools"].as_array().unwrap();
        let ids: HashSet<_> = tools
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), tools.len());
        for id in [
            "gitea",
            "forgejo",
            "vikunja",
            "zulip",
            "nextcloud",
            "libreoffice",
            "ollama",
            "llama-cpp",
            "aider",
            "librechat",
            "llama2-c",
        ] {
            assert!(ids.contains(id), "{id}");
        }
        for tool in tools {
            assert!(tool["install_url"]
                .as_str()
                .unwrap()
                .starts_with("https://"));
            assert!(tool["release"].is_string());
            assert!(tool["software_license"].is_string());
            assert!(tool["platforms"]
                .as_array()
                .is_some_and(|platforms| !platforms.is_empty()));
            if tool["group"] == "local_ai" {
                assert_eq!(tool["model_weights"], "separate_license");
            }
            assert_eq!(
                tool["availability"] == "visor_installable",
                tool["id"] == "llama2-c"
            );
        }
        assert!(run(["plan".into(), "forgejo".into()], &mut Vec::new())
            .unwrap_err()
            .contains("forgejo.org"));
    }
}
