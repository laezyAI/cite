//! Content hashing and the incremental build cache (SHA-256 of sources plus compiler version).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};

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

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(to_hex(&hasher.finalize()))
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    to_hex(&Sha256::digest(bytes))
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
    fn test_changed_since_removed_file() {
        let hashes = HashMap::from([("a.md".to_string(), "abc".to_string())]);
        let cache = BuildCache::new(0.0, hashes);
        assert_eq!(cache.changed_since(&HashMap::new()), vec!["a.md"]);
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
}
