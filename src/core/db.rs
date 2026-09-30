use std::collections::HashMap;
use std::path::{Path, PathBuf};

use libsql::{Builder, Connection, Value, params};
use tracing::info;

use crate::core::CiteError;
use crate::core::cache::BuildCache;
use crate::core::compiler::{ContentBundle, word_count};
use crate::core::project::ProjectContext;

pub fn global_db_path() -> PathBuf {
    std::env::var_os("CITE_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::core::cite_home().join("cite.db"))
}

pub struct DbManager {
    conn: Connection,
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

        if self
            .conn
            .query("SELECT link FROM timeline_entries LIMIT 1", ())
            .await
            .is_err()
        {
            self.conn
                .execute("ALTER TABLE timeline_entries ADD COLUMN link TEXT", ())
                .await?;
        }

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

    /// Replace the local snapshot of a project with the freshly compiled bundle.
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

        for pod in &bundle.podcasts {
            let meta = &pod.podcast;
            tx.execute(
                "INSERT INTO podcasts (id, project_id, title, file, source_url, category,
                        thumbnail, audio, citation_file, content, word_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                ],
            )
            .await?;
        }

        for timeline in &bundle.timelines {
            for entry in &timeline.entries {
                tx.execute(
                    "INSERT INTO timeline_entries
                        (id, project_id, podcast_id, date, title, summary, url, link)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        entry.id.as_str(),
                        project_id.as_str(),
                        timeline.podcast_id.as_str(),
                        entry.date.as_deref(),
                        entry.title.as_str(),
                        entry.summary.as_deref(),
                        entry.url.as_deref(),
                        entry.link.as_deref(),
                    ],
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

    pub async fn record_build(
        &self,
        record: &super::project::BuildRecord,
    ) -> Result<(), CiteError> {
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

    pub async fn record_deployment(
        &self,
        report: &super::project::DeployReport,
    ) -> Result<(), CiteError> {
        let now = chrono::Utc::now().to_rfc3339();
        let id = uuid::Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO deployment_history
                        (id, project_id, deployment_id, deployed_at, storage_path, news_count, timeline_count, asset_count, success, dry_run)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    id,
                    report.project_id.clone(),
                    report.deployment_id.clone(),
                    now,
                    report.storage_path.clone(),
                    report.news_count,
                    report.timeline_count,
                    report.asset_count,
                    report.success as i64,
                    report.dry_run as i64,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn get_podcasts_with_content(
        &self,
        project_id: &str,
    ) -> Result<Vec<super::project::StoredPodcast>, CiteError> {
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
            podcasts.push(super::project::StoredPodcast {
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

    pub async fn get_timelines(
        &self,
        project_id: &str,
    ) -> Result<Vec<super::project::StoredTimeline>, CiteError> {
        let mut rows = self
            .conn
            .query(
                "SELECT date, title, url, entry_type, link
                 FROM timeline_entries WHERE project_id = ?1
                 ORDER BY date DESC NULLS LAST",
                params![project_id],
            )
            .await?;

        let mut entries = Vec::new();
        while let Some(row) = rows.next().await? {
            entries.push(super::project::StoredTimeline {
                date: opt_string(&row, 0),
                title: row.get(1)?,
                url: opt_string(&row, 2),
                entry_type: opt_string(&row, 3),
                link: opt_string(&row, 4),
            });
        }
        Ok(entries)
    }

    pub async fn get_restore_snapshot(
        &self,
        project_id: &str,
    ) -> Result<super::project::RestoredProject, CiteError> {
        use super::project::{RestoredPodcast, RestoredProject, RestoredTimeline};

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
                "SELECT id, title, file, source_url, category, thumbnail, audio, citation_file, content
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
            });
        }

        let mut rows = self
            .conn
            .query(
                "SELECT podcast_id, date, title, summary, url, link
                 FROM timeline_entries WHERE project_id = ?1
                 ORDER BY date ASC NULLS LAST",
                params![project_id],
            )
            .await?;
        while let Some(row) = rows.next().await? {
            snapshot.timelines.push(RestoredTimeline {
                podcast_id: row.get(0)?,
                date: opt_string(&row, 1),
                title: get_opt_string(&row, 2),
                summary: opt_string(&row, 3),
                url: opt_string(&row, 4),
                link: opt_string(&row, 5),
            });
        }

        Ok(snapshot)
    }

    pub async fn get_deployment_history(
        &self,
        project_id: &str,
    ) -> Result<Vec<super::project::StoredDeployment>, CiteError> {
        let mut rows = self
            .conn
            .query(
                "SELECT deployment_id, deployed_at, success, news_count, asset_count
                 FROM deployment_history WHERE project_id = ?1
                 ORDER BY deployed_at DESC",
                params![project_id],
            )
            .await?;

        let mut deployments = Vec::new();
        while let Some(row) = rows.next().await? {
            deployments.push(super::project::StoredDeployment {
                deployment_id: row.get(0)?,
                deployed_at: row.get(1)?,
                success: row.get::<i64>(2)? != 0,
                news_count: row.get(3)?,
                asset_count: row.get(4)?,
            });
        }
        Ok(deployments)
    }

    pub async fn get_build_history(
        &self,
        project_id: &str,
    ) -> Result<Vec<super::project::StoredBuild>, CiteError> {
        let mut rows = self
            .conn
            .query(
                "SELECT podcast_count, timeline_count, total_words, duration_ms, was_incremental, built_at
                 FROM build_history WHERE project_id = ?1
                 ORDER BY built_at DESC
                 LIMIT 50",
                params![project_id],
            )
            .await?;

        let mut builds = Vec::new();
        while let Some(row) = rows.next().await? {
            builds.push(super::project::StoredBuild {
                podcast_count: row.get(0)?,
                timeline_count: row.get(1)?,
                total_words: row.get(2)?,
                duration_ms: row.get(3)?,
                was_incremental: row.get::<i64>(4)? != 0,
                built_at: get_opt_string(&row, 5),
            });
        }
        Ok(builds)
    }

    pub async fn get_project_stats(
        &self,
        project_id: &str,
    ) -> Result<super::project::ProjectStats, CiteError> {
        let mut rows = self
            .conn
            .query(
                "SELECT
                    (SELECT COUNT(*) FROM podcasts WHERE project_id = ?1),
                    (SELECT COUNT(*) FROM timeline_entries WHERE project_id = ?1),
                    (SELECT COALESCE(SUM(word_count), 0) FROM podcasts WHERE project_id = ?1),
                    (SELECT COUNT(*) FROM build_history WHERE project_id = ?1),
                    (SELECT MAX(built_at) FROM build_history WHERE project_id = ?1),
                    (SELECT COUNT(*) FROM deployment_history WHERE project_id = ?1 AND success = 1),
                    (SELECT MAX(deployed_at) FROM deployment_history WHERE project_id = ?1 AND success = 1)",
                params![project_id],
            )
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| CiteError::Database("Project stats query returned no row".into()))?;

        Ok(super::project::ProjectStats {
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

    pub async fn get_all_stats(&self) -> Result<super::project::AllStats, CiteError> {
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

        Ok(super::project::AllStats {
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
            timelines[0].link.as_deref(),
            Some("https://example.com/news/b")
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
        assert_eq!(snap.timelines[0].title, "Event");
        assert_eq!(snap.timelines[0].summary.as_deref(), Some("Summary text"));
        assert_eq!(
            snap.timelines[0].link.as_deref(),
            Some("https://example.com/news/b")
        );

        assert!(db.get_restore_snapshot("missing").await.is_err());
    }
}
