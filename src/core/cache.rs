use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::core::CiteError;

#[derive(Debug, Clone)]
pub struct BuildCache {
    pub compiler_version: f64,
    pub hashes: HashMap<String, String>,
}

impl BuildCache {
    pub fn new(compiler_version: f64, hashes: HashMap<String, String>) -> Self {
        Self {
            compiler_version,
            hashes,
        }
    }

    pub fn changed_since(&self, current: &HashMap<String, String>) -> Vec<String> {
        let mut changed = Vec::new();
        for (path, hash) in current {
            match self.hashes.get(path) {
                Some(old) if old == hash => {}
                _ => changed.push(path.clone()),
            }
        }
        for path in self.hashes.keys() {
            if !current.contains_key(path) {
                changed.push(path.clone());
            }
        }
        changed
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UuidCache {
    pub mapping: HashMap<String, String>,
}

impl UuidCache {
    pub fn load(root: &Path) -> Self {
        let path = root.join(".cite").join("cache").join("uuid_map.json");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| Self {
                mapping: HashMap::new(),
            })
    }

    pub fn save(&self, root: &Path) {
        let dir = root.join(".cite").join("cache");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("uuid_map.json");
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    pub fn get_or_create(&mut self, key: &str) -> String {
        if let Some(id) = self.mapping.get(key) {
            return id.clone();
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.mapping.insert(key.to_string(), id.clone());
        id
    }
}

/// Hash every existing file in `files`, off the async runtime.
pub async fn hash_files(files: Vec<PathBuf>) -> Result<HashMap<String, String>, CiteError> {
    tokio::task::spawn_blocking(move || {
        let mut hashes = HashMap::with_capacity(files.len());
        for path in files {
            if path.is_file() {
                let hash = sha256_file(&path)?;
                hashes.insert(path.to_string_lossy().into_owned(), hash);
            }
        }
        Ok(hashes)
    })
    .await
    .map_err(|e| CiteError::Config(format!("Hashing task failed: {e}")))?
}

/// Streaming SHA-256 of a file as lowercase hex; never loads the whole file into memory.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(to_hex(&hasher.finalize()))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_changed_since_new_file() {
        let cache = BuildCache::new(0.0, HashMap::new());
        let mut current = HashMap::new();
        current.insert("a.md".into(), "abc".into());
        let changed = cache.changed_since(&current);
        assert_eq!(changed, vec!["a.md"]);
    }

    #[test]
    fn test_changed_since_unchanged() {
        let mut hashes = HashMap::new();
        hashes.insert("a.md".into(), "abc".into());
        let cache = BuildCache::new(0.0, hashes);
        let mut current = HashMap::new();
        current.insert("a.md".into(), "abc".into());
        let changed = cache.changed_since(&current);
        assert!(changed.is_empty());
    }

    #[test]
    fn test_changed_since_modified() {
        let mut hashes = HashMap::new();
        hashes.insert("a.md".into(), "abc".into());
        let cache = BuildCache::new(0.0, hashes);
        let mut current = HashMap::new();
        current.insert("a.md".into(), "def".into());
        let changed = cache.changed_since(&current);
        assert_eq!(changed, vec!["a.md"]);
    }

    #[test]
    fn test_sha256_file_known_value() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("hello.txt");
        std::fs::write(&f, "hello").unwrap();
        assert_eq!(
            sha256_file(&f).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[tokio::test]
    async fn test_hash_files_skips_missing() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("a.md");
        std::fs::write(&present, "a").unwrap();
        let hashes = hash_files(vec![present.clone(), dir.path().join("missing.md")])
            .await
            .unwrap();
        assert_eq!(hashes.len(), 1);
        assert!(hashes.contains_key(present.to_string_lossy().as_ref()));
    }

    #[test]
    fn test_uuid_cache_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = UuidCache::load(dir.path());
        let id = cache.get_or_create("test-key");
        assert!(!id.is_empty());
        cache.save(dir.path());
        let loaded = UuidCache::load(dir.path());
        assert_eq!(loaded.mapping.get("test-key"), Some(&id));
    }
}
