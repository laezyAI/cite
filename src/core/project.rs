//! Loaded project: `cite.toml` plus parsed metadata with build paths, discovery, and clean.
use std::path::{Path, PathBuf};

use crate::core::CiteError;
use crate::core::db::DbManager;
use crate::core::manifest::Manifest;
use crate::core::metadata::Metadata;

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
        let manifest: Manifest =
            toml::from_str(&toml_str).map_err(|e| CiteError::Parse(format!("cite.toml: {e}")))?;

        let meta_file = &manifest.project.metadata_file;
        let meta_path = root.join(meta_file);
        let metadata = if meta_path.exists() {
            let yaml_str = std::fs::read_to_string(&meta_path)?;
            Metadata::parse(&yaml_str).map_err(|e| CiteError::Parse(format!("{meta_file}: {e}")))?
        } else {
            Metadata::default()
        };

        Ok(Self {
            root: root.to_path_buf(),
            manifest,
            metadata,
        })
    }

    pub fn build_dir(&self) -> PathBuf {
        self.root.join("build")
    }

    pub fn bundle_path(&self) -> PathBuf {
        self.build_dir().join("content.json")
    }

    pub fn source_files(&self) -> Vec<PathBuf> {
        let config = ["cite.toml", self.manifest.project.metadata_file.as_str()];
        let referenced = self.metadata.referenced_files();
        config
            .into_iter()
            .chain(referenced.iter().map(String::as_str))
            .map(|f| self.root.join(f))
            .collect()
    }

    pub async fn clean(&self, db: &DbManager) -> Result<(), CiteError> {
        let build_dir = self.build_dir();
        if build_dir.exists() {
            tokio::fs::remove_dir_all(&build_dir).await?;
        }

        let _ = db.clear_cache(&self.project_id()).await;
        Ok(())
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
        let db = rt.block_on(async { DbManager::open_path(&db_path).await.unwrap() });
        rt.block_on(ctx.clean(&db)).unwrap();
        assert!(!ctx.build_dir().exists());
    }
}
