use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use sha2::{Digest, Sha256};

/// Number of hex characters of the SHA-256 digest used as the URL key.
const HASH_LEN: usize = 16;

/// In-memory lookup from path hash to the full (canonical) file path.
#[derive(Default)]
pub struct Index {
    entries: RwLock<HashMap<String, PathBuf>>,
}

impl Index {
    /// Hash a path. Callers should pass a canonical path so the same file
    /// always maps to the same hash.
    pub fn hash(path: &Path) -> String {
        let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()[..HASH_LEN]
            .to_string()
    }

    /// Record a path in the index and return its hash.
    pub fn insert(&self, path: PathBuf) -> String {
        let hash = Self::hash(&path);
        self.entries
            .write()
            .unwrap()
            .entry(hash.clone())
            .or_insert(path);
        hash
    }

    pub fn get(&self, hash: &str) -> Option<PathBuf> {
        self.entries.read().unwrap().get(hash).cloned()
    }

    /// All indexed markdown documents as `(hash, path)`, sorted by path.
    pub fn documents(&self) -> Vec<(String, PathBuf)> {
        let mut docs: Vec<_> = self
            .entries
            .read()
            .unwrap()
            .iter()
            .filter(|(_, p)| is_markdown(p))
            .map(|(h, p)| (h.clone(), p.clone()))
            .collect();
        docs.sort_by(|a, b| a.1.cmp(&b.1));
        docs
    }
}

pub fn is_markdown(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("md" | "markdown" | "mdown" | "mkd" | "mkdn")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_short() {
        let a = Index::hash(Path::new("/tmp/README.md"));
        assert_eq!(a, Index::hash(Path::new("/tmp/README.md")));
        assert_eq!(a.len(), HASH_LEN);
        assert_ne!(a, Index::hash(Path::new("/tmp/OTHER.md")));
    }

    #[test]
    fn insert_then_lookup() {
        let index = Index::default();
        let hash = index.insert(PathBuf::from("/tmp/README.md"));
        assert_eq!(index.get(&hash), Some(PathBuf::from("/tmp/README.md")));
        assert_eq!(index.get("nope"), None);
    }
}
