//! Deploying built projects to Supabase news, podcast, and timeline rows with storage objects (validates before writing; updates lock-recorded rows; descriptions always sent; internal urls fill missing sources).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};
use uuid::Uuid;

use crate::core::CiteError;
use crate::core::auth::{self, Connection};
use crate::core::cache::sha256_bytes;
use crate::core::compiler::{self, BundlePodcast, BundleTimeline, CompileOutcome, ContentBundle};
use crate::core::db::{DbManager, DeployReport};
use crate::core::doctor;
use crate::core::lockfile::Lockfile;
use crate::core::markdown::{plain_text, word_count};
use crate::core::media::mime_type;
use crate::core::metadata::{MAX_SUMMARY_WORDS, TimelineEntry, TimelineItem};
use crate::core::project::ProjectContext;
use crate::core::supabase::{Supabase, encode, row};

const ASSETS_BUCKET: &str = "assets";
const PODCASTS_BUCKET: &str = "podcasts";
const EXISTING_NEWS_COLUMNS: &str = "select=id,thumbnail,podcasts!fk_podcasts_news(podcast_url)";

struct DeployContext {
    api: Supabase,
    root: PathBuf,
    project: String,
    artist_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeploymentRecord {
    deployment_id: String,
    #[serde(default)]
    backend_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    storage_path: String,
    news_ids: Vec<i64>,
    #[serde(default)]
    updated_news_ids: Vec<i64>,
    timeline_ids: Vec<i64>,
    #[serde(default)]
    asset_paths: Vec<String>,
}

struct ExistingNews {
    id: i64,
    thumbnail: Option<String>,
    podcast_url: Option<String>,
}

impl ExistingNews {
    fn from_row(row: &Value) -> Option<Self> {
        let podcast = match &row["podcasts"] {
            Value::Array(rows) => rows.first(),
            other => Some(other),
        };
        Some(Self {
            id: row["id"].as_i64()?,
            thumbnail: row["thumbnail"].as_str().map(str::to_string),
            podcast_url: podcast
                .and_then(|p| p["podcast_url"].as_str())
                .map(str::to_string),
        })
    }
}

#[derive(Debug, Clone)]
struct Category {
    id: i64,
    name: String,
}

fn parse_artist_id(artist_id: &str) -> Result<Uuid, CiteError> {
    match artist_id.trim() {
        "" => Err(CiteError::Config(
            "Set artist_id in [project] in cite.toml to one of the artists 'cite login' lists"
                .to_string(),
        )),
        id => Uuid::parse_str(id).map_err(|_| {
            CiteError::Config(format!("artist_id '{id}' in cite.toml is not a valid UUID"))
        }),
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn ensure_valid(ctx: &ProjectContext) -> Result<(), CiteError> {
    let errors = doctor::validate(ctx).errors;
    if errors.is_empty() {
        return Ok(());
    }
    Err(CiteError::Deploy(format!(
        "Fix {} problem(s) before deploying:\n  - {}",
        errors.len(),
        errors.join("\n  - ")
    )))
}

async fn current_bundle(db: &DbManager, ctx: &ProjectContext) -> Result<ContentBundle, CiteError> {
    let bundle_path = ctx.bundle_path();
    let force = !bundle_path.is_file();
    let outcome = compiler::compile(db, ctx, force).await?;
    if matches!(outcome, CompileOutcome::Complete { .. }) {
        outcome.emit();
    }
    let json = tokio::fs::read_to_string(&bundle_path).await?;
    Ok(serde_json::from_str(&json)?)
}

pub async fn deploy(
    db: &DbManager,
    ctx: &ProjectContext,
    dry_run: bool,
) -> Result<String, CiteError> {
    let artist_id = parse_artist_id(&ctx.manifest.project.artist_id)?;
    ensure_valid(ctx)?;
    let bundle = current_bundle(db, ctx).await?;
    if dry_run {
        return preview(ctx, &bundle);
    }
    let conn = auth::connect(ctx).await?;
    ensure_artist_owned(&conn, artist_id).await?;
    let category_ids = resolve_category_ids(&conn.api, &bundle.podcasts).await?;
    let mut lock = Lockfile::load(&ctx.root)?;
    let dctx = DeployContext {
        api: conn.api,
        root: ctx.root.clone(),
        project: bundle.project.clone(),
        artist_id,
    };

    let mut record = DeploymentRecord {
        deployment_id: Uuid::new_v4().to_string(),
        backend_url: dctx.api.url().to_string(),
        storage_path: String::new(),
        news_ids: Vec::new(),
        updated_news_ids: Vec::new(),
        timeline_ids: Vec::new(),
        asset_paths: Vec::new(),
    };
    info!("Deploying: {}", record.deployment_id);

    let result = deploy_bundle(&dctx, &bundle, &category_ids, &mut lock, &mut record).await;
    lock.save(&ctx.root)?;
    persist_deployment_record(ctx, &record).await?;
    record_deployment(db, ctx, &record, result.is_ok()).await;
    if let Err(e) = result {
        warn!(
            "Deploy failed partway; fix the error and deploy again, or run 'cite rollback {}' to remove what was created",
            record.deployment_id
        );
        return Err(e);
    }

    Ok(format!(
        "Deployed {} podcast(s) ({} created, {} updated), {} timeline row(s)",
        record.news_ids.len() + record.updated_news_ids.len(),
        record.news_ids.len(),
        record.updated_news_ids.len(),
        record.timeline_ids.len()
    ))
}

fn preview(ctx: &ProjectContext, bundle: &ContentBundle) -> Result<String, CiteError> {
    warn!("DRY RUN - nothing will be sent");
    let lock = Lockfile::load(&ctx.root)?;
    let backend = auth::backend_url(ctx).ok();
    let mut updates = 0;
    for pod in &bundle.podcasts {
        let title = &pod.podcast.title;
        let known = backend
            .as_deref()
            .and_then(|url| lock.news_id(url, &pod.podcast.file));
        match known {
            Some(id) => {
                updates += 1;
                info!("Update '{title}' (news #{id})");
            }
            None => info!("Publish '{title}'"),
        }
    }
    Ok(format!(
        "Dry run: {} podcast(s), {updates} update(s), {} new",
        bundle.podcasts.len(),
        bundle.podcasts.len() - updates
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
            news_count: record.news_ids.len() as i64,
            timeline_count: record.timeline_ids.len() as i64,
            asset_count: record.asset_paths.len() as i64,
            success,
        })
        .await;
}

pub async fn rollback(ctx: &ProjectContext, deployment_id: &str) -> Result<String, CiteError> {
    let api = auth::connect(ctx).await?.api;
    let (record_path, record) = load_deployment_record(ctx, deployment_id).await?;
    if !record.backend_url.is_empty() && record.backend_url != api.url() {
        return Err(CiteError::Deploy(format!(
            "Deployment '{deployment_id}' went to {}, but the project now points at {}",
            record.backend_url,
            api.url()
        )));
    }

    warn!("Rolling back deployment: {deployment_id}");

    for news_id in &record.news_ids {
        api.delete("news", *news_id).await?;
        info!("Cleared news {news_id}");
    }

    let storage_paths = record.asset_paths.iter().chain([&record.storage_path]);
    for storage_path in storage_paths.filter(|p| !p.is_empty()) {
        match api.delete_object(storage_path).await {
            Ok(()) => info!("Cleared storage object {storage_path}"),
            Err(e) => warn!("Failed to delete storage object {storage_path}: {e}"),
        }
    }

    let mut lock = Lockfile::load(&ctx.root)?;
    lock.remove_news_ids(api.url(), &record.news_ids);
    lock.save(&ctx.root)?;

    if let Err(e) = tokio::fs::remove_file(&record_path).await {
        warn!("Failed to remove local deployment record: {e}");
    }

    if !record.updated_news_ids.is_empty() {
        warn!(
            "{} news item(s) were updated in place and keep their deployed content; rollback only removes what a deployment created",
            record.updated_news_ids.len()
        );
    }

    Ok("Rollback complete".to_string())
}

async fn ensure_artist_owned(conn: &Connection, artist_id: Uuid) -> Result<(), CiteError> {
    let rows = conn
        .api
        .select("artists", &format!("select=id,user_id&id=eq.{artist_id}"))
        .await?;
    let Some(artist) = rows.first() else {
        return Err(CiteError::Auth(format!(
            "Artist '{artist_id}' does not exist in the database. Create the artist before deploying."
        )));
    };
    if let Some(user_id) = &conn.user_id
        && artist["user_id"].as_str() != Some(user_id.as_str())
    {
        return Err(CiteError::Auth(format!(
            "Artist '{artist_id}' is not owned by the logged-in account. Use an artist_id listed by 'cite login'."
        )));
    }
    Ok(())
}

async fn deploy_bundle(
    dctx: &DeployContext,
    bundle: &ContentBundle,
    category_ids: &[i64],
    lock: &mut Lockfile,
    record: &mut DeploymentRecord,
) -> Result<(), CiteError> {
    let backend = dctx.api.url();
    let mut deployed = Vec::with_capacity(bundle.podcasts.len());
    for (pod, &category_id) in bundle.podcasts.iter().zip(category_ids) {
        let file = pod.podcast.file.as_str();
        let known_id = lock.news_id(backend, file);
        let existing = find_existing_news(dctx, known_id, &pod.podcast.title).await?;
        let (news_id, created) = deploy_news(dctx, pod, category_id, existing, record).await?;
        lock.set_news_id(backend, file, news_id);
        deployed.push((pod, news_id, created));
    }

    let episode_ids: HashMap<&str, i64> = deployed
        .iter()
        .map(|(pod, news_id, _)| (pod.podcast.file.as_str(), *news_id))
        .collect();
    for (pod, news_id, created) in deployed {
        if !created {
            dctx.api
                .delete_where("timeline_news", &format!("parent_news_id=eq.{news_id}"))
                .await?;
        }
        let citation_groups: Vec<&BundleTimeline> = bundle
            .timelines
            .iter()
            .filter(|tl| tl.podcast_id == pod.id)
            .collect();
        let timeline = Timeline {
            parent_news_id: news_id,
            items: &pod.podcast.timeline,
            citation_groups: &citation_groups,
            episode_ids: &episode_ids,
        };
        deploy_timeline(&dctx.api, &timeline, &mut record.timeline_ids).await?;
    }
    Ok(())
}

async fn find_existing_news(
    dctx: &DeployContext,
    known_id: Option<i64>,
    title: &str,
) -> Result<Option<ExistingNews>, CiteError> {
    let select = |filter: String| async move {
        let query = format!(
            "{EXISTING_NEWS_COLUMNS}&artist_id=eq.{}&{filter}",
            dctx.artist_id
        );
        let rows = dctx.api.select("news", &query).await?;
        Ok::<_, CiteError>(rows.first().and_then(ExistingNews::from_row))
    };

    if let Some(id) = known_id {
        let news = select(format!("id=eq.{id}")).await?;
        if news.is_none() {
            warn!("News #{id} in cite.lock for '{title}' no longer exists; publishing it again");
        }
        return Ok(news);
    }

    let by_title = format!("title=eq.{}&order=created_at.desc&limit=1", encode(title));
    let news = select(by_title).await?;
    if let Some(news) = &news {
        info!(
            "Matched '{title}' to existing news #{} by title; recorded in cite.lock",
            news.id
        );
    }
    Ok(news)
}

async fn deploy_news(
    dctx: &DeployContext,
    podcast: &BundlePodcast,
    category_id: i64,
    existing: Option<ExistingNews>,
    record: &mut DeploymentRecord,
) -> Result<(i64, bool), CiteError> {
    let api = &dctx.api;
    let title = &podcast.podcast.title;
    let content = podcast.content.as_deref();

    let fallback_url = format!(
        "cite://{}/{}/{}",
        dctx.artist_id, dctx.project, podcast.podcast.file
    );
    let url_id = ensure_url_id(
        api,
        podcast.podcast.source_url.as_deref(),
        &fallback_url,
        content.map(word_count),
    )
    .await?;
    let summary = podcast
        .podcast
        .summary
        .clone()
        .or_else(|| summarize_content(content));
    let mut fields = row([
        ("title", Value::from(title.as_str())),
        ("summary", summary.map_or(Value::Null, Value::String)),
        ("category_id", Value::from(category_id)),
        ("url_id", Value::from(url_id)),
    ]);

    let created = existing.is_none();
    let (news_id, current_thumbnail, current_audio) = match existing {
        Some(news) => {
            api.update("news", news.id, &fields).await?;
            record.updated_news_ids.push(news.id);
            info!("Updated news item: {title} (id={})", news.id);
            (news.id, news.thumbnail, news.podcast_url)
        }
        None => {
            fields.insert(
                "artist_id".into(),
                Value::String(dctx.artist_id.to_string()),
            );
            fields.insert("published_at".into(), Value::String(now_rfc3339()));
            let id = api.insert("news", &fields).await?;
            record.news_ids.push(id);
            info!("Created news item: {title} (id={id})");
            (id, None, None)
        }
    };

    let thumbnail = persist_thumbnail(dctx, podcast, news_id, current_thumbnail.as_deref()).await?;
    if created {
        record.asset_paths.extend(thumbnail.clone());
    }
    if thumbnail != current_thumbnail {
        let value = thumbnail.clone().map_or(Value::Null, Value::String);
        api.update("news", news_id, &row([("thumbnail", value)]))
            .await?;
        delete_replaced_object(api, current_thumbnail.as_deref(), thumbnail.as_deref()).await;
    }

    let audio = persist_audio(dctx, podcast, news_id, current_audio.as_deref()).await?;
    if created {
        record.asset_paths.extend(audio.clone());
    }
    persist_podcast_row(
        api,
        podcast,
        news_id,
        title,
        audio.as_deref(),
        current_audio.is_some(),
    )
    .await?;
    delete_replaced_object(api, current_audio.as_deref(), audio.as_deref()).await;

    Ok((news_id, created))
}

async fn persist_thumbnail(
    dctx: &DeployContext,
    podcast: &BundlePodcast,
    news_id: i64,
    current: Option<&str>,
) -> Result<Option<String>, CiteError> {
    sync_asset(
        dctx,
        podcast.podcast.thumbnail.as_deref(),
        ASSETS_BUCKET,
        &format!("news_{news_id}"),
        current,
    )
    .await
}

async fn persist_audio(
    dctx: &DeployContext,
    podcast: &BundlePodcast,
    news_id: i64,
    current: Option<&str>,
) -> Result<Option<String>, CiteError> {
    sync_asset(
        dctx,
        podcast.podcast.audio.as_deref(),
        PODCASTS_BUCKET,
        &format!("podcast_{news_id}"),
        current,
    )
    .await
}

async fn persist_podcast_row(
    api: &Supabase,
    podcast: &BundlePodcast,
    news_id: i64,
    title: &str,
    audio: Option<&str>,
    had_audio: bool,
) -> Result<(), CiteError> {
    match audio {
        Some(path) => {
            let duration_minutes = podcast
                .audio_meta
                .as_ref()
                .map(|meta| meta.duration_secs / 60.0);
            upsert_podcast_row(api, news_id, title, path, duration_minutes).await
        }
        None if had_audio => {
            api.delete_where("podcasts", &format!("news_id=eq.{news_id}"))
                .await
        }
        None => Ok(()),
    }
}

struct Timeline<'a> {
    parent_news_id: i64,
    items: &'a [TimelineItem],
    citation_groups: &'a [&'a BundleTimeline],
    episode_ids: &'a HashMap<&'a str, i64>,
}

async fn deploy_timeline(
    api: &Supabase,
    timeline: &Timeline<'_>,
    timeline_ids: &mut Vec<i64>,
) -> Result<(), CiteError> {
    let parent = timeline.parent_news_id;
    let mut sort_order = 0_i64;

    for item in timeline.items {
        let child_id = match item {
            TimelineItem::Citation(path) => {
                let group = timeline.citation_groups.iter().find(|g| g.source == *path);
                for entry in group.iter().flat_map(|g| &g.entries) {
                    if let Some(id) = deploy_event(api, parent, entry, sort_order + 1).await? {
                        sort_order += 1;
                        timeline_ids.push(id);
                    }
                }
                continue;
            }
            TimelineItem::Event(entry) => {
                if let Some(id) = deploy_event(api, parent, entry, sort_order + 1).await? {
                    sort_order += 1;
                    timeline_ids.push(id);
                }
                continue;
            }
            TimelineItem::Episode(file) => {
                *timeline.episode_ids.get(file.as_str()).ok_or_else(|| {
                    CiteError::Deploy(format!(
                        "Timeline entry '{file}' is not an episode of this project"
                    ))
                })?
            }
            TimelineItem::News(id) => {
                if api.find_id("news", "id", &id.to_string()).await?.is_none() {
                    return Err(CiteError::Deploy(format!(
                        "Timeline news item {id} does not exist in the database"
                    )));
                }
                *id
            }
        };
        if child_id == parent {
            return Err(CiteError::Deploy(format!(
                "News {parent} cannot list itself in its own timeline"
            )));
        }

        sort_order += 1;
        let payload = row([
            ("parent_news_id", Value::from(parent)),
            ("child_news_id", Value::from(child_id)),
            ("sort_order", Value::from(sort_order)),
        ]);
        timeline_ids.push(api.insert("timeline_news", &payload).await?);
        info!("Linked news {parent} -> {child_id}");
    }

    Ok(())
}

async fn deploy_event(
    api: &Supabase,
    parent_news_id: i64,
    entry: &TimelineEntry,
    sort_order: i64,
) -> Result<Option<i64>, CiteError> {
    let title = entry.title.trim();
    if title.is_empty() {
        warn!("Skipping timeline entry without a title");
        return Ok(None);
    }

    let description = entry.description.as_deref().map(str::trim);
    let mut payload = row([
        ("parent_news_id", Value::from(parent_news_id)),
        ("title", Value::from(title)),
        ("description", Value::from(description.unwrap_or_default())),
        ("sort_order", Value::from(sort_order)),
    ]);

    if let Some(url) = entry.url.as_deref() {
        let url_id = ensure_url_id(api, Some(url), url, None).await?;
        payload.insert("url_id".into(), Value::from(url_id));
    }

    match entry.event_date() {
        Some(event_date) => {
            payload.insert("event_date".into(), Value::String(event_date));
        }
        None => {
            if let Some(date) = entry.date.as_deref().filter(|d| !d.trim().is_empty()) {
                warn!("Unrecognized timeline date '{date}', deploying without event_date");
            }
        }
    }

    let id = api.insert("timeline_news", &payload).await?;
    info!("Deployed timeline event: {title}");
    Ok(Some(id))
}

fn deployments_dir(ctx: &ProjectContext) -> PathBuf {
    ctx.root.join(".cite").join("deployments")
}

async fn persist_deployment_record(
    ctx: &ProjectContext,
    record: &DeploymentRecord,
) -> Result<(), CiteError> {
    let dir = deployments_dir(ctx);
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{}.json", record.deployment_id));
    tokio::fs::write(path, serde_json::to_vec_pretty(record)?).await?;
    Ok(())
}

async fn load_deployment_record(
    ctx: &ProjectContext,
    deployment_id: &str,
) -> Result<(PathBuf, DeploymentRecord), CiteError> {
    let file = format!("{deployment_id}.json");
    let candidates = [
        deployments_dir(ctx).join(&file),
        ctx.build_dir().join("deployments").join(&file),
    ];
    for path in candidates {
        if let Ok(json) = tokio::fs::read_to_string(&path).await {
            let record: DeploymentRecord = serde_json::from_str(&json)?;
            return Ok((path, record));
        }
    }
    Err(CiteError::Deploy(format!(
        "No local deployment record found for '{deployment_id}'. Run deploy first."
    )))
}

async fn fetch_categories(api: &Supabase) -> Result<Vec<Category>, CiteError> {
    let rows = api.select("categories", "select=id,name").await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some(Category {
                id: row["id"].as_i64()?,
                name: row["name"].as_str()?.to_string(),
            })
        })
        .collect())
}

async fn resolve_category_ids(
    api: &Supabase,
    podcasts: &[BundlePodcast],
) -> Result<Vec<i64>, CiteError> {
    let mut categories = fetch_categories(api).await?;
    let mut ids = Vec::with_capacity(podcasts.len());
    for pod in podcasts {
        let Some(name) = pod.podcast.category.as_deref() else {
            return Err(CiteError::Deploy(format!(
                "Podcast '{}' has no category. Set 'category' in metadata (available: {})",
                pod.podcast.title,
                category_names(&categories)
            )));
        };
        ids.push(resolve_category_id(api, name, &mut categories).await?);
    }
    Ok(ids)
}

async fn resolve_category_id(
    api: &Supabase,
    name: &str,
    categories: &mut Vec<Category>,
) -> Result<i64, CiteError> {
    if let Some(category) = categories
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(name))
    {
        return Ok(category.id);
    }

    let payload = row([
        ("name", Value::from(name)),
        ("description", Value::from("Created automatically by cite")),
    ]);
    let id = api.insert("categories", &payload).await.map_err(|_| {
        CiteError::Deploy(format!(
            "Unknown category '{name}' (available: {}). Creating categories requires the service role key.",
            category_names(categories)
        ))
    })?;
    info!("Created category '{name}' (id={id})");
    categories.push(Category {
        id,
        name: name.to_string(),
    });
    Ok(id)
}

fn category_names(categories: &[Category]) -> String {
    categories
        .iter()
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

async fn ensure_domain_id(api: &Supabase, domain_name: &str) -> Option<i64> {
    if let Ok(Some(id)) = api.find_id("domains", "domain_name", domain_name).await {
        return Some(id);
    }
    let payload = row([
        ("domain_name", Value::from(domain_name)),
        ("is_trusted", Value::Bool(false)),
    ]);
    api.insert("domains", &payload).await.ok()
}

async fn ensure_url_id(
    api: &Supabase,
    source_url: Option<&str>,
    fallback_url: &str,
    word_count: Option<i64>,
) -> Result<i64, CiteError> {
    let source_url = source_url.filter(|value| !value.trim().is_empty());
    let url_value = source_url.unwrap_or(fallback_url);
    if let Some(id) = api.find_id("urls", "url", url_value).await? {
        return Ok(id);
    }

    let mut payload = row([
        ("url", Value::from(url_value)),
        ("accessed_at", Value::String(now_rfc3339())),
    ]);
    if let Some(count) = word_count {
        payload.insert("word_count".into(), Value::from(count));
    }
    if let Some(domain_name) = source_url.and_then(extract_domain_name)
        && let Some(domain_id) = ensure_domain_id(api, &domain_name).await
    {
        payload.insert("domain_id".into(), Value::from(domain_id));
    }

    api.insert("urls", &payload).await
}

async fn sync_asset(
    dctx: &DeployContext,
    asset: Option<&str>,
    bucket: &str,
    stem: &str,
    current: Option<&str>,
) -> Result<Option<String>, CiteError> {
    let Some(asset) = asset.map(str::trim).filter(|a| !a.is_empty()) else {
        return Ok(None);
    };

    let local_path = dctx.root.join(asset);
    let bytes = tokio::fs::read(&local_path).await?;
    let ext = file_extension(&local_path);
    let hash = sha256_bytes(&bytes);
    let object_path = format!("{}/{stem}-{}.{ext}", dctx.artist_id, &hash[..12]);
    let storage_path = format!("{bucket}/{object_path}");
    if current == Some(storage_path.as_str()) {
        return Ok(Some(storage_path));
    }
    dctx.api
        .upload(bucket, &object_path, &bytes, mime_type(ext))
        .await
        .map(Some)
}

async fn delete_replaced_object(api: &Supabase, old: Option<&str>, new: Option<&str>) {
    let Some(old) = old.filter(|old| Some(*old) != new) else {
        return;
    };
    match api.delete_object(old).await {
        Ok(()) => info!("Removed replaced object {old}"),
        Err(e) => warn!("Could not remove replaced object {old}: {e}"),
    }
}

fn file_extension(path: &Path) -> &str {
    path.extension()
        .and_then(|value| value.to_str())
        .unwrap_or("bin")
}

async fn upsert_podcast_row(
    api: &Supabase,
    news_id: i64,
    title: &str,
    podcast_url: &str,
    duration_minutes: Option<f64>,
) -> Result<(), CiteError> {
    let duration = duration_minutes
        .filter(|d| *d > 0.0)
        .and_then(|d| serde_json::Number::from_f64((d * 100.0).round() / 100.0))
        .map_or(Value::Null, Value::Number);
    let payload = row([
        ("news_id", Value::from(news_id)),
        ("title", Value::from(title)),
        ("podcast_url", Value::from(podcast_url)),
        ("duration_minutes", duration),
    ]);
    api.upsert("podcasts", &payload, "news_id").await
}

fn extract_domain_name(source_url: &str) -> Option<String> {
    let without_scheme = source_url
        .split_once("//")
        .map_or(source_url, |(_, rest)| rest);
    let authority = without_scheme.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?.trim();
    let host = host.to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    (!host.is_empty()).then(|| host.to_string())
}

fn summarize_content(content: Option<&str>) -> Option<String> {
    let text = plain_text(content?);
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }

    let summary = words[..words.len().min(MAX_SUMMARY_WORDS)].join(" ");
    if words.len() > MAX_SUMMARY_WORDS {
        Some(format!("{summary}..."))
    } else {
        Some(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            extract_domain_name("https://WWW.Reuters.com:443/tech?id=1"),
            Some("reuters.com".to_string())
        );
        assert_eq!(
            extract_domain_name("https://user@news.example.org#top"),
            Some("news.example.org".to_string())
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
        assert_eq!(
            summarize_content(Some(
                "---\nk: v\n---\n# Title\n\nShort  **episode**\nintro."
            ))
            .as_deref(),
            Some("Short episode intro.")
        );
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
                url: Some(staging_url.into()),
                api_key: Some("key".into()),
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
    summary: Hand-written summary.
    category: Tech
    timeline:
      - content/episode.md
      - title: Written inline
        date: 2025-05-22
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

        let news_one = server.mock(|w, t| {
            w.method(POST)
                .path("/rest/v1/news")
                .body_contains("Episode One");
            t.status(200).json_body(serde_json::json!([{ "id": 1 }]));
        });
        let news_two = server.mock(|w, t| {
            w.method(POST)
                .path("/rest/v1/news")
                .body_contains("Episode Two")
                .body_contains("Hand-written summary.");
            t.status(200).json_body(serde_json::json!([{ "id": 2 }]));
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

        assert_eq!(
            news_one.hits() + news_two.hits(),
            2,
            "one news row per podcast"
        );
        assert_eq!(
            child_news_get.hits(),
            1,
            "remote child news id verified before linking"
        );
        assert_eq!(news_patch.hits(), 1, "thumbnail patched after upload");
        assert_eq!(
            categories_post.hits(),
            1,
            "unknown category created once and reused case-insensitively"
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
            2,
            "news id and episode file linked as timeline rows"
        );
        assert_eq!(
            timeline_inline.hits(),
            3,
            "citation events and the inline event deployed as events"
        );
        assert!(storage_post.hits() >= 1, "asset uploads");
        assert!(artists_get.hits() >= 1, "artist existence checked");
        let records_dir = deployments_dir(&ctx);
        assert_eq!(
            std::fs::read_dir(&records_dir).unwrap().count(),
            1,
            "deployment record persisted"
        );
        let lock = Lockfile::load(&ctx.root).unwrap();
        assert_eq!(lock.news_id(&base, "content/episode.md"), Some(1));
        assert_eq!(lock.news_id(&base, "content/episode-two.md"), Some(2));

        let id = std::fs::read_dir(&records_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .trim_end_matches(".json")
            .to_string();

        rollback(&ctx, &id).await.expect("rollback should succeed");

        assert_eq!(
            del_timeline.hits(),
            0,
            "timeline rows cascade with their news row"
        );
        assert_eq!(del_news.hits(), 2, "news deleted");
        assert!(storage_del.hits() >= 1, "storage object deleted");
        assert_eq!(
            std::fs::read_dir(&records_dir).unwrap().count(),
            0,
            "local deployment record removed"
        );
        let lock = Lockfile::load(&ctx.root).unwrap();
        assert_eq!(
            lock.news_id(&base, "content/episode.md"),
            None,
            "rolled-back news forgotten by cite.lock"
        );
    }

    fn mock_lookups(server: &MockServer) {
        server.mock(|w, t| {
            w.method(GET).path("/rest/v1/categories");
            t.status(200)
                .json_body(serde_json::json!([{ "id": 5, "name": "tech" }]));
        });
        server.mock(|w, t| {
            w.method(GET).path_contains("/rest/v1/artists");
            t.status(200)
                .json_body(serde_json::json!([{ "id": "11111111-1111-1111-1111-111111111111" }]));
        });
        server.mock(|w, t| {
            w.method(GET).path_contains("/rest/v1/");
            t.status(200).json_body(serde_json::json!([]));
        });
    }

    fn only_deployment_record(ctx: &ProjectContext) -> DeploymentRecord {
        let dir = deployments_dir(ctx);
        let mut entries = std::fs::read_dir(dir).unwrap();
        let path = entries.next().unwrap().unwrap().path();
        assert!(entries.next().is_none(), "exactly one deployment record");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn deployment_records(ctx: &ProjectContext) -> Vec<DeploymentRecord> {
        std::fs::read_dir(deployments_dir(ctx))
            .unwrap()
            .map(|entry| {
                let json = std::fs::read_to_string(entry.unwrap().path()).unwrap();
                serde_json::from_str(&json).unwrap()
            })
            .collect()
    }

    #[tokio::test]
    async fn test_redeploy_updates_existing_news() {
        let server = MockServer::start();
        let existing =
            |id: i64| serde_json::json!([{ "id": id, "thumbnail": null, "podcasts": null }]);
        for (param, value, body) in [
            ("id", "eq.1", existing(1)),
            ("id", "eq.2", existing(2)),
            ("id", "eq.77", serde_json::json!([{ "id": 77 }])),
            ("title", "eq.Episode One", existing(1)),
        ] {
            server.mock(|w, t| {
                w.method(GET)
                    .path("/rest/v1/news")
                    .query_param(param, value);
                t.status(200).json_body(body);
            });
        }
        mock_lookups(&server);
        let news_post = server.mock(|w, t| {
            w.method(POST).path("/rest/v1/news");
            t.status(200).json_body(serde_json::json!([{ "id": 2 }]));
        });
        let republished = server.mock(|w, t| {
            w.method(PATCH)
                .path("/rest/v1/news")
                .body_contains("published_at");
            t.status(200);
        });
        let news_patch = server.mock(|w, t| {
            w.method(PATCH).path("/rest/v1/news");
            t.status(200);
        });
        let timeline_reset = server.mock(|w, t| {
            w.method(DELETE)
                .path("/rest/v1/timeline_news")
                .query_param_exists("parent_news_id");
            t.status(200);
        });
        server.mock(|w, t| {
            w.method(POST);
            t.status(200).json_body(serde_json::json!([{ "id": 9 }]));
        });
        server.mock(|w, t| {
            w.method(DELETE);
            t.status(200);
        });

        let (_dir, ctx, db) = setup(&server.base_url()).await;
        deploy(&db, &ctx, false).await.expect("first deploy");
        deploy(&db, &ctx, false).await.expect("second deploy");

        assert_eq!(news_post.hits(), 1, "only Episode Two was ever inserted");
        assert!(news_patch.hits() >= 3, "news rows updated in place");
        assert_eq!(republished.hits(), 0, "published_at kept on update");
        assert_eq!(timeline_reset.hits(), 3, "timeline rebuilt per update");

        let mut updated: Vec<Vec<i64>> = deployment_records(&ctx)
            .into_iter()
            .map(|r| {
                assert!(r.news_ids.len() <= 1);
                r.updated_news_ids
            })
            .collect();
        updated.sort();
        assert_eq!(updated, vec![vec![1], vec![1, 2]]);

        let lock = Lockfile::load(&ctx.root).unwrap();
        let base = server.base_url();
        assert_eq!(
            lock.news_id(&base, "content/episode.md"),
            Some(1),
            "adopted by title"
        );
        assert_eq!(lock.news_id(&base, "content/episode-two.md"), Some(2));
    }

    #[tokio::test]
    async fn test_lockfile_entry_wins_over_title_match() {
        let server = MockServer::start();
        server.mock(|w, t| {
            w.method(GET)
                .path("/rest/v1/news")
                .query_param("id", "eq.77");
            t.status(200).json_body(serde_json::json!([{ "id": 77 }]));
        });
        let title_lookup = server.mock(|w, t| {
            w.method(GET)
                .path("/rest/v1/news")
                .query_param_exists("title");
            t.status(200).json_body(serde_json::json!([{ "id": 1 }]));
        });
        mock_lookups(&server);
        let news_post = server.mock(|w, t| {
            w.method(POST)
                .path("/rest/v1/news")
                .body_contains("Episode One");
            t.status(200).json_body(serde_json::json!([{ "id": 40 }]));
        });
        let news_post_two = server.mock(|w, t| {
            w.method(POST)
                .path("/rest/v1/news")
                .body_contains("Episode Two");
            t.status(200).json_body(serde_json::json!([{ "id": 41 }]));
        });
        server.mock(|w, t| {
            w.method(POST);
            t.status(200).json_body(serde_json::json!([{ "id": 9 }]));
        });
        server.mock(|w, t| {
            w.method(PATCH);
            t.status(200);
        });

        let (_dir, ctx, db) = setup(&server.base_url()).await;
        let mut lock = Lockfile::default();
        for file in ["content/episode.md", "content/episode-two.md"] {
            lock.set_news_id(&server.base_url(), file, 5);
        }
        lock.save(&ctx.root).unwrap();

        deploy(&db, &ctx, false).await.expect("deploy");
        assert_eq!(news_post.hits() + news_post_two.hits(), 2);
        assert_eq!(title_lookup.hits(), 0, "cite.lock is authoritative");

        let lock = Lockfile::load(&ctx.root).unwrap();
        assert_eq!(
            lock.news_id(&server.base_url(), "content/episode.md"),
            Some(40)
        );
        assert_eq!(
            lock.news_id(&server.base_url(), "content/episode-two.md"),
            Some(41)
        );
    }

    #[tokio::test]
    async fn test_failed_deploy_records_created_rows() {
        let server = MockServer::start();
        mock_lookups(&server);
        for table in ["urls", "domains", "news"] {
            server.mock(|w, t| {
                w.method(POST).path(format!("/rest/v1/{table}"));
                t.status(200).json_body(serde_json::json!([{ "id": 1 }]));
            });
        }
        server.mock(|w, t| {
            w.method(PATCH).path("/rest/v1/news");
            t.status(200).json_body(serde_json::json!([]));
        });
        server.mock(|w, t| {
            w.method(POST).path_contains("/storage/v1/object");
            t.status(200);
        });
        server.mock(|w, t| {
            w.method(POST).path("/rest/v1/podcasts");
            t.status(500).body("boom");
        });

        let (_dir, ctx, db) = setup(&server.base_url()).await;
        assert!(deploy(&db, &ctx, false).await.is_err());

        let record = only_deployment_record(&ctx);
        assert_eq!(record.news_ids, vec![1], "news row kept for rollback");
        assert_eq!(record.asset_paths.len(), 2, "thumbnail and audio kept");
    }

    #[tokio::test]
    async fn test_missing_category_fails_before_any_insert() {
        let server = MockServer::start();
        mock_lookups(&server);
        let inserts = server.mock(|w, t| {
            w.method(POST);
            t.status(200).json_body(serde_json::json!([{ "id": 1 }]));
        });

        let (_dir, ctx, db) = setup(&server.base_url()).await;
        let bundle_path = ctx.bundle_path();
        let mut bundle: Value =
            serde_json::from_str(&std::fs::read_to_string(&bundle_path).unwrap()).unwrap();
        bundle["podcasts"][1]
            .as_object_mut()
            .unwrap()
            .remove("category");
        std::fs::write(&bundle_path, bundle.to_string()).unwrap();

        let err = deploy(&db, &ctx, false).await.unwrap_err();
        assert!(err.to_string().contains("has no category"), "{err}");
        assert!(err.to_string().contains("tech"), "lists available: {err}");
        assert_eq!(inserts.hits(), 0, "nothing written");
    }
    #[tokio::test]
    async fn test_dry_run_previews_from_lockfile_without_requests() {
        let server = MockServer::start();
        let any = server.mock(|w, t| {
            w.path_contains("/");
            t.status(500);
        });
        let (_dir, ctx, db) = setup(&server.base_url()).await;
        let mut lock = Lockfile::default();
        lock.set_news_id(&server.base_url(), "content/episode.md", 12);
        lock.save(&ctx.root).unwrap();

        let msg = deploy(&db, &ctx, true).await.expect("dry run");
        assert_eq!(msg, "Dry run: 2 podcast(s), 1 update(s), 1 new");
        assert_eq!(any.hits(), 0, "dry run never contacts Supabase");
        assert!(!deployments_dir(&ctx).exists(), "nothing recorded");
    }

    #[tokio::test]
    async fn test_invalid_metadata_blocks_deploy_before_any_request() {
        let server = MockServer::start();
        let any = server.mock(|w, t| {
            w.path_contains("/");
            t.status(500);
        });
        let (dir, _, db) = setup(&server.base_url()).await;
        let metadata = std::fs::read_to_string(dir.path().join("metadata.yml")).unwrap();
        let long = "word ".repeat(MAX_SUMMARY_WORDS + 1);
        std::fs::write(
            dir.path().join("metadata.yml"),
            metadata.replace("Hand-written summary.", &long),
        )
        .unwrap();
        let ctx = ProjectContext::load(dir.path()).unwrap();

        let err = deploy(&db, &ctx, false).await.unwrap_err().to_string();
        assert!(err.contains("summary has 51 words"), "{err}");
        assert_eq!(any.hits(), 0, "nothing sent");
    }

    #[tokio::test]
    async fn test_deploy_rebuilds_changed_sources() {
        let server = MockServer::start();
        let (dir, ctx, db) = setup(&server.base_url()).await;
        std::fs::write(
            dir.path().join("content/episode-two.md"),
            "# Two\nRewritten.",
        )
        .unwrap();

        deploy(&db, &ctx, true).await.expect("dry run");
        let bundle = std::fs::read_to_string(ctx.bundle_path()).unwrap();
        assert!(bundle.contains("Rewritten."), "stale build not deployed");
    }

    #[tokio::test]
    async fn test_rollback_refuses_another_supabase_project() {
        let server = MockServer::start();
        let deletes = server.mock(|w, t| {
            w.method(DELETE);
            t.status(200);
        });
        let (_dir, ctx, _db) = setup(&server.base_url()).await;
        let record = DeploymentRecord {
            deployment_id: "d1".into(),
            backend_url: "https://other.supabase.co".into(),
            storage_path: String::new(),
            news_ids: vec![1],
            updated_news_ids: Vec::new(),
            timeline_ids: Vec::new(),
            asset_paths: vec!["assets/x.png".into()],
        };
        persist_deployment_record(&ctx, &record).await.unwrap();

        let err = rollback(&ctx, "d1").await.unwrap_err().to_string();
        assert!(err.contains("went to https://other.supabase.co"), "{err}");
        assert_eq!(deletes.hits(), 0, "nothing deleted");
        assert!(rollback(&ctx, "unknown").await.is_err());
    }

    #[tokio::test]
    async fn test_artist_must_belong_to_logged_in_user() {
        let server = MockServer::start();
        server.mock(|w, t| {
            w.method(GET).path("/rest/v1/artists");
            t.status(200).json_body(serde_json::json!([
                { "id": "11111111-1111-1111-1111-111111111111", "user_id": "owner" }
            ]));
        });
        let artist_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let conn = |user_id: Option<&str>| Connection {
            api: Supabase::new(reqwest::Client::new(), &server.base_url(), "k", "t"),
            user_id: user_id.map(str::to_string),
        };

        assert!(
            ensure_artist_owned(&conn(Some("owner")), artist_id)
                .await
                .is_ok()
        );
        assert!(
            ensure_artist_owned(&conn(None), artist_id).await.is_ok(),
            "service key"
        );
        let err = ensure_artist_owned(&conn(Some("someone-else")), artist_id)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not owned"), "{err}");
    }
}
