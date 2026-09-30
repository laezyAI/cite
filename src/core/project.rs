use std::path::{Path, PathBuf};

use crate::core::CiteError;
use crate::core::manifest::Manifest;
use crate::core::metadata::Metadata;
use tracing::info;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BuildRecord {
    pub project_id: String,
    pub compiler_version: f64,
    pub podcast_count: i64,
    pub timeline_count: i64,
    pub total_words: i64,
    pub duration_ms: i64,
    pub was_incremental: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeployReport {
    pub project_id: String,
    pub deployment_id: String,
    pub storage_path: String,
    pub news_count: i64,
    pub timeline_count: i64,
    pub asset_count: i64,
    pub success: bool,
    pub dry_run: bool,
}

/// A podcast record from the DB (with content)
#[derive(Debug, Clone)]
pub struct StoredPodcast {
    pub title: String,
    pub word_count: i64,
    pub category: String,
    pub file: String,
    pub has_audio: bool,
    pub has_thumbnail: bool,
}

/// A timeline entry from the DB
#[derive(Debug, Clone)]
pub struct StoredTimeline {
    pub date: Option<String>,
    pub title: String,
    pub url: Option<String>,
    pub entry_type: Option<String>,
    pub link: Option<String>,
}

/// A deployment record from the DB
#[derive(Debug, Clone)]
pub struct StoredDeployment {
    pub deployment_id: String,
    pub deployed_at: String,
    pub success: bool,
    pub news_count: i64,
    pub asset_count: i64,
}

/// A build record from the DB
#[derive(Debug, Clone)]
pub struct StoredBuild {
    pub podcast_count: i64,
    pub timeline_count: i64,
    pub total_words: i64,
    pub duration_ms: i64,
    pub was_incremental: bool,
    pub built_at: String,
}

/// Per-project analytics from the DB
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

/// Cross-project analytics from the DB
#[derive(Debug, Clone)]
pub struct AllStats {
    pub project_count: i64,
    pub total_podcasts: i64,
    pub total_timelines: i64,
    pub total_words: i64,
    pub total_builds: i64,
}

/// A podcast row with everything needed to restore project files
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
    pub content: Option<String>,
}

/// A timeline row with its owning timeline group
#[derive(Debug, Clone)]
pub struct RestoredTimeline {
    pub podcast_id: String,
    pub date: Option<String>,
    pub title: String,
    pub summary: Option<String>,
    pub url: Option<String>,
    pub link: Option<String>,
}

/// Project-level data for restoring an archived project
#[derive(Debug, Clone)]
pub struct RestoredProject {
    pub name: String,
    pub language: String,
    pub artist_id: String,
    pub metadata_file: String,
    pub podcasts: Vec<RestoredPodcast>,
    pub timelines: Vec<RestoredTimeline>,
}

#[derive(Debug, Clone)]
pub struct ProjectContext {
    pub root: PathBuf,
    pub manifest: Manifest,
    pub metadata: Metadata,
}

impl ProjectContext {
    pub fn project_id(&self) -> String {
        self.root.to_string_lossy().to_string()
    }

    pub fn load(root: &Path) -> Result<Self, CiteError> {
        let manifest_path = root.join("cite.toml");
        if !manifest_path.exists() {
            return Err(CiteError::Config(format!(
                "No cite.toml found at '{}'. Run 'cite init' first.",
                manifest_path.display()
            )));
        }
        let toml_str = std::fs::read_to_string(&manifest_path)?;
        let manifest: Manifest = toml::from_str(&toml_str)?;

        let meta_path = root.join(&manifest.project.metadata_file);
        let metadata = if meta_path.exists() {
            let yaml_str = std::fs::read_to_string(&meta_path)?;
            serde_yaml::from_str(&yaml_str)?
        } else {
            Metadata::default()
        };

        Ok(Self {
            root: root.to_path_buf(),
            manifest,
            metadata,
        })
    }

    pub fn content_dir(&self) -> PathBuf {
        self.root.join("content")
    }

    pub fn build_dir(&self) -> PathBuf {
        self.root.join("build")
    }

    pub fn content_files(&self) -> Vec<PathBuf> {
        self.metadata
            .referenced_files()
            .iter()
            .map(|f| self.root.join(f))
            .collect()
    }

    pub async fn clean(&self, db: &crate::core::db::DbManager) -> Result<(), CiteError> {
        let build_dir = self.build_dir();
        if build_dir.exists() {
            tokio::fs::remove_dir_all(&build_dir).await?;
        }

        let _ = db.clear_cache(&self.project_id()).await;
        Ok(())
    }
}

pub async fn print_status(db: &crate::core::db::DbManager, ctx: &ProjectContext) {
    info!("Name: {}", ctx.manifest.project.name);
    info!("Root: {}", ctx.root.display());
    info!("Artist ID: {}", ctx.manifest.project.artist_id);
    if let Some(b) = &ctx.manifest.backend
        && let Some(u) = &b.staging_url
    {
        info!("Staging: {u}");
    }
    info!("Podcasts: {}", ctx.metadata.podcasts.len());

    let project_id = ctx.project_id();

    if let Ok(stats) = db.get_project_stats(&project_id).await {
        info!("Total words: {}", stats.total_words);
        info!("Timeline entries: {}", stats.timeline_count);
        info!("Builds recorded: {}", stats.build_count);
        if let Some(ref last) = stats.last_built {
            info!("Last build: {last}");
        }
        info!("Deployments: {}", stats.deployment_count);
        if let Some(ref last) = stats.last_deployed {
            info!("Last deploy: {last}");
        }
    }

    if let Ok(builds) = db.get_build_history(&project_id).await
        && let Some(b) = builds.first()
    {
        info!(
            "Recent build: {} podcasts, {} timelines, {} words, {}ms ({})",
            b.podcast_count,
            b.timeline_count,
            b.total_words,
            b.duration_ms,
            if b.was_incremental { "incr" } else { "full" },
        );
    }

    if let Ok(deploys) = db.get_deployment_history(&project_id).await
        && let Some(d) = deploys.first()
    {
        info!(
            "Recent deploy: {} at {} ({})",
            d.deployment_id,
            d.deployed_at,
            if d.success { "ok" } else { "fail" },
        );
    }
}

pub fn discover_projects(root: &Path) -> Vec<PathBuf> {
    let mut projects = Vec::new();

    if root.join("cite.toml").exists() {
        let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        projects.push(canon);
    }

    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && p != root && p.join("cite.toml").exists() {
                let canon = p.canonicalize().unwrap_or(p);
                projects.push(canon);
            }
        }
    }

    projects.sort();
    projects.dedup();
    projects
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_projects_none() {
        let dir = tempfile::tempdir().unwrap();
        let projects = discover_projects(dir.path());
        assert!(projects.is_empty());
    }

    #[test]
    fn test_discover_projects_current_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cite.toml"), "[project]\nname = \"test\"\n").unwrap();
        let expected = dir.path().canonicalize().unwrap();
        let projects = discover_projects(dir.path());
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0], expected);
    }

    #[test]
    fn test_discover_projects_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("cite.toml"), "[project]\nname = \"sub\"\n").unwrap();
        let expected = sub.canonicalize().unwrap();
        let projects = discover_projects(dir.path());
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0], expected);
    }

    #[test]
    fn test_discover_projects_both() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cite.toml"), "[project]\nname = \"root\"\n").unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("cite.toml"), "[project]\nname = \"sub\"\n").unwrap();
        let projects = discover_projects(dir.path());
        assert_eq!(projects.len(), 2);
    }

    #[test]
    fn test_project_context_load_fails_without_cite_toml() {
        let dir = tempfile::tempdir().unwrap();
        let result = ProjectContext::load(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cite.toml"));
    }

    #[test]
    fn test_project_context_load_success() {
        let dir = tempfile::tempdir().unwrap();
        let toml_content = r#"
[project]
name = "test-project"
language = "en"
metadata_file = "meta.yml"
artist_id = "00000000-0000-0000-0000-000000000001"

[build]
compiler_version = 1.0
incremental = true
"#;
        std::fs::write(dir.path().join("cite.toml"), toml_content).unwrap();
        std::fs::write(dir.path().join("meta.yml"), "podcasts: []").unwrap();
        let ctx = ProjectContext::load(dir.path()).unwrap();
        assert_eq!(ctx.manifest.project.name, "test-project");
        assert_eq!(ctx.manifest.project.language, "en");
        assert_eq!(ctx.manifest.build.compiler_version, 1.0);
    }

    #[test]
    fn test_project_context_content_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cite.toml"), "[project]\nname = \"x\"\n").unwrap();
        let ctx = ProjectContext::load(dir.path()).unwrap();
        assert_eq!(ctx.content_dir(), dir.path().join("content"));
    }

    #[test]
    fn test_project_context_build_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cite.toml"), "[project]\nname = \"x\"\n").unwrap();
        let ctx = ProjectContext::load(dir.path()).unwrap();
        assert_eq!(ctx.build_dir(), dir.path().join("build"));
    }

    #[test]
    fn test_project_context_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cite.toml"), "[project]\nname = \"x\"\n").unwrap();
        let ctx = ProjectContext::load(dir.path()).unwrap();
        assert_eq!(ctx.project_id(), dir.path().to_string_lossy());
    }

    #[test]
    fn test_project_context_clean_removes_build_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cite.toml"), "[project]\nname = \"x\"\n").unwrap();
        std::fs::create_dir(dir.path().join("build")).unwrap();
        std::fs::write(dir.path().join("build").join("artifact.txt"), "data").unwrap();
        let ctx = ProjectContext::load(dir.path()).unwrap();
        assert!(ctx.build_dir().exists());
        let db_path = dir.path().join("test.db");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = rt.block_on(async {
            crate::core::db::DbManager::open_path(&db_path)
                .await
                .unwrap()
        });
        rt.block_on(ctx.clean(&db)).unwrap();
        assert!(!ctx.build_dir().exists());
    }
}
