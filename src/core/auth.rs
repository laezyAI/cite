//! Supabase access: stored credentials, login session with auto-refresh, and authenticated connections.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use crate::core::CiteError;
use crate::core::manifest::BackendConfig;
use crate::core::project::ProjectContext;
use crate::core::supabase::{Row, Supabase, row};

const TOKEN_EXPIRY_MARGIN_SECS: i64 = 60;

pub struct Connection {
    pub api: Supabase,
    pub user_id: Option<String>,
}

pub async fn connect(ctx: &ProjectContext) -> Result<Connection, CiteError> {
    let backend = resolve_backend(ctx.manifest.backend.as_ref())?;
    let client = reqwest::Client::new();
    let base_url = base_url(&backend);
    let api_key = backend.api_key.as_deref().unwrap_or_default();
    let session = active_session(&client, base_url, api_key).await?;
    let bearer = resolve_bearer(&backend, session.as_ref())?;
    Ok(Connection {
        api: Supabase::new(client, base_url, api_key, &bearer),
        user_id: session.map(|s| s.user_id),
    })
}

pub fn backend_url(ctx: &ProjectContext) -> Result<String, CiteError> {
    Ok(base_url(&resolve_backend(ctx.manifest.backend.as_ref())?).to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SupabaseCredentials {
    url: String,
    api_key: String,
}

#[derive(Serialize, Deserialize)]
struct CredentialsFile {
    supabase: SupabaseCredentials,
}

fn creds_path() -> PathBuf {
    std::env::var_os("CITE_CREDS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::core::cite_home().join("credentials.toml"))
}

fn load_credentials() -> Result<SupabaseCredentials, CiteError> {
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

    let file: CredentialsFile = toml::from_str(&std::fs::read_to_string(&path)?).map_err(|e| {
        CiteError::Config(format!(
            "{} needs a [supabase] section with url and api_key: {e}",
            path.display()
        ))
    })?;
    info!("Loaded credentials from {}", path.display());
    Ok(file.supabase)
}

fn save_credentials(creds: &SupabaseCredentials) -> Result<(), CiteError> {
    let file = CredentialsFile {
        supabase: creds.clone(),
    };
    crate::core::write_private(&creds_path(), toml::to_string(&file)?.as_bytes())?;
    Ok(())
}

fn resolve_backend(project_backend: Option<&BackendConfig>) -> Result<BackendConfig, CiteError> {
    if let Some(backend) = project_backend {
        return Ok(backend.clone());
    }
    let creds = load_credentials()?;
    Ok(BackendConfig {
        url: Some(creds.url),
        api_key: Some(creds.api_key),
    })
}

fn base_url(backend: &BackendConfig) -> &str {
    backend
        .url
        .as_deref()
        .unwrap_or_default()
        .trim_end_matches('/')
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    user: TokenUser,
}

#[derive(Deserialize)]
struct TokenUser {
    id: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct Session {
    access_token: String,
    refresh_token: String,
    email: String,
    user_id: String,
    expires_at: i64,
    url: String,
}

impl Session {
    fn new(token: TokenResponse, email: String, url: &str) -> Self {
        Self {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            email,
            user_id: token.user.id,
            expires_at: chrono::Utc::now().timestamp() + token.expires_in,
            url: url.to_string(),
        }
    }

    fn is_expired(&self) -> bool {
        self.expires_at <= chrono::Utc::now().timestamp() + TOKEN_EXPIRY_MARGIN_SECS
    }
}

fn session_path() -> PathBuf {
    crate::core::cite_home().join("session.json")
}

fn load_session() -> Option<Session> {
    let content = std::fs::read_to_string(session_path()).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_session(session: &Session) -> Result<(), CiteError> {
    crate::core::write_private(&session_path(), &serde_json::to_vec_pretty(session)?)?;
    Ok(())
}

async fn request_token(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    grant_type: &str,
    body: Value,
) -> Result<TokenResponse, CiteError> {
    let url = format!("{base_url}/auth/v1/token?grant_type={grant_type}");
    let response = client
        .post(&url)
        .header("apikey", api_key)
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CiteError::Auth(format!("HTTP {status} - {body}")));
    }
    serde_json::from_str(&body).map_err(|e| CiteError::Auth(format!("Invalid token response: {e}")))
}

async fn active_session(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Option<Session>, CiteError> {
    let Some(session) = load_session().filter(|s| s.url == base_url) else {
        return Ok(None);
    };
    if !session.is_expired() {
        return Ok(Some(session));
    }

    let body = serde_json::json!({ "refresh_token": session.refresh_token });
    let token = request_token(client, base_url, api_key, "refresh_token", body)
        .await
        .map_err(|e| CiteError::Auth(format!("Session expired; run 'cite login' again ({e})")))?;
    let refreshed = Session::new(token, session.email, base_url);
    save_session(&refreshed)?;
    info!("Refreshed login session for {}", refreshed.email);
    Ok(Some(refreshed))
}

fn resolve_bearer(backend: &BackendConfig, session: Option<&Session>) -> Result<String, CiteError> {
    if let Some(s) = session {
        return Ok(s.access_token.clone());
    }
    match backend.api_key.as_deref() {
        Some(key) if !key.is_empty() => Ok(key.to_string()),
        _ => Err(CiteError::Auth(
            "Not logged in and no backend.api_key configured. Run 'cite login' or set the key in cite.toml"
                .to_string(),
        )),
    }
}

pub async fn login(
    project_backend: Option<BackendConfig>,
    email: Option<String>,
    password: Option<String>,
) -> Result<(), CiteError> {
    let backend = match resolve_backend(project_backend.as_ref()) {
        Ok(backend) => backend,
        Err(_) => prompt_credentials()?,
    };
    let url = base_url(&backend);
    let api_key = backend.api_key.as_deref().unwrap_or_default();
    if url.is_empty() || api_key.is_empty() {
        return Err(CiteError::Auth(
            "A Supabase URL and API key are required for login. Set them in [backend] in cite.toml, ~/.cite/credentials.toml, or CITE_SUPABASE_URL/CITE_SUPABASE_API_KEY."
                .to_string(),
        ));
    }

    let email = match email {
        Some(e) => e,
        None => prompt_line("Email: ")?,
    };
    let password = match password {
        Some(p) => p,
        None => prompt_secret("Password: ")?,
    };

    let client = reqwest::Client::new();
    let body = serde_json::json!({ "email": email, "password": password });
    let token = request_token(&client, url, api_key, "password", body)
        .await
        .map_err(|e| CiteError::Auth(format!("Login failed: {e}")))?;

    let session = Session::new(token, email, url);
    save_session(&session)?;
    info!("Logged in as {}", session.email);

    let api = Supabase::new(client, url, api_key, &session.access_token);
    match fetch_user_artists(&api, &session.user_id).await {
        Ok(artists) if artists.is_empty() => {
            warn!("No artist linked to this account");
            match prompt_create_artist(&api, &session.user_id).await? {
                Some((id, name)) => info!("Created artist '{name}' ({id})"),
                None => info!("Skipped artist creation"),
            }
        }
        Ok(artists) => {
            info!("Associated artist(s):");
            for (id, name) in &artists {
                info!("  - {name} ({id})");
            }
        }
        Err(e) => warn!("Could not fetch artists: {e}"),
    }

    Ok(())
}

async fn fetch_user_artists(
    api: &Supabase,
    user_id: &str,
) -> Result<Vec<(String, String)>, CiteError> {
    let rows = api
        .select("artists", &format!("select=id,name&user_id=eq.{user_id}"))
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some((
                r["id"].as_str()?.to_string(),
                r["name"].as_str()?.to_string(),
            ))
        })
        .collect())
}

async fn prompt_create_artist(
    api: &Supabase,
    user_id: &str,
) -> Result<Option<(String, String)>, CiteError> {
    let name = prompt_line("Artist name: ")?;
    if name.is_empty() {
        return Ok(None);
    }
    let description = prompt_line("Description (optional): ")?;
    let website = prompt_line("Website URL (optional): ")?;

    let payload = artist_row(&name, user_id, &description, &website);
    let created = api.insert_returning("artists", &payload).await?;
    let id = created["id"].as_str().unwrap_or_default().to_string();
    Ok(Some((id, name)))
}

fn artist_row(name: &str, user_id: &str, description: &str, website: &str) -> Row {
    let mut payload = row([
        ("name", Value::from(name)),
        ("user_id", Value::from(user_id)),
    ]);
    if !description.is_empty() {
        payload.insert("description".into(), Value::from(description));
    }
    if !website.is_empty() {
        let website = if website.contains("://") {
            website.to_string()
        } else {
            format!("https://{website}")
        };
        payload.insert("website_url".into(), Value::String(website));
    }
    payload
}

fn prompt_line(label: &str) -> Result<String, CiteError> {
    print!("{label}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

fn prompt_secret(label: &str) -> Result<String, CiteError> {
    if !std::io::stdin().is_terminal() {
        return prompt_line(label);
    }
    print!("{label}");
    let _ = std::io::stdout().flush();
    terminal::enable_raw_mode()?;
    let secret = read_hidden_line();
    let _ = terminal::disable_raw_mode();
    println!();
    secret
}

fn read_hidden_line() -> Result<String, CiteError> {
    let mut secret = String::new();
    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Enter => return Ok(secret),
            KeyCode::Backspace => {
                secret.pop();
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Err(CiteError::Auth("Login cancelled".to_string()));
            }
            KeyCode::Char(c) => secret.push(c),
            _ => {}
        }
    }
}

fn prompt_credentials() -> Result<BackendConfig, CiteError> {
    println!("No credentials found. Let's set them up.");
    let url = prompt_line("Supabase URL: ")?;
    let api_key = prompt_secret("Supabase API key (anon/public): ")?;
    save_credentials(&SupabaseCredentials {
        url: url.clone(),
        api_key: api_key.clone(),
    })?;
    Ok(BackendConfig {
        url: Some(url),
        api_key: Some(api_key),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(url: &str, key: &str) -> BackendConfig {
        BackendConfig {
            url: Some(url.to_string()),
            api_key: Some(key.to_string()),
        }
    }

    fn token(expires_in: i64) -> TokenResponse {
        TokenResponse {
            access_token: "user-jwt".into(),
            refresh_token: "refresh".into(),
            expires_in,
            user: TokenUser { id: "uid".into() },
        }
    }

    #[test]
    fn test_resolve_bearer_uses_inline_when_no_session() {
        assert_eq!(
            resolve_bearer(&backend("https://x.supabase.co", "inline"), None).unwrap(),
            "inline"
        );
    }

    #[test]
    fn test_resolve_bearer_prefers_session() {
        let session = Session::new(token(3600), "a@b.c".into(), "https://x.supabase.co");
        assert_eq!(
            resolve_bearer(&backend("https://x.supabase.co", "inline"), Some(&session)).unwrap(),
            "user-jwt"
        );
    }

    #[test]
    fn test_resolve_bearer_errors_when_empty() {
        assert!(resolve_bearer(&backend("https://x.supabase.co", ""), None).is_err());
    }

    #[test]
    fn test_session_expiry() {
        assert!(!Session::new(token(3600), "e".into(), "u").is_expired());
        assert!(Session::new(token(30), "e".into(), "u").is_expired());
    }

    #[test]
    fn test_artist_row_is_owned_by_the_user() {
        let payload = artist_row("Aoux", "uid", "", "aoux.ai");
        assert_eq!(payload["user_id"], "uid");
        assert_eq!(payload["website_url"], "https://aoux.ai");
        assert!(!payload.contains_key("description"));
    }

    #[tokio::test]
    async fn test_fetch_user_artists_filters_by_owner() {
        let server = httpmock::MockServer::start();
        let owned = server.mock(|w, t| {
            w.method(httpmock::Method::GET)
                .path("/rest/v1/artists")
                .query_param("user_id", "eq.uid");
            t.status(200)
                .json_body(serde_json::json!([{ "id": "a1", "name": "Mine" }]));
        });
        let api = Supabase::new(reqwest::Client::new(), &server.base_url(), "k", "t");
        let artists = fetch_user_artists(&api, "uid").await.unwrap();
        assert_eq!(artists, vec![("a1".to_string(), "Mine".to_string())]);
        owned.assert();
    }
}

#[cfg(test)]
mod credentials_tests {
    use super::*;
    use std::env;
    use std::sync::{Mutex, MutexGuard};

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
    fn test_expired_session_is_refreshed_and_other_projects_ignored() {
        let _lock = env_lock();
        let home = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set("HOME", home.path().to_str().unwrap());
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(refresh_scenario());
    }

    async fn refresh_scenario() {
        let server = httpmock::MockServer::start();
        let refresh = server.mock(|w, t| {
            w.method(httpmock::Method::POST)
                .path("/auth/v1/token")
                .query_param("grant_type", "refresh_token")
                .json_body(serde_json::json!({ "refresh_token": "old-refresh" }));
            t.status(200).json_body(serde_json::json!({
                "access_token": "new-jwt",
                "refresh_token": "new-refresh",
                "expires_in": 3600,
                "user": { "id": "uid" }
            }));
        });
        let url = server.base_url();
        let mut expired = Session::new(
            TokenResponse {
                access_token: "old-jwt".into(),
                refresh_token: "old-refresh".into(),
                expires_in: 0,
                user: TokenUser { id: "uid".into() },
            },
            "a@b.c".into(),
            &url,
        );
        save_session(&expired).unwrap();
        let client = reqwest::Client::new();

        let session = active_session(&client, &url, "k").await.unwrap().unwrap();
        assert_eq!(session.access_token, "new-jwt");
        assert_eq!(
            load_session().unwrap().refresh_token,
            "new-refresh",
            "saved"
        );
        refresh.assert();

        expired.url = "https://other.supabase.co".into();
        save_session(&expired).unwrap();
        assert!(active_session(&client, &url, "k").await.unwrap().is_none());
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
