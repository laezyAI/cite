pub mod cache;
pub mod compiler;
pub mod credentials;
pub mod db;
pub mod deploy;
pub mod doctor;
pub mod manifest;
pub mod media;
pub mod metadata;
pub mod project;
pub mod scaffold;
pub mod uninstall;
pub mod upgrade;

pub mod report;
pub use report::CiteError;

use std::path::{Path, PathBuf};

/// The user's home directory (`HOME`, falling back to `USERPROFILE` on Windows).
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Global state directory: `~/.cite`.
pub fn cite_home() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cite")
}

/// Write a file readable only by the current user (credentials, sessions).
pub fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // `mode` only applies on creation; tighten files written by older versions too.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(contents)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}
