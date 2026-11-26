//! The local workspace linter uses the same owner-admitted read as any other plugin.
use super::*;
use crate::permission_templates::{self, Operation};
use sha2::{Digest, Sha256};

const ID: &str = "sh.hyperme.json-lint";
const TOOL: &str = "lint-json";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LintRequest {
    workspace: String,
    file: String,
}

fn target(workspace: &str, file: &str) -> Result<String, ApiError> {
    if workspace == "/"
        || !workspace.starts_with('/')
        || workspace[1..].split('/').any(invalid_component)
        || file.split('/').any(invalid_component)
        || !file.ends_with(".json")
        || workspace.len() + 1 + file.len() > 4096
    {
        return Err(ApiError::bad(
            "Choose a relative .json file in the assigned workspace",
        ));
    }
    Ok(format!("{workspace}/{file}"))
}

fn invalid_component(value: &str) -> bool {
    matches!(value, "" | "." | "..")
        || value.contains(['*', '?', '\\'])
        || value.chars().any(char::is_control)
}

pub(super) async fn run(
    State(state): State<Arc<AppState>>,
    Extension(link): Extension<CompanionLink>,
    Json(body): Json<LintRequest>,
) -> Result<Response, ApiError> {
    private(link)?;
    let target = target(&body.workspace, &body.file)?;
    let (session, context) =
        super::super::terminal_tasks::assigned_context(&state, &body.workspace).await?;
    let grant = lock(&state.plugins).get(session, ID).ok_or_else(|| {
        ApiError::bad("Enable the JSON lint plugin for the selected session and file")
    })?;
    if !matches!(&grant.target, plugin::Target::Workspace(path) if *path == target) {
        return Err(ApiError::forbidden(
            "The JSON lint plugin grant does not cover this file",
        ));
    }
    let metadata = grant.package.metadata();
    if metadata["profile"] != "local-stdio"
        || metadata["api_version"] != 1
        || metadata["tool"]["name"] != TOOL
    {
        return Err(ApiError::bad(
            "The enabled JSON lint package does not support the expected tool and API",
        ));
    }
    let prefs = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        move || permission_templates::load(&local_reads::directory(&state)?)
    })
    .await
    .map_err(|_| ApiError::unavailable("Workspace permissions unavailable"))?
    .map_err(|_| ApiError::unavailable("Workspace permissions unavailable"))?;
    let identity = &context["identity"];
    let organization = serde_json::from_value::<Uuid>(identity["organization_id"].clone()).ok();
    let device = serde_json::from_value::<Uuid>(identity["device_id"].clone()).ok();
    let refused = || ApiError::forbidden("Allow this exact file in Visor Workspace permissions");
    let selection = prefs
        .selections
        .into_iter()
        .find(|selection| {
            selection.scope.project == body.workspace
                && Some(selection.scope.organization) == organization
                && Some(selection.scope.device) == device
        })
        .ok_or_else(refused)?;
    let rule = selection
        .rules
        .iter()
        .find(|rule| rule.operation == Operation::Read && rule.target == target)
        .ok_or_else(refused)?;
    let request = reads::ReadRequest {
        request_id: Uuid::new_v4(),
        session_id: session,
        target,
        max_bytes: rule.max_bytes.min(plugin::MAX_READ_BYTES),
        timeout_seconds: rule.timeout_seconds.min(10),
        selection,
    };
    call_workspace(state, session, grant, request, Some(body.file))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Findings {
    diagnostics: Vec<Finding>,
    truncated: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Finding {
    line: usize,
    column: usize,
    rule: String,
}

pub(super) fn report(
    file: String,
    bytes: &[u8],
    metadata: &Value,
    result: Value,
) -> Result<Value, plugin::Error> {
    let invalid = plugin::Error::InvalidOutput;
    let findings: Findings = serde_json::from_value(result).map_err(|_| invalid)?;
    if findings.diagnostics.len() > 32 || (findings.truncated && findings.diagnostics.len() != 32) {
        return Err(invalid);
    }
    // Match the editor's text model for position and freshness checks.
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let display = text.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<_> = display
        .split('\n')
        .map(|line| line.encode_utf16().count() + 1)
        .collect();
    let mut diagnostics = Vec::new();
    for finding in findings.diagnostics {
        let (severity, message) = match finding.rule.as_str() {
            "duplicate-key" => ("warning", "Duplicate JSON object key"),
            "json-syntax" => ("error", "Invalid JSON syntax"),
            "json-encoding" => ("error", "JSON must be UTF-8"),
            _ => return Err(invalid),
        };
        let Some(&width) = finding
            .line
            .checked_sub(1)
            .and_then(|index| lines.get(index))
        else {
            return Err(invalid);
        };
        if !(1..=width).contains(&finding.column) {
            return Err(invalid);
        }
        diagnostics.push(json!({
            "line":finding.line,"column":finding.column,"rule":finding.rule,
            "severity":severity,"message":message
        }));
    }
    Ok(json!({
        "file":file,
        "content_sha256":format!("{:x}", Sha256::digest(bytes)),
        "display_sha256":format!("{:x}", Sha256::digest(display)),
        "runtime":metadata["profile"],
        "linter_version":metadata["package_version"],
        "config":"json-strict-v1",
        "diagnostics":diagnostics,
        "truncated":findings.truncated
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_must_be_lexically_inside_assigned_workspace() {
        for file in [
            "/tmp/x.json",
            "../x.json",
            "nested/../x.json",
            "./x.json",
            "nested//x.json",
            "x.py",
            "x\\y.json",
            "x\n.json",
        ] {
            assert!(target("/a", file).is_err(), "{file:?}");
        }
        assert!(target("/a/../b", "x.json").is_err());
        assert_eq!(
            target("/a", "nested/x.json").ok().unwrap(),
            "/a/nested/x.json"
        );
    }

    #[test]
    fn report_binds_diagnostics_to_owner_file_and_known_rules() {
        let metadata = json!({"profile":"local-stdio","package_version":"0.1.0"});
        let result = |rule, column| json!({"diagnostics":[{"rule":rule,"line":1,"column":column}],"truncated":false});
        let accepted = report(
            "x.json".into(),
            b"{}",
            &metadata,
            result("duplicate-key", 2),
        )
        .unwrap();
        assert_eq!(accepted["file"], "x.json");
        assert_eq!(
            accepted["diagnostics"][0]["message"],
            "Duplicate JSON object key"
        );
        assert!(report("x.json".into(), b"{}", &metadata, result("untrusted", 2)).is_err());
        assert!(report(
            "x.json".into(),
            b"{}",
            &metadata,
            result("duplicate-key", 5)
        )
        .is_err());
    }
}
