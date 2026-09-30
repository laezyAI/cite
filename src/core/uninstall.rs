use std::io::Write;

use tracing::{info, instrument, warn};

use crate::core::CiteError;

#[instrument]
pub fn uninstall() -> Result<(), CiteError> {
    let current_exe = std::env::current_exe()
        .map_err(|e| CiteError::Config(format!("Cannot determine executable path: {e}")))?;

    let install_dir = current_exe
        .parent()
        .ok_or_else(|| CiteError::Config("Cannot determine install directory".into()))?;

    info!("cite installed at: {}", current_exe.display());

    warn!("This will delete the binary. Shell config files might NOT be modified");
    print!("Are you sure? [y/N] ");
    let _ = std::io::stdout().flush();

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    match input.trim().to_lowercase().as_str() {
        "y" | "yes" => {}
        _ => {
            warn!("Uninstall cancelled");
            return Ok(());
        }
    }

    std::fs::remove_file(&current_exe)?;
    info!("Removed {}", current_exe.display());

    if install_dir
        .read_dir()
        .map(|mut d| d.next().is_none())
        .unwrap_or(false)
    {
        let _ = std::fs::remove_dir(install_dir);
        info!("Removed empty directory {}", install_dir.display());
    }

    // Remove local database, session, and credentials
    let cite_dir = crate::core::cite_home();
    if cite_dir.exists() {
        let _ = std::fs::remove_file(cite_dir.join("cite.db"));
        let _ = std::fs::remove_file(cite_dir.join("session.json"));
        let _ = std::fs::remove_file(cite_dir.join("credentials.toml"));
        info!("Removed ~/.cite/cite.db, session.json, and credentials.toml");
        if std::fs::read_dir(&cite_dir)
            .map(|mut d| d.next().is_none())
            .unwrap_or(true)
        {
            let _ = std::fs::remove_dir(&cite_dir);
            info!("Removed empty ~/.cite directory");
        }
    }

    let install_dir_str = install_dir.to_string_lossy();
    let found = crate::core::home_dir().is_some_and(|home| {
        [".zshrc", ".bashrc", ".bash_profile", ".profile"]
            .iter()
            .any(|rc| {
                std::fs::read_to_string(home.join(rc))
                    .is_ok_and(|c| c.contains(install_dir_str.as_ref()))
            })
    });

    if found {
        info!("Shell config files reference the install directory");
        info!("  Edit ~/.zshrc, ~/.bashrc, etc. and remove lines containing:");
        info!("    {install_dir_str}");
        info!("  Then restart your shell or run: source ~/.zshrc");
    }
    info!("cite has been uninstalled");
    Ok(())
}
