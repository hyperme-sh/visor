//! Identifies the current WSL distribution for the private workspace transport.
use std::sync::OnceLock;

static WSL_DISTRO: OnceLock<Result<Option<String>, &'static str>> = OnceLock::new();

fn identify(
    release: &str,
    wsl_runtime: bool,
    name: Option<&str>,
) -> Result<Option<String>, &'static str> {
    let release = release.to_ascii_lowercase();
    if !release.contains("microsoft") && !release.contains("wsl") || !wsl_runtime && name.is_none()
    {
        return Ok(None);
    }
    let name = name
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 1024
                && value.trim() == *value
                && !value.contains(['/', '\\'])
                && !value.chars().any(char::is_control)
        })
        .ok_or("WSL distribution identity unavailable")?;
    Ok(Some(name.into()))
}

pub fn wsl_distro() -> Result<Option<&'static str>, &'static str> {
    WSL_DISTRO
        .get_or_init(|| {
            if !cfg!(target_os = "linux") {
                return Ok(None);
            }
            let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
            let name = std::env::var("WSL_DISTRO_NAME");
            identify(
                &release,
                std::path::Path::new("/run/WSL").is_dir(),
                name.as_deref().ok(),
            )
        })
        .as_ref()
        .map(Option::as_deref)
        .map_err(|error| *error)
}

pub fn wsl_linux_path(value: &str) -> bool {
    value.starts_with('/')
        && !value.starts_with("//")
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
        && !matches!(value.as_bytes(), [b'/', drive, b':', ..] if drive.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_wsl_distribution_without_mistaking_a_container_for_wsl() {
        let wsl = "6.18.58.1-microsoft-standard-WSL2";
        assert_eq!(
            identify(wsl, true, Some("Ubuntu Work")).unwrap().as_deref(),
            Some("Ubuntu Work")
        );
        assert_eq!(
            identify(wsl, false, Some("Debian")).unwrap().as_deref(),
            Some("Debian")
        );
        assert_eq!(identify(wsl, false, None).unwrap(), None);
        assert!(identify(wsl, true, None).is_err());
        assert!(identify(wsl, true, Some("debian\n")).is_err());
        assert_eq!(identify("6.1.0", false, Some("stale")).unwrap(), None);
    }

    #[test]
    fn accepts_linux_paths_and_refuses_drive_and_unc_paths() {
        assert!(wsl_linux_path("/home/work/code"));
        assert!(wsl_linux_path("/mnt/c/project"));
        for path in [
            "C:/work",
            "C:\\work",
            "/C:/work",
            "//wsl.localhost/Debian/work",
            "\\\\wsl$\\Debian\\work",
        ] {
            assert!(!wsl_linux_path(path), "{path}");
        }
    }
}
