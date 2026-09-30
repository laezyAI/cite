use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::{info, warn};

use crate::core::CiteError;

const REPO: &str = "laezyAI/cite";
const BIN_NAME: &str = "cite";

/// A parsed `major.minor.patch[-pre]` version with semver precedence.
///
/// Field order drives the derived ordering: a release (`is_release = true`) sorts after
/// any prerelease of the same core version, and prerelease identifiers compare
/// numerically when numeric and lexically otherwise (numeric < alphanumeric).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    core: (u64, u64, u64),
    is_release: bool,
    pre: Vec<PreId>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PreId {
    Numeric(u64),
    Alpha(String),
}

impl Version {
    fn parse(v: &str) -> Option<Self> {
        let v = v.split_once('+').map_or(v, |(v, _build)| v);
        let (core, pre) = match v.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (v, None),
        };
        let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
        let core = (parts.next()??, parts.next()??, parts.next()??);
        if parts.next().is_some() {
            return None;
        }
        let pre: Vec<PreId> = pre
            .map(|pre| {
                pre.split('.')
                    .map(|id| match id.parse() {
                        Ok(n) => PreId::Numeric(n),
                        Err(_) => PreId::Alpha(id.to_string()),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let is_release = pre.is_empty();
        Some(Self {
            core,
            is_release,
            pre,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (major, minor, patch) = self.core;
        write!(f, "{major}.{minor}.{patch}")?;
        for (i, id) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match id {
                PreId::Numeric(n) => write!(f, "{n}")?,
                PreId::Alpha(s) => f.write_str(s)?,
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
}

/// Self-update by running the cargo-dist installer published with the newest release.
pub async fn upgrade() -> Result<String, CiteError> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .ok_or_else(|| CiteError::Config("Invalid package version".into()))?;

    info!("Checking for updates");
    let client = reqwest::Client::builder()
        .user_agent(concat!("cite/", env!("CARGO_PKG_VERSION")))
        .build()?;

    // `/releases/latest` skips prereleases, so pick the highest version from the list instead.
    let releases: Vec<Release> = client
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases?per_page=30"
        ))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let Some((latest, tag)) = latest_release(&releases) else {
        return Err(CiteError::Config("No published releases found".into()));
    };

    if latest <= current {
        return Ok(format!("Already up to date (v{current})"));
    }
    warn!("New version available: v{latest} (current: v{current})");

    let (script_name, interpreter, args) = installer_command();
    let script_url = format!("https://github.com/{REPO}/releases/download/{tag}/{script_name}");
    info!("Downloading {script_url}");
    let script = client
        .get(&script_url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    let script_path = std::env::temp_dir().join(format!("{BIN_NAME}-{latest}-{script_name}"));
    std::fs::write(&script_path, &script)?;

    let installer = script_path.clone();
    let result = tokio::task::spawn_blocking(move || run_installer(&installer, interpreter, args))
        .await
        .map_err(|e| CiteError::Config(format!("Installer task failed: {e}")))?;
    let _ = std::fs::remove_file(&script_path);
    result?;

    Ok(format!("Updated to v{latest}"))
}

fn latest_release(releases: &[Release]) -> Option<(Version, &str)> {
    releases
        .iter()
        .filter(|r| !r.draft)
        .filter_map(|r| {
            let version = Version::parse(r.tag_name.strip_prefix('v').unwrap_or(&r.tag_name));
            version.map(|v| (v, r.tag_name.as_str()))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
}

fn installer_command() -> (&'static str, &'static str, &'static [&'static str]) {
    if cfg!(windows) {
        (
            "cite-installer.ps1",
            "powershell",
            &["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"],
        )
    } else {
        ("cite-installer.sh", "sh", &[])
    }
}

fn run_installer(script: &Path, interpreter: &str, args: &[&str]) -> Result<(), CiteError> {
    // Windows cannot overwrite a running executable, but it can rename it out of the way.
    let parked = park_running_exe()?;

    let status = std::process::Command::new(interpreter)
        .args(args)
        .arg(script)
        .status();

    let failed = !matches!(&status, Ok(s) if s.success());
    if let Some((original, parked)) = &parked
        && failed
    {
        let _ = std::fs::rename(parked, original);
    }

    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(CiteError::Config(format!("Installer exited with {s}"))),
        Err(e) => Err(CiteError::Config(format!(
            "Failed to run installer with '{interpreter}': {e}"
        ))),
    }
}

#[cfg(windows)]
fn park_running_exe() -> Result<Option<(PathBuf, PathBuf)>, CiteError> {
    let exe = std::env::current_exe()?;
    let parked = exe.with_extension("exe.old");
    let _ = std::fs::remove_file(&parked);
    std::fs::rename(&exe, &parked)?;
    Ok(Some((exe, parked)))
}

#[cfg(not(windows))]
fn park_running_exe() -> Result<Option<(PathBuf, PathBuf)>, CiteError> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, draft: bool) -> Release {
        Release {
            tag_name: tag.to_string(),
            draft,
        }
    }

    #[test]
    fn test_latest_release_orders_prereleases() {
        let releases = [
            release("v0.1.0-alpha.4", false),
            release("v0.1.0-alpha.10", false),
            release("v0.1.0-alpha.5", false),
        ];
        let (version, tag) = latest_release(&releases).unwrap();
        assert_eq!(version, Version::parse("0.1.0-alpha.10").unwrap());
        assert_eq!(tag, "v0.1.0-alpha.10");
    }

    #[test]
    fn test_latest_release_prefers_stable_and_skips_drafts() {
        let releases = [
            release("v0.2.0", true),
            release("v0.1.0", false),
            release("v0.1.0-alpha.9", false),
            release("not-a-version", false),
        ];
        let (version, _) = latest_release(&releases).unwrap();
        assert_eq!(version, Version::parse("0.1.0").unwrap());
    }

    #[test]
    fn test_version_precedence_follows_semver() {
        let ordered = [
            "0.9.9",
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
        ];
        for pair in ordered.windows(2) {
            let (a, b) = (
                Version::parse(pair[0]).unwrap(),
                Version::parse(pair[1]).unwrap(),
            );
            assert!(a < b, "{} < {}", pair[0], pair[1]);
        }
    }

    #[test]
    fn test_version_parse_and_display() {
        assert_eq!(
            Version::parse("0.1.0-alpha.5").unwrap().to_string(),
            "0.1.0-alpha.5"
        );
        assert_eq!(
            Version::parse("1.2.3+build.7").unwrap().to_string(),
            "1.2.3"
        );
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
        assert!(Version::parse("v1.2.3").is_none());
        assert!(Version::parse(env!("CARGO_PKG_VERSION")).is_some());
    }

    #[test]
    fn test_latest_release_empty() {
        assert!(latest_release(&[]).is_none());
    }

    #[test]
    fn test_installer_script_matches_dist_artifact() {
        let (script, _, _) = installer_command();
        assert!(script.starts_with("cite-installer."));
    }
}
