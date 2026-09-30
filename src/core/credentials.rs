use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::core::CiteError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupabaseCredentials {
    pub url: String,
    pub api_key: String,
}

fn creds_path() -> PathBuf {
    std::env::var_os("CITE_CREDS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::core::cite_home().join("credentials.toml"))
}

pub fn load_credentials() -> Result<SupabaseCredentials, CiteError> {
    if let (Ok(url), Ok(key)) = (
        std::env::var("CITE_SUPABASE_URL"),
        std::env::var("CITE_SUPABASE_API_KEY"),
    ) {
        info!("Using credentials from environment variables");
        return Ok(SupabaseCredentials { url, api_key: key });
    }

    let path = creds_path();
    if !path.exists() {
        return Err(CiteError::Config(
            "No credentials found. Run 'cite login' or set CITE_SUPABASE_URL and CITE_SUPABASE_API_KEY environment variables."
                .to_string(),
        ));
    }

    let s = std::fs::read_to_string(&path)?;
    let table: toml::Value = toml::from_str(&s)?;
    let supabase = table.get("supabase").ok_or_else(|| {
        CiteError::Config("Missing [supabase] section in credentials.toml".into())
    })?;
    let url = supabase
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CiteError::Config("Missing url in [supabase]".into()))?
        .to_string();
    let api_key = supabase
        .get("api_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CiteError::Config("Missing api_key in [supabase]".into()))?
        .to_string();

    info!("Loaded credentials from {}", path.display());
    Ok(SupabaseCredentials { url, api_key })
}

pub fn save_credentials(creds: &SupabaseCredentials) -> Result<(), CiteError> {
    #[derive(Serialize)]
    struct CredentialsFile<'a> {
        supabase: &'a SupabaseCredentials,
    }
    let content = toml::to_string(&CredentialsFile { supabase: creds })?;
    crate::core::write_private(&creds_path(), content.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::{Mutex, MutexGuard};

    /// Tests in this module mutate process-wide env vars; run them one at a time.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct EnvGuard {
        key: String,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &str, value: &str) -> Self {
            let previous = env::var(key).ok();
            unsafe {
                env::set_var(key, value);
            }
            Self {
                key: key.to_string(),
                previous,
            }
        }

        fn remove(key: &str) -> Self {
            let previous = env::var(key).ok();
            unsafe {
                env::remove_var(key);
            }
            Self {
                key: key.to_string(),
                previous,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(val) => unsafe {
                    env::set_var(&self.key, val);
                },
                None => unsafe {
                    env::remove_var(&self.key);
                },
            }
        }
    }

    #[test]
    fn test_creds_path_default() {
        let _lock = env_lock();
        let p = creds_path();
        assert!(p.to_string_lossy().contains(".cite/credentials.toml"));
    }

    #[test]
    fn test_credentials_from_env() {
        let _lock = env_lock();
        let _guard_url = EnvGuard::set("CITE_SUPABASE_URL", "https://env-test.supabase.co");
        let _guard_key = EnvGuard::set("CITE_SUPABASE_API_KEY", "env-key-456");

        let result = load_credentials();
        assert!(result.is_ok());
        let creds = result.unwrap();
        assert_eq!(creds.url, "https://env-test.supabase.co");
        assert_eq!(creds.api_key, "env-key-456");
    }

    #[test]
    fn test_save_credentials_round_trip_escapes_values() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let creds_file = dir.path().join("nested").join("credentials.toml");
        let _guard = EnvGuard::set("CITE_CREDS_PATH", creds_file.to_str().unwrap());
        let _url_guard = EnvGuard::remove("CITE_SUPABASE_URL");
        let _key_guard = EnvGuard::remove("CITE_SUPABASE_API_KEY");

        let creds = SupabaseCredentials {
            url: "https://x.supabase.co".into(),
            api_key: r#"key-with-"quote"-and\backslash"#.into(),
        };
        save_credentials(&creds).unwrap();
        let loaded = load_credentials().unwrap();
        assert_eq!(loaded.url, creds.url);
        assert_eq!(loaded.api_key, creds.api_key);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&creds_file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn test_credentials_file_invalid() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let creds_file = dir.path().join("credentials.toml");
        std::fs::write(&creds_file, "not-toml").unwrap();

        let _guard = EnvGuard::set("CITE_CREDS_PATH", creds_file.to_str().unwrap());
        let result = load_credentials();
        assert!(result.is_err());
    }

    #[test]
    fn test_credentials_missing_supabase_section() {
        let _lock = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let creds_file = dir.path().join("credentials.toml");
        std::fs::write(&creds_file, "[other]\nkey = \"val\"").unwrap();

        let _guard = EnvGuard::set("CITE_CREDS_PATH", creds_file.to_str().unwrap());
        let _url_guard = EnvGuard::remove("CITE_SUPABASE_URL");
        let _key_guard = EnvGuard::remove("CITE_SUPABASE_API_KEY");
        let result = load_credentials();
        assert!(result.is_err());
        match result {
            Err(CiteError::Config(msg)) => assert!(msg.contains("[supabase]"), "{msg}"),
            _ => panic!("expected Config error"),
        }
    }
}
