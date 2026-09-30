use std::io::Write;
use std::path::PathBuf;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::RequestBuilder;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};
use uuid::Uuid;

use crate::core::CiteError;
use crate::core::compiler::{BundlePodcast, BundleTimeline, ContentBundle, word_count};
use crate::core::credentials;
use crate::core::db::DbManager;
use crate::core::manifest::BackendConfig;
use crate::core::metadata::{TimelineEntry, TimelineItem};
use crate::core::project::{DeployReport, ProjectContext};

const ASSETS_BUCKET: &str = "assets";
const PODCASTS_BUCKET: &str = "podcasts";
const DEFAULT_CATEGORY: &str = "General";

#[derive(Debug, Clone)]
struct DeployContext {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    bearer: String,
    root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeploymentRecord {
    deployment_id: String,
    storage_path: String,
    news_ids: Vec<i64>,
    timeline_ids: Vec<i64>,
    #[serde(default)]
    asset_paths: Vec<String>,
}

#[derive(Debug)]
struct UploadedAsset {
    storage_path: String,
}

#[derive(Debug, Clone)]
struct Category {
    id: i64,
    name: String,
}

fn with_auth(builder: RequestBuilder, api_key: &str, bearer: &str) -> RequestBuilder {
    builder
        .header("apikey", api_key)
        .header("Authorization", format!("Bearer {bearer}"))
}

fn encode_url(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn base_url(backend: &BackendConfig) -> &str {
    backend
        .staging_url
        .as_deref()
        .unwrap_or_default()
        .trim_end_matches('/')
}

fn build_context(
    ctx: &ProjectContext,
    backend: &BackendConfig,
) -> Result<DeployContext, CiteError> {
    let session = load_session();
    Ok(DeployContext {
        client: reqwest::Client::new(),
        base_url: base_url(backend).to_string(),
        api_key: backend.staging_service_key.clone().unwrap_or_default(),
        bearer: resolve_bearer(backend, session.as_ref())?,
        root: ctx.root.clone(),
    })
}

async fn ensure_success(
    response: reqwest::Response,
    context: impl std::fmt::Display,
) -> Result<reqwest::Response, CiteError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(CiteError::Deploy(format!(
        "{context}: HTTP {status} - {body}"
    )))
}

async fn load_bundle(ctx: &ProjectContext) -> Result<ContentBundle, CiteError> {
    let bundle_path = ctx.build_dir().join("content.json");
    let bundle_str = match tokio::fs::read_to_string(&bundle_path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CiteError::Config(
                "No build artifact found. Run 'cite build' first.".to_string(),
            ));
        }
        Err(e) => return Err(e.into()),
    };
    Ok(serde_json::from_str(&bundle_str)?)
}

pub async fn deploy(
    db: &DbManager,
    ctx: &ProjectContext,
    dry_run: bool,
) -> Result<String, CiteError> {
    let backend = resolve_backend_config(ctx)?;
    let bundle = load_bundle(ctx).await?;
    let deployment_id = Uuid::new_v4().to_string();
    let artist_id = bundle.artist_id.trim();

    info!("Deploying: {deployment_id}");

    if dry_run {
        warn!("DRY RUN - no data will be sent");
        info!("Podcast items: {}", bundle.podcasts.len());
        info!("Timeline groups: {}", bundle.timelines.len());
        if !artist_id.is_empty() {
            info!("Artist ID: {artist_id}");
        }
        let _ = db
            .record_deployment(&DeployReport {
                project_id: ctx.project_id(),
                deployment_id,
                storage_path: String::new(),
                news_count: bundle.podcasts.len() as i64,
                timeline_count: bundle.timelines.len() as i64,
                asset_count: 0,
                success: true,
                dry_run: true,
            })
            .await;
        return Ok("Dry run complete".to_string());
    }

    let artist_id = Uuid::parse_str(artist_id).map_err(|_| {
        CiteError::Config("artist_id in content.json must be a valid UUID".to_string())
    })?;
    let dctx = build_context(ctx, &backend)?;
    let object_path = format!("{artist_id}/{}/{deployment_id}.json", bundle.project);
    let storage_path = format!("{ASSETS_BUCKET}/{object_path}");

    let mut record = DeploymentRecord {
        deployment_id,
        storage_path,
        news_ids: Vec::new(),
        timeline_ids: Vec::new(),
        asset_paths: Vec::new(),
    };

    ensure_artist_exists(&dctx, artist_id).await?;
    let categories = fetch_categories(&dctx).await?;

    for pod in &bundle.podcasts {
        let pod_groups: Vec<&BundleTimeline> = bundle
            .timelines
            .iter()
            .filter(|tl| tl.podcast_id == pod.id)
            .collect();
        match deploy_podcast(&dctx, pod, &pod_groups, artist_id, &categories).await {
            Ok((news_id, timeline_ids, asset_paths)) => {
                record.news_ids.push(news_id);
                record.timeline_ids.extend(timeline_ids);
                record.asset_paths.extend(asset_paths);
            }
            Err(e) => {
                warn!("Deploy failed partway: {e}");
                record_deployment(db, ctx, &record, false).await;
                persist_deployment_record(ctx, &record).await?;
                return Err(e);
            }
        }
    }

    let bundle_json = serde_json::to_vec_pretty(&bundle)?;
    upload_bytes(
        &dctx,
        ASSETS_BUCKET,
        &object_path,
        &bundle_json,
        "application/json",
    )
    .await?;
    info!("Uploaded bundle to {}", record.storage_path);

    persist_deployment_record(ctx, &record).await?;
    record_deployment(db, ctx, &record, true).await;

    Ok(format!(
        "Deployed {} podcast(s), {} timeline(s), {} asset(s)",
        record.news_ids.len(),
        record.timeline_ids.len(),
        record.asset_paths.len()
    ))
}

async fn record_deployment(
    db: &DbManager,
    ctx: &ProjectContext,
    record: &DeploymentRecord,
    success: bool,
) {
    let _ = db
        .record_deployment(&DeployReport {
            project_id: ctx.project_id(),
            deployment_id: record.deployment_id.clone(),
            storage_path: record.storage_path.clone(),
            news_count: record.news_ids.len() as i64,
            timeline_count: record.timeline_ids.len() as i64,
            asset_count: record.asset_paths.len() as i64,
            success,
            dry_run: false,
        })
        .await;
}

pub async fn deploy_staging(
    db: &DbManager,
    ctx: &ProjectContext,
    dry_run: bool,
) -> Result<String, CiteError> {
    let bundle = load_bundle(ctx).await?;
    let deployment_id = Uuid::new_v4().to_string();
    let podcasts = bundle.podcasts.len() as i64;
    let timelines = bundle.timelines.len() as i64;

    info!("Staging deployment: {deployment_id}");

    if !dry_run {
        db.sync_project(ctx, &bundle).await?;
    }
    let _ = db
        .record_deployment(&DeployReport {
            project_id: ctx.project_id(),
            deployment_id: deployment_id.clone(),
            storage_path: String::new(),
            news_count: podcasts,
            timeline_count: timelines,
            asset_count: 0,
            success: !dry_run,
            dry_run,
        })
        .await;

    if dry_run {
        warn!("DRY RUN - no data written to cite.db");
        return Ok("Staging dry run complete".to_string());
    }

    persist_deployment_record(
        ctx,
        &DeploymentRecord {
            deployment_id,
            storage_path: String::new(),
            news_ids: Vec::new(),
            timeline_ids: Vec::new(),
            asset_paths: Vec::new(),
        },
    )
    .await?;

    Ok(format!(
        "Staged {podcasts} podcast(s), {timelines} timeline(s) to local database"
    ))
}

async fn deploy_podcast(
    dctx: &DeployContext,
    podcast: &BundlePodcast,
    pod_groups: &[&BundleTimeline],
    artist_id: Uuid,
    categories: &[Category],
) -> Result<(i64, Vec<i64>, Vec<String>), CiteError> {
    let title = &podcast.podcast.title;
    let content = podcast.content.as_deref();
    let category_id =
        resolve_category_id(dctx, podcast.podcast.category.as_deref(), categories).await?;

    let fallback_url = format!("cite://podcasts/{}", podcast.id);
    let url_id = ensure_url_id(
        dctx,
        podcast.podcast.source_url.as_deref(),
        &fallback_url,
        content.map(word_count),
    )
    .await?;
    let news_id = insert_news_row(dctx, title, content, category_id, url_id, artist_id).await?;
    info!("Created news item: {title} (id={news_id})");

    let mut asset_paths = Vec::new();

    let thumb_stem = format!("{artist_id}/news_{news_id}");
    if let Some(asset) = upload_optional_asset(
        dctx,
        podcast.podcast.thumbnail.as_deref(),
        ASSETS_BUCKET,
        &thumb_stem,
    )
    .await?
    {
        update_news_thumbnail(dctx, news_id, &asset.storage_path).await?;
        info!("Uploaded thumbnail: {}", asset.storage_path);
        asset_paths.push(asset.storage_path);
    }

    let audio_stem = format!("{artist_id}/podcast_{news_id}");
    if let Some(audio) = upload_optional_asset(
        dctx,
        podcast.podcast.audio.as_deref(),
        PODCASTS_BUCKET,
        &audio_stem,
    )
    .await?
    {
        let duration_minutes = podcast
            .audio_meta
            .as_ref()
            .map(|meta| meta.duration_secs / 60.0);
        insert_podcast_row(dctx, news_id, title, &audio.storage_path, duration_minutes).await?;
        info!("Created podcast: {title}");
        asset_paths.push(audio.storage_path);
    }

    let timeline_ids =
        deploy_timeline(dctx, news_id, &podcast.podcast.timeline, pod_groups).await?;

    Ok((news_id, timeline_ids, asset_paths))
}

async fn deploy_timeline(
    dctx: &DeployContext,
    parent_news_id: i64,
    items: &[TimelineItem],
    citation_groups: &[&BundleTimeline],
) -> Result<Vec<i64>, CiteError> {
    let mut timeline_ids = Vec::new();

    for item in items {
        match item {
            TimelineItem::News(id) => {
                if lookup_row_id(dctx, "news", "id", &id.to_string())
                    .await?
                    .is_none()
                {
                    return Err(CiteError::Deploy(format!(
                        "Timeline news item {id} does not exist in the database"
                    )));
                }
                let sort_order = (timeline_ids.len() + 1) as i64;
                let row_id = insert_row(
                    dctx,
                    "timeline_news",
                    build_map([
                        ("parent_news_id", Value::Number(parent_news_id.into())),
                        ("child_news_id", Value::Number((*id).into())),
                        ("sort_order", Value::Number(sort_order.into())),
                    ]),
                )
                .await?;
                info!("Linked news {parent_news_id} -> {id}");
                timeline_ids.push(row_id);
            }
            TimelineItem::Citation(path) => {
                let Some(group) = citation_groups.iter().find(|g| g.source == *path) else {
                    continue;
                };
                for entry in &group.entries {
                    let sort_order = (timeline_ids.len() + 1) as i64;
                    if let Some(id) =
                        deploy_citation_event(dctx, parent_news_id, entry, sort_order).await?
                    {
                        timeline_ids.push(id);
                    }
                }
            }
        }
    }

    Ok(timeline_ids)
}

async fn deploy_citation_event(
    dctx: &DeployContext,
    parent_news_id: i64,
    entry: &TimelineEntry,
    sort_order: i64,
) -> Result<Option<i64>, CiteError> {
    let title = entry.title.trim();
    if title.is_empty() {
        warn!("Skipping timeline entry without a title");
        return Ok(None);
    }

    let mut payload = build_map([
        ("parent_news_id", Value::Number(parent_news_id.into())),
        ("title", Value::String(title.to_string())),
        ("sort_order", Value::Number(sort_order.into())),
    ]);

    if let Some(summary) = entry
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        payload.insert("description".into(), Value::String(summary.to_string()));
    }

    let url = entry
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .or_else(|| {
            entry
                .link
                .as_deref()
                .map(str::trim)
                .filter(|l| !l.is_empty())
        });
    if let Some(url) = url {
        let url_id = ensure_url_id(dctx, Some(url), url, None).await?;
        payload.insert("url_id".into(), Value::Number(url_id.into()));
    }

    match entry.date.as_deref().and_then(event_date_rfc3339) {
        Some(event_date) => {
            payload.insert("event_date".into(), Value::String(event_date));
        }
        None => {
            if let Some(date) = entry.date.as_deref() {
                warn!("Unrecognized timeline date '{date}', deploying without event_date");
            }
        }
    }

    info!("Deployed timeline event: {title}");
    insert_row(dctx, "timeline_news", payload).await.map(Some)
}

fn event_date_rfc3339(date: &str) -> Option<String> {
    let s = date.trim();
    let digits = s.as_bytes();
    let year_only = s.len() == 4 && digits[..4].iter().all(|b| b.is_ascii_digit());
    let year_month = s.len() == 7
        && digits[4] == b'-'
        && digits[..4].iter().all(|b| b.is_ascii_digit())
        && digits[5..7].iter().all(|b| b.is_ascii_digit());

    if year_only {
        Some(format!("{s}-01-01T00:00:00Z"))
    } else if year_month {
        Some(format!("{s}-01T00:00:00Z"))
    } else {
        None
    }
}

async fn persist_deployment_record(
    ctx: &ProjectContext,
    record: &DeploymentRecord,
) -> Result<(), CiteError> {
    let deployments_dir = ctx.build_dir().join("deployments");
    tokio::fs::create_dir_all(&deployments_dir).await?;
    let path = deployments_dir.join(format!("{}.json", record.deployment_id));
    tokio::fs::write(path, serde_json::to_vec_pretty(record)?).await?;
    Ok(())
}

async fn load_deployment_record(
    ctx: &ProjectContext,
    deployment_id: &str,
) -> Result<(std::path::PathBuf, DeploymentRecord), CiteError> {
    let path = ctx
        .build_dir()
        .join("deployments")
        .join(format!("{}.json", deployment_id));
    let json = tokio::fs::read_to_string(&path).await.map_err(|_| {
        CiteError::Deploy(format!(
            "No local deployment record found for '{deployment_id}'. Run deploy first."
        ))
    })?;
    let record: DeploymentRecord = serde_json::from_str(&json)?;
    Ok((path, record))
}

async fn delete_row_by_id(dctx: &DeployContext, table: &str, id: i64) -> Result<(), CiteError> {
    let url = format!("{}/rest/v1/{table}?id=eq.{id}", dctx.base_url);
    let response = with_auth(dctx.client.delete(&url), &dctx.api_key, &dctx.bearer)
        .send()
        .await?;
    ensure_success(response, format!("Failed to delete {table} row {id}")).await?;
    Ok(())
}

async fn delete_storage_object(dctx: &DeployContext, storage_path: &str) -> Result<(), CiteError> {
    let (bucket, object_path) = storage_path
        .split_once('/')
        .unwrap_or((ASSETS_BUCKET, storage_path));
    let url = format!("{}/storage/v1/object/{bucket}/{object_path}", dctx.base_url);
    let response = with_auth(dctx.client.delete(&url), &dctx.api_key, &dctx.bearer)
        .send()
        .await?;
    ensure_success(
        response,
        format!("Failed to delete storage object {storage_path}"),
    )
    .await?;
    Ok(())
}

fn resolve_backend_config(ctx: &ProjectContext) -> Result<BackendConfig, CiteError> {
    if let Some(backend) = &ctx.manifest.backend {
        return Ok(backend.clone());
    }
    let creds = credentials::load_credentials()?;
    Ok(BackendConfig {
        staging_url: Some(creds.url),
        staging_service_key: Some(creds.api_key),
    })
}

pub async fn rollback(ctx: &ProjectContext, deployment_id: &str) -> Result<String, CiteError> {
    let backend = resolve_backend_config(ctx)?;

    let (record_path, record) = load_deployment_record(ctx, deployment_id).await?;
    let dctx = build_context(ctx, &backend)?;

    warn!("Rolling back deployment: {deployment_id}");

    for timeline_id in &record.timeline_ids {
        delete_row_by_id(&dctx, "timeline_news", *timeline_id).await?;
        info!("Cleared timeline event {timeline_id}");
    }

    for news_id in &record.news_ids {
        delete_row_by_id(&dctx, "news", *news_id).await?;
        info!("Cleared news {news_id}");
    }

    for asset_path in &record.asset_paths {
        if let Err(e) = delete_storage_object(&dctx, asset_path).await {
            warn!("Failed to delete asset {asset_path}: {e}");
        } else {
            info!("Cleared asset {asset_path}");
        }
    }

    if let Err(e) = delete_storage_object(&dctx, &record.storage_path).await {
        warn!("Failed to delete storage: {e}");
    } else {
        info!("Cleared storage object");
    }

    if let Err(e) = tokio::fs::remove_file(&record_path).await {
        warn!("Failed to remove local deployment record: {e}");
    }

    Ok("Rollback complete".to_string())
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct Session {
    access_token: String,
    refresh_token: String,
    email: String,
}

fn session_path() -> PathBuf {
    crate::core::cite_home().join("session.json")
}

fn load_session() -> Option<Session> {
    let path = session_path();
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_session(session: &Session) -> Result<(), CiteError> {
    crate::core::write_private(&session_path(), &serde_json::to_vec_pretty(session)?)?;
    Ok(())
}

fn resolve_bearer(backend: &BackendConfig, session: Option<&Session>) -> Result<String, CiteError> {
    if let Some(s) = session {
        return Ok(s.access_token.clone());
    }
    match backend.staging_service_key.as_deref() {
        Some(key) if !key.is_empty() => Ok(key.to_string()),
        _ => Err(CiteError::Auth(
            "Not logged in and no backend.staging_service_key configured. Run 'cite login' or set the key in cite.toml"
                .to_string(),
        )),
    }
}

pub async fn login(
    creds: Option<credentials::SupabaseCredentials>,
    backend_config: Option<crate::core::manifest::BackendConfig>,
    email: Option<String>,
    password: Option<String>,
) -> Result<(), CiteError> {
    let creds = match creds {
        Some(c) => c,
        None => credentials::load_credentials().or_else(|_| {
            if let Some(b) = backend_config {
                Ok(credentials::SupabaseCredentials {
                    url: b.staging_url.unwrap_or_default(),
                    api_key: b.staging_service_key.unwrap_or_default(),
                })
            } else {
                prompt_credentials()
            }
        })?,
    };

    if creds.api_key.is_empty() {
        return Err(CiteError::Auth(
            "Supabase API key is required for login. Set it in ~/.cite/credentials.toml or CITE_SUPABASE_API_KEY env var."
                .to_string(),
        ));
    }

    let backend = BackendConfig {
        staging_url: Some(creds.url.clone()),
        staging_service_key: Some(creds.api_key.clone()),
    };

    let email = match email {
        Some(e) => e,
        None => prompt_line("Email: ")?,
    };
    let password = match password {
        Some(p) => p,
        None => prompt_line("Password: ")?,
    };

    let client = reqwest::Client::new();
    let url = format!("{}/auth/v1/token?grant_type=password", base_url(&backend));
    let response = client
        .post(&url)
        .header(
            "apikey",
            backend.staging_service_key.as_deref().unwrap_or_default(),
        )
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "email": email, "password": password }))
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CiteError::Auth(format!(
            "Login failed: HTTP {status} - {body}"
        )));
    }

    let token: TokenResponse = serde_json::from_str(&body)
        .map_err(|e| CiteError::Auth(format!("Invalid login response: {e}")))?;

    let session = Session {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        email,
    };
    save_session(&session)?;
    info!("Logged in as {}", session.email);

    match fetch_user_artists(&client, &backend, &session.access_token).await {
        Ok(artists) if artists.is_empty() => {
            warn!("No artist linked to this account");
            match prompt_create_artist(&client, &backend, &session.access_token).await? {
                Some((id, name)) => {
                    info!("Created artist '{name}' ({id})");
                }
                None => {
                    info!("Skipped artist creation");
                }
            }
        }
        Ok(artists) => {
            info!("Associated artist(s):");
            for (id, name) in &artists {
                info!("  - {name} ({id})");
            }
        }
        Err(e) => {
            warn!("Could not fetch artists: {e}");
        }
    }

    Ok(())
}

async fn fetch_user_artists(
    client: &reqwest::Client,
    backend: &BackendConfig,
    token: &str,
) -> Result<Vec<(String, String)>, CiteError> {
    let url = format!("{}/rest/v1/artists?select=id,name", base_url(backend));
    let resp = with_auth(
        client.get(&url),
        backend.staging_service_key.as_deref().unwrap_or_default(),
        token,
    )
    .send()
    .await?;
    if !resp.status().is_success() {
        return Err(CiteError::Auth(format!(
            "Failed to fetch artists: HTTP {}",
            resp.status()
        )));
    }
    let rows: Vec<Value> = resp.json().await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let id = r.get("id")?.as_str()?.to_string();
            let name = r.get("name")?.as_str()?.to_string();
            Some((id, name))
        })
        .collect())
}

fn prompt_line(label: &str) -> Result<String, CiteError> {
    print!("{label}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

async fn prompt_create_artist(
    client: &reqwest::Client,
    backend: &BackendConfig,
    token: &str,
) -> Result<Option<(String, String)>, CiteError> {
    let name = prompt_line("Artist name: ")?;
    if name.is_empty() {
        return Ok(None);
    }
    let description = prompt_line("Description (optional): ")?;
    let website = prompt_line("Website URL (optional): ")?;

    let mut payload = build_map([("name", Value::String(name.clone()))]);
    if !description.is_empty() {
        payload.insert("description".into(), Value::String(description));
    }
    if !website.is_empty() {
        payload.insert("website_url".into(), Value::String(website));
    }

    let url = format!("{}/rest/v1/artists", base_url(backend));
    let resp = with_auth(
        client.post(&url),
        backend.staging_service_key.as_deref().unwrap_or_default(),
        token,
    )
    .header("Prefer", "return=representation")
    .json(&payload)
    .send()
    .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(CiteError::Auth(format!(
            "Failed to create artist: HTTP {status} - {body}"
        )));
    }
    let row: Value = resp.json().await?;
    let id = extract_uuid(&row).unwrap_or_default();
    Ok(Some((id, name)))
}

fn build_map<const N: usize>(fields: [(&str, Value); N]) -> serde_json::Map<String, Value> {
    fields
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

fn extract_id(value: &Value) -> Option<i64> {
    match value {
        Value::Array(rows) => rows
            .first()
            .and_then(|row| row.get("id").and_then(|id| id.as_i64())),
        Value::Object(map) => map.get("id").and_then(|id| id.as_i64()),
        _ => None,
    }
}

fn extract_uuid(value: &Value) -> Option<String> {
    match value {
        Value::Array(rows) => rows
            .first()
            .and_then(|row| row.get("id").and_then(|id| id.as_str().map(String::from))),
        Value::Object(map) => map.get("id").and_then(|id| id.as_str().map(String::from)),
        _ => None,
    }
}

async fn lookup_row_id(
    dctx: &DeployContext,
    table: &str,
    field: &str,
    value: &str,
) -> Result<Option<i64>, CiteError> {
    let url = format!(
        "{}/rest/v1/{table}?{field}=eq.{}",
        dctx.base_url,
        encode_url(value)
    );
    let resp = with_auth(dctx.client.get(&url), &dctx.api_key, &dctx.bearer)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let rows: Vec<Value> = resp.json().await?;
    Ok(rows
        .first()
        .and_then(|row| row.get("id").and_then(|id| id.as_i64())))
}

async fn insert_row(
    dctx: &DeployContext,
    table: &str,
    payload: serde_json::Map<String, Value>,
) -> Result<i64, CiteError> {
    let url = format!("{}/rest/v1/{table}", dctx.base_url);
    let response = with_auth(dctx.client.post(&url), &dctx.api_key, &dctx.bearer)
        .header("Prefer", "return=representation")
        .json(&payload)
        .send()
        .await?;
    let response = ensure_success(response, format!("Failed to insert into {table}")).await?;

    let row: Value = response.json().await?;
    extract_id(&row)
        .ok_or_else(|| CiteError::Deploy(format!("Could not get {table} id from response")))
}

async fn ensure_artist_exists(dctx: &DeployContext, artist_id: Uuid) -> Result<(), CiteError> {
    let url = format!("{}/rest/v1/artists?id=eq.{}", dctx.base_url, artist_id);
    let resp = with_auth(dctx.client.get(&url), &dctx.api_key, &dctx.bearer)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(CiteError::Auth(format!(
            "Failed to verify artist {artist_id}: HTTP {}",
            resp.status()
        )));
    }
    let rows: Vec<Value> = resp.json().await?;
    if rows.is_empty() {
        return Err(CiteError::Auth(format!(
            "Artist '{artist_id}' does not exist in the database. Create the artist before deploying."
        )));
    }
    Ok(())
}

async fn fetch_categories(dctx: &DeployContext) -> Result<Vec<Category>, CiteError> {
    let url = format!("{}/rest/v1/categories?select=id,name", dctx.base_url);
    let resp = with_auth(dctx.client.get(&url), &dctx.api_key, &dctx.bearer)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(CiteError::Deploy(format!(
            "Failed to fetch categories: HTTP {}",
            resp.status()
        )));
    }
    let rows: Vec<Value> = resp.json().await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Category {
                id: row.get("id")?.as_i64()?,
                name: row.get("name")?.as_str()?.to_string(),
            })
        })
        .collect())
}

async fn resolve_category_id(
    dctx: &DeployContext,
    category_name: Option<&str>,
    categories: &[Category],
) -> Result<i64, CiteError> {
    let name = category_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_CATEGORY);

    if let Some(category) = categories
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(name))
    {
        return Ok(category.id);
    }

    match insert_row(
        dctx,
        "categories",
        build_map([
            ("name", Value::String(name.to_string())),
            (
                "description",
                Value::String("Created automatically by cite".to_string()),
            ),
        ]),
    )
    .await
    {
        Ok(id) => Ok(id),
        Err(_) => {
            let available = fetch_categories(dctx).await?;
            if let Some(category) = available.iter().find(|c| c.name.eq_ignore_ascii_case(name)) {
                return Ok(category.id);
            }
            let names = available
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(CiteError::Deploy(format!(
                "Unknown category '{name}'. Available categories: {names}. Creating new categories requires elevated access."
            )))
        }
    }
}

async fn ensure_domain_id(
    dctx: &DeployContext,
    domain_name: &str,
) -> Result<Option<i64>, CiteError> {
    if let Some(id) = lookup_row_id(dctx, "domains", "domain_name", domain_name).await? {
        return Ok(Some(id));
    }

    // Domains are server-managed; a blocked insert is not fatal to the deployment.
    match insert_row(
        dctx,
        "domains",
        build_map([
            ("domain_name", Value::String(domain_name.to_string())),
            ("is_trusted", Value::Bool(false)),
        ]),
    )
    .await
    {
        Ok(id) => Ok(Some(id)),
        Err(_) => Ok(None),
    }
}

async fn ensure_url_id(
    dctx: &DeployContext,
    source_url: Option<&str>,
    fallback_url: &str,
    word_count: Option<i64>,
) -> Result<i64, CiteError> {
    let url_value = source_url
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(fallback_url);
    if let Some(id) = lookup_row_id(dctx, "urls", "url", url_value).await? {
        return Ok(id);
    }

    let mut payload = build_map([("url", Value::String(url_value.to_string()))]);
    if let Some(count) = word_count {
        payload.insert("word_count".into(), Value::Number(count.into()));
    }
    payload.insert("accessed_at".into(), Value::String(now_rfc3339()));

    if let Some(source_url) = source_url.filter(|value| !value.trim().is_empty())
        && let Some(domain_name) = extract_domain_name(source_url)
        && let Ok(Some(domain_id)) = ensure_domain_id(dctx, &domain_name).await
    {
        payload.insert("domain_id".into(), Value::Number(domain_id.into()));
    }

    insert_row(dctx, "urls", payload).await
}

async fn upload_optional_asset(
    dctx: &DeployContext,
    asset: Option<&str>,
    bucket: &str,
    object_stem: &str,
) -> Result<Option<UploadedAsset>, CiteError> {
    let Some(asset_path) = asset.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };

    let local_path = dctx.root.join(asset_path);
    if !local_path.exists() {
        return Ok(None);
    }

    let bytes = tokio::fs::read(&local_path).await?;
    let ext = local_path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("bin");
    let object_path = format!("{object_stem}.{ext}");
    let mime = mime_for_extension(ext);

    let storage_path = upload_bytes(dctx, bucket, &object_path, &bytes, mime).await?;

    Ok(Some(UploadedAsset { storage_path }))
}

async fn insert_news_row(
    dctx: &DeployContext,
    title: &str,
    content: Option<&str>,
    category_id: i64,
    url_id: i64,
    artist_id: Uuid,
) -> Result<i64, CiteError> {
    let mut payload = build_map([
        ("title", Value::String(title.to_string())),
        ("category_id", Value::Number(category_id.into())),
        ("url_id", Value::Number(url_id.into())),
        ("artist_id", Value::String(artist_id.to_string())),
        ("published_at", Value::String(now_rfc3339())),
    ]);

    if let Some(summary) = summarize_content(content) {
        payload.insert("summary".into(), Value::String(summary));
    }

    insert_row(dctx, "news", payload).await
}

async fn update_row(
    dctx: &DeployContext,
    table: &str,
    id: i64,
    payload: serde_json::Map<String, Value>,
) -> Result<(), CiteError> {
    let url = format!("{}/rest/v1/{table}?id=eq.{id}", dctx.base_url);
    let response = with_auth(dctx.client.patch(&url), &dctx.api_key, &dctx.bearer)
        .header("Prefer", "return=representation")
        .json(&payload)
        .send()
        .await?;
    ensure_success(response, format!("Failed to update {table} {id}")).await?;
    Ok(())
}

async fn update_news_thumbnail(
    dctx: &DeployContext,
    news_id: i64,
    thumbnail: &str,
) -> Result<(), CiteError> {
    update_row(
        dctx,
        "news",
        news_id,
        build_map([("thumbnail", Value::String(thumbnail.to_string()))]),
    )
    .await
}

async fn insert_podcast_row(
    dctx: &DeployContext,
    news_id: i64,
    title: &str,
    podcast_url: &str,
    duration_minutes: Option<f64>,
) -> Result<(), CiteError> {
    let mut payload = build_map([
        ("news_id", Value::Number(news_id.into())),
        ("title", Value::String(title.to_string())),
        ("podcast_url", Value::String(podcast_url.to_string())),
    ]);

    if let Some(duration) = duration_minutes.filter(|d| *d > 0.0)
        && let Some(value) = serde_json::Number::from_f64((duration * 100.0).round() / 100.0)
    {
        payload.insert("duration_minutes".into(), Value::Number(value));
    }

    insert_row(dctx, "podcasts", payload).await?;
    Ok(())
}

fn extract_domain_name(source_url: &str) -> Option<String> {
    let without_scheme = source_url
        .split_once("//")
        .map(|(_, rest)| rest)
        .unwrap_or(source_url);
    let domain = without_scheme.split('/').next()?.trim();
    if domain.is_empty() {
        None
    } else {
        Some(domain.to_string())
    }
}

fn prompt_credentials() -> Result<credentials::SupabaseCredentials, CiteError> {
    println!("No credentials found. Let's set them up.");
    let url = prompt_line("Supabase URL: ")?;
    let api_key = prompt_line("Supabase API key (anon/public): ")?;
    let creds = credentials::SupabaseCredentials { url, api_key };
    credentials::save_credentials(&creds)?;
    Ok(creds)
}

fn summarize_content(content: Option<&str>) -> Option<String> {
    let content = content?.trim();
    if content.is_empty() {
        return None;
    }

    let summary = content
        .split_whitespace()
        .take(50)
        .collect::<Vec<_>>()
        .join(" ");
    if summary.len() < content.len() {
        Some(format!("{summary}..."))
    } else {
        Some(summary)
    }
}

async fn upload_bytes(
    dctx: &DeployContext,
    bucket: &str,
    object_path: &str,
    bytes: &[u8],
    mime: &str,
) -> Result<String, CiteError> {
    let base_url = &dctx.base_url;
    let url = format!("{base_url}/storage/v1/object/{bucket}/{object_path}");

    let mut last_err = None;
    for attempt in 0..3 {
        let response = with_auth(dctx.client.post(&url), &dctx.api_key, &dctx.bearer)
            .header("Content-Type", mime)
            .body(bytes.to_vec())
            .send()
            .await;
        match response {
            Ok(r) if r.status().is_success() => {
                let storage_path = format!("{bucket}/{object_path}");
                info!("Uploaded {storage_path}");
                return Ok(storage_path);
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                last_err = Some(format!("HTTP {status} - {body}"));
            }
            Err(e) => last_err = Some(e.to_string()),
        }
        warn!(
            "Upload attempt {}/3 failed: {}",
            attempt + 1,
            last_err.as_ref().unwrap()
        );
        if attempt < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(500 * (attempt as u64 + 1))).await;
        }
    }

    Err(CiteError::Deploy(format!(
        "Failed to upload {object_path} after 3 attempts: {}",
        last_err.unwrap_or_default()
    )))
}

fn mime_for_extension(ext: &str) -> &'static str {
    match ext.to_lowercase().as_str() {
        "md" => "text/markdown",
        "rst" => "text/x-rst",
        "json" => "application/json",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(url: &str, key: &str) -> BackendConfig {
        BackendConfig {
            staging_url: Some(url.to_string()),
            staging_service_key: Some(key.to_string()),
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
        let session = Session {
            access_token: "user-jwt".into(),
            refresh_token: "refresh".into(),
            email: "a@b.c".into(),
        };
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
    fn test_extract_domain_name() {
        assert_eq!(
            extract_domain_name("https://example.com/path"),
            Some("example.com".to_string())
        );
        assert_eq!(
            extract_domain_name("http://a.b.c/d"),
            Some("a.b.c".to_string())
        );
        assert_eq!(
            extract_domain_name("scheme-less.example.com/foo"),
            Some("scheme-less.example.com".to_string())
        );
        assert_eq!(extract_domain_name(""), None);
    }

    #[test]
    fn test_summarize_content() {
        let long = "word ".repeat(80);
        let s = summarize_content(Some(&long)).unwrap();
        assert!(s.ends_with("..."));
        assert_eq!(s.split_whitespace().count(), 50);
        assert!(s.len() < long.len());
        assert_eq!(summarize_content(Some("   ")), None);
        assert_eq!(summarize_content(None), None);
    }

    #[test]
    fn test_event_date_rfc3339() {
        assert_eq!(
            event_date_rfc3339("2023").as_deref(),
            Some("2023-01-01T00:00:00Z")
        );
        assert_eq!(
            event_date_rfc3339(" 2023-03 ").as_deref(),
            Some("2023-03-01T00:00:00Z")
        );
        assert_eq!(event_date_rfc3339("march"), None);
        assert_eq!(event_date_rfc3339("2023-0"), None);
        assert_eq!(event_date_rfc3339(""), None);
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::core::compiler;
    use crate::core::manifest::{BackendConfig, BuildConfig, Manifest, ProjectConfig};
    use crate::core::project::ProjectContext;
    use httpmock::Method::PATCH;
    use httpmock::prelude::*;
    use std::path::Path;

    fn write_project(dir: &Path, staging_url: &str) {
        let manifest = Manifest {
            project: ProjectConfig {
                name: "test".into(),
                language: "en".into(),
                metadata_file: "metadata.yml".into(),
                artist_id: "11111111-1111-1111-1111-111111111111".into(),
            },
            build: BuildConfig::default(),
            backend: Some(BackendConfig {
                staging_url: Some(staging_url.into()),
                staging_service_key: Some("key".into()),
            }),
        };
        std::fs::write(dir.join("cite.toml"), toml::to_string(&manifest).unwrap()).unwrap();
        std::fs::write(
            dir.join("metadata.yml"),
            r#"
podcasts:
  - title: "Episode One"
    file: content/episode.md
    source_url: "https://example.com/episode"
    category: tech
    thumbnail: assets/image/cover.png
    audio: assets/audio/episode.mp3
    timeline:
      - content/papers.bib
      - 77
  - title: "Episode Two"
    file: content/episode-two.md
"#,
        )
        .unwrap();

        std::fs::create_dir_all(dir.join("content")).unwrap();
        std::fs::write(
            dir.join("content/episode.md"),
            "# Episode One\nWelcome to the show.",
        )
        .unwrap();
        std::fs::write(dir.join("content/episode-two.md"), "# Episode Two\nLater.").unwrap();
        std::fs::write(
            dir.join("content/papers.bib"),
            r#"
@article{ref2023,
  title = {A Great Paper},
  author = {Doe, J.},
  year = {2023},
  month = mar,
  abstract = {Important findings.},
}

@article{ref2024,
  title = {Linked Story},
  author = {Roe, J.},
  year = {2024},
  abstract = {Related coverage.},
  link = {https://example.com/related}
}
"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("assets/image")).unwrap();
        std::fs::write(dir.join("assets/image/cover.png"), "png").unwrap();
        std::fs::create_dir_all(dir.join("assets/audio")).unwrap();
        std::fs::write(dir.join("assets/audio/episode.mp3"), "mp3").unwrap();
    }

    async fn setup(staging_url: &str) -> (tempfile::TempDir, ProjectContext, DbManager) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        write_project(dir.path(), staging_url);
        let ctx = ProjectContext::load(dir.path()).unwrap();
        let db = DbManager::open_path(&db_path).await.unwrap();
        compiler::compile(&db, &ctx, true).await.unwrap();
        (dir, ctx, db)
    }

    #[tokio::test]
    async fn test_deploy_inserts_and_rollback_deletes() {
        let server = MockServer::start();
        let base = server.base_url();

        let news = server.mock(|w, t| {
            w.method(POST).path("/rest/v1/news");
            t.status(200).json_body(serde_json::json!([{ "id": 1 }]));
        });
        let news_patch = server.mock(|w, t| {
            w.method(PATCH).path("/rest/v1/news");
            t.status(200).json_body(serde_json::json!([{ "id": 1 }]));
        });
        let categories_post = server.mock(|w, t| {
            w.method(POST).path("/rest/v1/categories");
            t.status(200).json_body(serde_json::json!([{ "id": 1 }]));
        });
        let urls = server.mock(|w, t| {
            w.method(POST).path("/rest/v1/urls");
            t.status(200).json_body(serde_json::json!([{ "id": 1 }]));
        });
        let domains = server.mock(|w, t| {
            w.method(POST).path("/rest/v1/domains");
            t.status(200).json_body(serde_json::json!([{ "id": 1 }]));
        });
        let podcasts = server.mock(|w, t| {
            w.method(POST).path("/rest/v1/podcasts");
            t.status(200).json_body(serde_json::json!([{ "id": 1 }]));
        });
        let child_news_get = server.mock(|w, t| {
            w.method(GET)
                .path("/rest/v1/news")
                .query_param("id", "eq.77");
            t.status(200).json_body(serde_json::json!([{ "id": 77 }]));
        });
        let timeline_child = server.mock(|w, t| {
            w.method(POST)
                .path("/rest/v1/timeline_news")
                .body_contains("child_news_id");
            t.status(200).json_body(serde_json::json!([{ "id": 2 }]));
        });
        let timeline_inline = server.mock(|w, t| {
            w.method(POST)
                .path("/rest/v1/timeline_news")
                .body_contains("title")
                .body_contains("sort_order");
            t.status(200).json_body(serde_json::json!([{ "id": 1 }]));
        });
        let storage_post = server.mock(|w, t| {
            w.method(POST).path_contains("/storage/v1/object");
            t.status(200);
        });
        let artists_get = server.mock(|w, t| {
            w.method(GET).path_contains("/rest/v1/artists");
            t.status(200)
                .json_body(serde_json::json!([{ "id": "11111111-1111-1111-1111-111111111111" }]));
        });
        let _get_fallback = server.mock(|w, t| {
            w.method(GET).path_contains("/rest/v1/");
            t.status(200).json_body(serde_json::json!([]));
        });
        let del_timeline = server.mock(|w, t| {
            w.method(DELETE).path("/rest/v1/timeline_news");
            t.status(200).json_body(serde_json::json!([]));
        });
        let del_news = server.mock(|w, t| {
            w.method(DELETE).path("/rest/v1/news");
            t.status(200).json_body(serde_json::json!([]));
        });
        let storage_del = server.mock(|w, t| {
            w.method(DELETE).path_contains("/storage/v1/object");
            t.status(200);
        });
        let _del_fallback = server.mock(|w, t| {
            w.method(DELETE).path_contains("/rest/v1/");
            t.status(200).json_body(serde_json::json!([]));
        });

        let (_dir, ctx, db) = setup(&base).await;
        deploy(&db, &ctx, false)
            .await
            .expect("deploy should succeed");

        assert_eq!(news.hits(), 2, "one news row per podcast");
        assert_eq!(
            child_news_get.hits(),
            1,
            "remote child news id verified before linking"
        );
        assert_eq!(news_patch.hits(), 1, "thumbnail patched after upload");
        assert_eq!(
            categories_post.hits(),
            2,
            "best-effort category insert per podcast (mock always succeeds)"
        );
        assert_eq!(
            urls.hits(),
            3,
            "news sources plus fallback url for Episode Two plus citation event url"
        );
        assert_eq!(domains.hits(), 2, "domain created via best-effort insert");
        assert_eq!(
            podcasts.hits(),
            1,
            "podcast row with storage path + duration"
        );
        assert_eq!(
            timeline_child.hits(),
            1,
            "news id ref linked as timeline row"
        );
        assert_eq!(
            timeline_inline.hits(),
            2,
            "citation events deployed as inline events"
        );
        assert!(storage_post.hits() >= 1, "bundle/asset uploads");
        assert!(artists_get.hits() >= 1, "artist existence checked");
        assert_eq!(
            std::fs::read_dir(ctx.build_dir().join("deployments"))
                .unwrap()
                .count(),
            1,
            "deployment record persisted"
        );

        let deployments_dir = ctx.build_dir().join("deployments");
        let id = std::fs::read_dir(&deployments_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .trim_end_matches(".json")
            .to_string();

        rollback(&ctx, &id).await.expect("rollback should succeed");

        assert_eq!(del_timeline.hits(), 3, "timeline rows deleted");
        assert_eq!(del_news.hits(), 2, "news deleted");
        assert!(storage_del.hits() >= 1, "storage object deleted");
        assert_eq!(
            std::fs::read_dir(&deployments_dir).unwrap().count(),
            0,
            "local deployment record removed"
        );
    }
}
