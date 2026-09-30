//! Local database at `~/.cite/cite.db`: last-build snapshots, build and deploy history, and caches (legacy `link` column kept as a fallback).
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use libsql::{Builder, Connection, Value, params};
use tracing::info;

use crate::core::CiteError;
use crate::core::cache::BuildCache;
use crate::core::compiler::ContentBundle;
use crate::core::markdown::word_count;
use crate::core::metadata::{Podcast, TimelineEntry};
use crate::core::project::ProjectContext;

pub fn global_db_path() -> PathBuf {
    std::env::var_os("CITE_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::core::cite_home().join("cite.db"))
}

pub struct DbManager {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct BuildRecord {
    pub project_id: String,
    pub compiler_version: f64,
    pub podcast_count: i64,
    pub timeline_count: i64,
    pub total_words: i64,
    pub duration_ms: i64,
    pub was_incremental: bool,
}

#[derive(Debug, Clone)]
pub struct DeployReport {
    pub project_id: String,
    pub deployment_id: String,
    pub news_count: i64,
    pub timeline_count: i64,
    pub asset_count: i64,
    pub success: bool,
}

#[derive(Debug, Clone)]
pub struct StoredPodcast {
    pub title: String,
    pub word_count: i64,
    pub category: String,
    pub file: String,
    pub has_audio: bool,
    pub has_thumbnail: bool,
}

#[derive(Debug, Clone)]
pub struct StoredTimeline {
    pub date: Option<String>,
    pub title: String,
    pub url: Option<String>,
    pub entry_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StoredDeployment {
    pub deployment_id: String,
    pub deployed_at: String,
    pub success: bool,
    pub news_count: i64,
    pub asset_count: i64,
}

#[derive(Debug, Clone)]
pub struct StoredBuild {
    pub podcast_count: i64,
    pub total_words: i64,
    pub duration_ms: i64,
    pub was_incremental: bool,
    pub built_at: String,
}

#[derive(Debug, Clone)]
pub struct ProjectStats {
    pub podcast_count: i64,
    pub timeline_count: i64,
    pub total_words: i64,
    pub build_count: i64,
    pub last_built: Option<String>,
    pub deployment_count: i64,
    pub last_deployed: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AllStats {
    pub project_count: i64,
    pub total_podcasts: i64,
    pub total_timelines: i64,
    pub total_words: i64,
    pub total_builds: i64,
}

#[derive(Debug, Clone)]
pub struct RestoredPodcast {
    pub id: String,
    pub title: String,
    pub file: String,
    pub source_url: Option<String>,
    pub category: Option<String>,
    pub thumbnail: Option<String>,
    pub audio: Option<String>,
    pub citation_file: Option<String>,
    pub metadata: Option<Podcast>,
    pub content: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RestoredTimeline {
    pub podcast_id: String,
    pub from_citation: bool,
    pub entry: TimelineEntry,
}

#[derive(Debug, Clone)]
pub struct RestoredProject {
    pub name: String,
    pub language: String,
    pub artist_id: String,
    pub metadata_file: String,
    pub podcasts: Vec<RestoredPodcast>,
    pub timelines: Vec<RestoredTimeline>,
}

fn get_opt_string(row: &libsql::Row, idx: i32) -> String {
    match row.get_value(idx) {
        Ok(Value::Text(s)) => s,
        _ => String::new(),
    }
}

fn opt_string(row: &libsql::Row, idx: i32) -> Option<String> {
    let s = get_opt_string(row, idx);
    if s.is_empty() { None } else { Some(s) }
}

fn entry_params(
    id: &str,
    project_id: &str,
    podcast_id: &str,
    entry: &TimelineEntry,
    entry_type: &str,
) -> [libsql::Result<Value>; 8] {
    params![
        id,
        project_id,
        podcast_id,
        entry.date.as_deref(),
        entry.title.as_str(),
        entry.description.as_deref(),
        entry.url.as_deref(),
        entry_type,
    ]
}

impl DbManager {
    pub async fn open() -> Result<Self, CiteError> {
        let path = global_db_path();
        Self::open_path(&path).await
    }

    pub async fn open_path(path: &Path) -> Result<Self, CiteError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Builder::new_local(path).build().await?;
        let conn = db.connect()?;
        let mgr = Self { conn };
        mgr.run_migrations().await?;
        Ok(mgr)
    }

    async fn run_migrations(&self) -> Result<(), CiteError> {
        let batch = "
            CREATE TABLE IF NOT EXISTS _schema_version (
                version INTEGER PRIMARY KEY,
                applied_at TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', CURRENT_TIMESTAMP))
            );
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                root_path TEXT NOT NULL DEFAULT '',
                language TEXT DEFAULT 'en',
                artist_id TEXT DEFAULT '',
                metadata_file TEXT DEFAULT 'metadata.yml',
                last_synced TEXT
            );
            CREATE TABLE IF NOT EXISTS podcasts (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                title TEXT NOT NULL DEFAULT '',
                file TEXT NOT NULL DEFAULT '',
                source_url TEXT,
                category TEXT,
                thumbnail TEXT,
                audio TEXT,
                citation_file TEXT,
                content TEXT,
                word_count INTEGER DEFAULT 0,
                built_at TEXT
            );
            CREATE TABLE IF NOT EXISTS timeline_entries (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                podcast_id TEXT NOT NULL,
                date TEXT,
                title TEXT NOT NULL DEFAULT '',
                summary TEXT,
                url TEXT,
                entry_type TEXT
            );
            CREATE TABLE IF NOT EXISTS build_history (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                compiler_version REAL NOT NULL,
                built_at TEXT NOT NULL,
                podcast_count INTEGER DEFAULT 0,
                timeline_count INTEGER DEFAULT 0,
                total_words INTEGER DEFAULT 0,
                duration_ms INTEGER DEFAULT 0,
                was_incremental INTEGER DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS deployment_history (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                deployment_id TEXT NOT NULL,
                deployed_at TEXT NOT NULL,
                storage_path TEXT DEFAULT '',
                news_count INTEGER DEFAULT 0,
                timeline_count INTEGER DEFAULT 0,
                asset_count INTEGER DEFAULT 0,
                success INTEGER DEFAULT 1,
                dry_run INTEGER DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS file_cache (
                file_path TEXT NOT NULL,
                project_id TEXT NOT NULL,
                sha256 TEXT NOT NULL DEFAULT '',
                last_modified TEXT,
                PRIMARY KEY (file_path, project_id)
            );
            CREATE INDEX IF NOT EXISTS idx_podcasts_project ON podcasts (project_id);
            CREATE INDEX IF NOT EXISTS idx_timeline_entries_project ON timeline_entries (project_id);
            CREATE INDEX IF NOT EXISTS idx_build_history_project ON build_history (project_id, built_at);
            CREATE INDEX IF NOT EXISTS idx_deployment_history_project ON deployment_history (project_id, deployed_at);
        ";

        for stmt in batch.split(';') {
            let trimmed = stmt.trim();
            if !trimmed.is_empty() {
                self.conn.execute(trimmed, ()).await?;
            }
        }

        self.add_column_if_missing("timeline_entries", "link")
            .await?;
        self.add_column_if_missing("podcasts", "metadata").await?;

        let mut rows = self
            .conn
            .query("SELECT COUNT(*) FROM _schema_version", ())
            .await?;
        let has_version = match rows.next().await? {
            Some(row) => row.get::<i64>(0)? > 0,
            None => false,
        };

        if !has_version {
            self.conn
                .execute("INSERT INTO _schema_version (version) VALUES (1)", ())
                .await?;
        }

        Ok(())
    }

    async fn add_column_if_missing(&self, table: &str, column: &str) -> Result<(), CiteError> {
        let probe = format!("SELECT {column} FROM {table} LIMIT 1");
        if self.conn.query(&probe, ()).await.is_err() {
            let alter = format!("ALTER TABLE {table} ADD COLUMN {column} TEXT");
            self.conn.execute(&alter, ()).await?;
        }
        Ok(())
    }

    pub async fn sync_project(
        &self,
        ctx: &ProjectContext,
        bundle: &ContentBundle,
    ) -> Result<(), CiteError> {
        let project_id = ctx.project_id();
        let project = &ctx.manifest.project;

        let tx = self.conn.transaction().await?;
        tx.execute(
            "INSERT INTO projects (id, name, root_path, language, artist_id, metadata_file, last_synced)
             VALUES (?1, ?2, ?1, ?3, ?4, ?5, strftime('%Y-%m-%dT%H:%M:%fZ', CURRENT_TIMESTAMP))
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                root_path = excluded.root_path,
                language = excluded.language,
                artist_id = excluded.artist_id,
                metadata_file = excluded.metadata_file,
                last_synced = excluded.last_synced",
            params![
                project_id.as_str(),
                project.name.as_str(),
                project.language.as_str(),
                project.artist_id.as_str(),
                project.metadata_file.as_str(),
            ],
        )
        .await?;
        tx.execute(
            "DELETE FROM podcasts WHERE project_id = ?1",
            params![project_id.as_str()],
        )
        .await?;
        tx.execute(
            "DELETE FROM timeline_entries WHERE project_id = ?1",
            params![project_id.as_str()],
        )
        .await?;

        let insert_entry = "INSERT INTO timeline_entries
                (id, project_id, podcast_id, date, title, summary, url, entry_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";
        for pod in &bundle.podcasts {
            let meta = &pod.podcast;
            let metadata = serde_json::to_string(meta)?;
            tx.execute(
                "INSERT INTO podcasts (id, project_id, title, file, source_url, category,
                        thumbnail, audio, citation_file, content, word_count, metadata)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    pod.id.as_str(),
                    project_id.as_str(),
                    meta.title.as_str(),
                    meta.file.as_str(),
                    meta.source_url.as_deref(),
                    meta.category.as_deref(),
                    meta.thumbnail.as_deref(),
                    meta.audio.as_deref(),
                    meta.citation(),
                    pod.content.as_deref(),
                    pod.content.as_deref().map_or(0, word_count),
                    metadata,
                ],
            )
            .await?;

            for (i, entry) in meta.inline_events().enumerate() {
                let id = format!("{}-event-{i}", pod.id);
                tx.execute(
                    insert_entry,
                    entry_params(&id, &project_id, &pod.id, entry, "event"),
                )
                .await?;
            }
        }

        for timeline in &bundle.timelines {
            for entry in &timeline.entries {
                tx.execute(
                    insert_entry,
                    entry_params(
                        &entry.id,
                        &project_id,
                        &timeline.podcast_id,
                        entry,
                        "citation",
                    ),
                )
                .await?;
            }
        }
        tx.commit().await?;

        info!(
            "Synced {} podcast(s) and {} timeline group(s) for '{}'",
            bundle.podcasts.len(),
            bundle.timelines.len(),
            project.name
        );
        Ok(())
    }

    pub async fn load_cache(&self, project_id: &str) -> Result<Option<BuildCache>, CiteError> {
        let mut rows = self
            .conn
            .query(
                "SELECT file_path, sha256 FROM file_cache WHERE project_id = ?1",
                params![project_id],
            )
            .await?;

        let mut hashes = HashMap::new();
        while let Some(row) = rows.next().await? {
            let path: String = row.get(0)?;
            let hash: String = row.get(1)?;
            hashes.insert(path, hash);
        }

        let cv: f64 = self
            .conn
            .query(
                "SELECT MAX(compiler_version) FROM build_history WHERE project_id = ?1",
                params![project_id],
            )
            .await?
            .next()
            .await?
            .map(|row| row.get::<f64>(0).unwrap_or(0.0))
            .unwrap_or(0.0);

        if hashes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(BuildCache::new(cv, hashes)))
        }
    }

    pub async fn save_cache(
        &self,
        project_id: &str,
        hashes: &HashMap<String, String>,
    ) -> Result<(), CiteError> {
        let tx = self.conn.transaction().await?;
        tx.execute(
            "DELETE FROM file_cache WHERE project_id = ?1",
            params![project_id],
        )
        .await?;

        let now = chrono::Utc::now().to_rfc3339();
        for (path, hash) in hashes {
            tx.execute(
                "INSERT INTO file_cache (file_path, project_id, sha256, last_modified)
                 VALUES (?1, ?2, ?3, ?4)",
                params![path.as_str(), project_id, hash.as_str(), now.as_str()],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn clear_cache(&self, project_id: &str) -> Result<(), CiteError> {
        self.conn
            .execute(
                "DELETE FROM file_cache WHERE project_id = ?1",
                params![project_id],
            )
            .await?;
        Ok(())
    }

    pub async fn record_build(&self, record: &BuildRecord) -> Result<(), CiteError> {
        let now = chrono::Utc::now().to_rfc3339();
        let id = uuid::Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO build_history
                        (id, project_id, compiler_version, built_at, podcast_count, timeline_count, total_words, duration_ms, was_incremental)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    id,
                    record.project_id.clone(),
                    record.compiler_version,
                    now,
                    record.podcast_count,
                    record.timeline_count,
                    record.total_words,
                    record.duration_ms,
                    record.was_incremental as i64,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn record_deployment(&self, report: &DeployReport) -> Result<(), CiteError> {
        let now = chrono::Utc::now().to_rfc3339();
        let id = uuid::Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO deployment_history
                        (id, project_id, deployment_id, deployed_at, news_count, timeline_count, asset_count, success)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id,
                    report.project_id.clone(),
                    report.deployment_id.clone(),
                    now,
                    report.news_count,
                    report.timeline_count,
                    report.asset_count,
                    report.success as i64,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn get_podcasts_with_content(
        &self,
        project_id: &str,
    ) -> Result<Vec<StoredPodcast>, CiteError> {
        let mut rows = self
            .conn
            .query(
                "SELECT title, word_count, category, file, audio, thumbnail
                 FROM podcasts WHERE project_id = ?1 ORDER BY title",
                params![project_id],
            )
            .await?;

        let mut podcasts = Vec::new();
        while let Some(row) = rows.next().await? {
            podcasts.push(StoredPodcast {
                title: row.get(0)?,
                word_count: row.get(1)?,
                category: get_opt_string(&row, 2),
                file: row.get(3)?,
                has_audio: !get_opt_string(&row, 4).is_empty(),
                has_thumbnail: !get_opt_string(&row, 5).is_empty(),
            });
        }
        Ok(podcasts)
    }

    pub async fn get_timelines(&self, project_id: &str) -> Result<Vec<StoredTimeline>, CiteError> {
        let mut rows = self
            .conn
            .query(
                "SELECT date, title, COALESCE(url, link), entry_type
                 FROM timeline_entries WHERE project_id = ?1
                 ORDER BY date DESC NULLS LAST",
                params![project_id],
            )
            .await?;

        let mut entries = Vec::new();
        while let Some(row) = rows.next().await? {
            entries.push(StoredTimeline {
                date: opt_string(&row, 0),
                title: row.get(1)?,
                url: opt_string(&row, 2),
                entry_type: opt_string(&row, 3),
            });
        }
        Ok(entries)
    }

    pub async fn get_restore_snapshot(
        &self,
        project_id: &str,
    ) -> Result<RestoredProject, CiteError> {
        let mut rows = self
            .conn
            .query(
                "SELECT name, language, artist_id, metadata_file FROM projects WHERE id = ?1",
                params![project_id],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Err(CiteError::Config(format!(
                "No local record found for project '{project_id}'"
            )));
        };
        let mut snapshot = RestoredProject {
            name: row.get(0)?,
            language: get_opt_string(&row, 1),
            artist_id: get_opt_string(&row, 2),
            metadata_file: get_opt_string(&row, 3),
            podcasts: Vec::new(),
            timelines: Vec::new(),
        };
        if snapshot.language.is_empty() {
            snapshot.language = "en".into();
        }
        if snapshot.metadata_file.is_empty() {
            snapshot.metadata_file = "metadata.yml".into();
        }

        let mut rows = self
            .conn
            .query(
                "SELECT id, title, file, source_url, category, thumbnail, audio, citation_file, content, metadata
                 FROM podcasts WHERE project_id = ?1 ORDER BY file",
                params![project_id],
            )
            .await?;
        while let Some(row) = rows.next().await? {
            snapshot.podcasts.push(RestoredPodcast {
                id: row.get(0)?,
                title: row.get(1)?,
                file: get_opt_string(&row, 2),
                source_url: opt_string(&row, 3),
                category: opt_string(&row, 4),
                thumbnail: opt_string(&row, 5),
                audio: opt_string(&row, 6),
                citation_file: opt_string(&row, 7),
                content: opt_string(&row, 8),
                metadata: opt_string(&row, 9).and_then(|json| serde_json::from_str(&json).ok()),
            });
        }

        let mut rows = self
            .conn
            .query(
                "SELECT podcast_id, date, title, summary, COALESCE(url, link), entry_type
                 FROM timeline_entries WHERE project_id = ?1
                 ORDER BY date ASC NULLS LAST",
                params![project_id],
            )
            .await?;
        while let Some(row) = rows.next().await? {
            snapshot.timelines.push(RestoredTimeline {
                podcast_id: row.get(0)?,
                from_citation: opt_string(&row, 5).as_deref() != Some("event"),
                entry: TimelineEntry {
                    id: String::new(),
                    date: opt_string(&row, 1),
                    title: get_opt_string(&row, 2),
                    description: opt_string(&row, 3),
                    url: opt_string(&row, 4),
                },
            });
        }

        Ok(snapshot)
    }

    pub async fn get_deployment_history(
        &self,
        project_id: &str,
    ) -> Result<Vec<StoredDeployment>, CiteError> {
        let mut rows = self
            .conn
            .query(
                "SELECT deployment_id, deployed_at, success, news_count, asset_count
                 FROM deployment_history WHERE project_id = ?1 AND dry_run = 0
                 ORDER BY deployed_at DESC",
                params![project_id],
            )
            .await?;

        let mut deployments = Vec::new();
        while let Some(row) = rows.next().await? {
            deployments.push(StoredDeployment {
                deployment_id: row.get(0)?,
                deployed_at: row.get(1)?,
                success: row.get::<i64>(2)? != 0,
                news_count: row.get(3)?,
                asset_count: row.get(4)?,
            });
        }
        Ok(deployments)
    }

    pub async fn get_build_history(&self, project_id: &str) -> Result<Vec<StoredBuild>, CiteError> {
        let mut rows = self
            .conn
            .query(
                "SELECT podcast_count, total_words, duration_ms, was_incremental, built_at
                 FROM build_history WHERE project_id = ?1
                 ORDER BY built_at DESC
                 LIMIT 50",
                params![project_id],
            )
            .await?;

        let mut builds = Vec::new();
        while let Some(row) = rows.next().await? {
            builds.push(StoredBuild {
                podcast_count: row.get(0)?,
                total_words: row.get(1)?,
                duration_ms: row.get(2)?,
                was_incremental: row.get::<i64>(3)? != 0,
                built_at: get_opt_string(&row, 4),
            });
        }
        Ok(builds)
    }

    pub async fn get_project_stats(&self, project_id: &str) -> Result<ProjectStats, CiteError> {
        let mut rows = self
            .conn
            .query(
                "SELECT
                    (SELECT COUNT(*) FROM podcasts WHERE project_id = ?1),
                    (SELECT COUNT(*) FROM timeline_entries WHERE project_id = ?1),
                    (SELECT COALESCE(SUM(word_count), 0) FROM podcasts WHERE project_id = ?1),
                    (SELECT COUNT(*) FROM build_history WHERE project_id = ?1),
                    (SELECT MAX(built_at) FROM build_history WHERE project_id = ?1),
                    (SELECT COUNT(*) FROM deployment_history WHERE project_id = ?1 AND success = 1 AND dry_run = 0),
                    (SELECT MAX(deployed_at) FROM deployment_history WHERE project_id = ?1 AND success = 1 AND dry_run = 0)",
                params![project_id],
            )
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| CiteError::Database("Project stats query returned no row".into()))?;

        Ok(ProjectStats {
            podcast_count: row.get(0)?,
            timeline_count: row.get(1)?,
            total_words: row.get(2)?,
            build_count: row.get(3)?,
            last_built: opt_string(&row, 4),
            deployment_count: row.get(5)?,
            last_deployed: opt_string(&row, 6),
        })
    }

    pub async fn list_db_projects(&self) -> Result<Vec<(String, String)>, CiteError> {
        let mut rows = self
            .conn
            .query("SELECT name, id FROM projects ORDER BY name", ())
            .await?;
        let mut projects = Vec::new();
        while let Some(row) = rows.next().await? {
            let name: String = row.get(0)?;
            let id: String = row.get(1)?;
            projects.push((name, id));
        }
        Ok(projects)
    }

    pub async fn get_all_stats(&self) -> Result<AllStats, CiteError> {
        let mut rows = self
            .conn
            .query(
                "SELECT
                    (SELECT COUNT(*) FROM projects),
                    (SELECT COUNT(*) FROM podcasts),
                    (SELECT COUNT(*) FROM timeline_entries),
                    (SELECT COALESCE(SUM(word_count), 0) FROM podcasts),
                    (SELECT COUNT(*) FROM build_history)",
                (),
            )
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| CiteError::Database("Global stats query returned no row".into()))?;

        Ok(AllStats {
            project_count: row.get(0)?,
            total_podcasts: row.get(1)?,
            total_timelines: row.get(2)?,
            total_words: row.get(3)?,
            total_builds: row.get(4)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_migration_and_queries() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = DbManager::open_path(&db_path).await.unwrap();

        db.conn
            .execute(
                "INSERT INTO projects (id, name) VALUES ('proj1', 'Test Project')",
                (),
            )
            .await
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO podcasts (id, project_id, title, file, word_count)
                 VALUES ('p1', 'proj1', 'Test Podcast', 'test.md', 100)",
                (),
            )
            .await
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO timeline_entries (id, project_id, podcast_id, date, title)
                 VALUES ('t1', 'proj1', 'p1', '2005-03', 'Test Entry')",
                (),
            )
            .await
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO build_history (id, project_id, compiler_version, built_at, podcast_count, timeline_count, total_words, duration_ms, was_incremental)
                 VALUES ('b1', 'proj1', 1.0, '2026-07-29T15:00:00.000Z', 1, 1, 100, 50, 0)",
                (),
            )
            .await
            .unwrap();

        let stats = db.get_project_stats("proj1").await.unwrap();
        assert_eq!(stats.podcast_count, 1);
        assert_eq!(stats.timeline_count, 1);
        assert_eq!(stats.total_words, 100);
        assert_eq!(stats.build_count, 1);
        assert_eq!(stats.deployment_count, 0);
        assert!(stats.last_built.is_some());

        let all = db.get_all_stats().await.unwrap();
        assert_eq!(all.project_count, 1);
        assert_eq!(all.total_podcasts, 1);
        assert_eq!(all.total_timelines, 1);
        assert_eq!(all.total_words, 100);
        assert_eq!(all.total_builds, 1);

        let pods = db.get_podcasts_with_content("proj1").await.unwrap();
        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].title, "Test Podcast");

        let timelines = db.get_timelines("proj1").await.unwrap();
        assert_eq!(timelines.len(), 1);

        let builds = db.get_build_history("proj1").await.unwrap();
        assert_eq!(builds.len(), 1);
    }

    #[tokio::test]
    async fn test_restore_snapshot_and_link_column() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = DbManager::open_path(&db_path).await.unwrap();

        db.conn
            .execute(
                "INSERT INTO projects (id, name, language, artist_id)
                 VALUES ('proj1', 'Restore Me', 'en', 'artist-uuid')",
                (),
            )
            .await
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO podcasts (id, project_id, title, file, source_url, content, word_count)
                 VALUES ('pod1', 'proj1', 'Episode', 'content/ep.md', 'https://example.com', '# Episode', 2)",
                (),
            )
            .await
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO timeline_entries (id, project_id, podcast_id, date, title, summary, url, link)
                 VALUES ('t1', 'proj1', 'pod1', '2024-02', 'Event', 'Summary text', 'https://example.com/a', 'https://example.com/news/b')",
                (),
            )
            .await
            .unwrap();

        let timelines = db.get_timelines("proj1").await.unwrap();
        assert_eq!(
            timelines[0].url.as_deref(),
            Some("https://example.com/a"),
            "url wins over the legacy link column"
        );

        let snap = db.get_restore_snapshot("proj1").await.unwrap();
        assert_eq!(snap.name, "Restore Me");
        assert_eq!(snap.language, "en");
        assert_eq!(snap.artist_id, "artist-uuid");
        assert_eq!(snap.podcasts.len(), 1);
        assert_eq!(snap.podcasts[0].content.as_deref(), Some("# Episode"));
        assert_eq!(
            snap.podcasts[0].source_url.as_deref(),
            Some("https://example.com")
        );
        assert_eq!(snap.timelines.len(), 1);
        let entry = &snap.timelines[0].entry;
        assert_eq!(entry.title, "Event");
        assert_eq!(entry.description.as_deref(), Some("Summary text"));
        assert!(
            snap.timelines[0].from_citation,
            "untyped rows predate inline events"
        );
        assert_eq!(entry.url.as_deref(), Some("https://example.com/a"));

        assert!(db.get_restore_snapshot("missing").await.is_err());
    }
}
