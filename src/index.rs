use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::Serialize;
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

/// A node in the sidebar file tree: a directory (`hash` is `None`) or a document.
#[derive(Serialize, Default, Debug, PartialEq)]
pub struct TreeNode {
    pub name: String,
    pub hash: Option<String>,
    /// Path relative to the tree root, for tooltips.
    pub path: String,
    pub children: Vec<TreeNode>,
}

/// `path` relative to `root` with `/` separators; paths outside `root` are
/// shown from the filesystem root.
pub fn relative_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Arrange `(hash, path)` documents into a directory tree relative to `root`.
/// Paths outside `root` are shown from the filesystem root. Directories come
/// before files, each sorted by name.
pub fn file_tree(docs: &[(String, PathBuf)], root: &Path) -> Vec<TreeNode> {
    let mut top = Vec::new();
    for (hash, path) in docs {
        let rel = path.strip_prefix(root).unwrap_or(path);
        let parts: Vec<String> = rel
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        let rel_display = parts.join("/");
        let mut level = &mut top;
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            let pos = level
                .iter()
                .position(|n: &TreeNode| n.name == *part && n.hash.is_some() == last);
            let pos = pos.unwrap_or_else(|| {
                level.push(TreeNode {
                    name: part.clone(),
                    hash: last.then(|| hash.clone()),
                    path: if last { rel_display.clone() } else { parts[..=i].join("/") },
                    children: Vec::new(),
                });
                level.len() - 1
            });
            level = &mut level[pos].children;
        }
    }
    sort_tree(&mut top);
    top
}

fn sort_tree(nodes: &mut [TreeNode]) {
    nodes.sort_by(|a, b| {
        (a.hash.is_some(), &a.name).cmp(&(b.hash.is_some(), &b.name))
    });
    for n in nodes {
        sort_tree(&mut n.children);
    }
}

/// Recursively collect markdown files under `dir`, sorted, skipping hidden
/// directories and common dependency/build directories.
pub fn scan_markdown(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(kind) = std::fs::metadata(&path).map(|m| m.file_type()) else {
                continue;
            };
            if kind.is_dir() {
                if !name.starts_with('.') && !matches!(&*name, "node_modules" | "target") {
                    stack.push(path);
                }
            } else if is_markdown(&path)
                && let Ok(canon) = path.canonicalize()
            {
                found.push(canon);
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

/// The document a directory opens on: a README/index sitting directly in it,
/// else the first one found. `docs` is the sorted result of `scan_markdown(dir)`.
pub fn root_document(docs: &[PathBuf], dir: &Path) -> Option<PathBuf> {
    docs.iter()
        .find(|p| p.parent() == Some(dir) && is_root_name(p))
        .or_else(|| docs.first())
        .cloned()
}

fn is_root_name(path: &Path) -> bool {
    path.file_stem()
        .is_some_and(|s| s.eq_ignore_ascii_case("readme") || s.eq_ignore_ascii_case("index"))
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
    fn tree_groups_shared_directories() {
        let docs = vec![
            ("a".to_string(), PathBuf::from("/r/README.md")),
            ("b".to_string(), PathBuf::from("/r/docs/x.md")),
            ("c".to_string(), PathBuf::from("/r/docs/y.md")),
        ];
        let tree = file_tree(&docs, Path::new("/r"));
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].name, "docs");
        assert_eq!(tree[0].hash, None);
        assert_eq!(tree[0].children.len(), 2);
        assert_eq!(tree[0].children[1].path, "docs/y.md");
        assert_eq!(tree[1].name, "README.md");
    }

    #[test]
    fn insert_then_lookup() {
        let index = Index::default();
        let hash = index.insert(PathBuf::from("/tmp/README.md"));
        assert_eq!(index.get(&hash), Some(PathBuf::from("/tmp/README.md")));
        assert_eq!(index.get("nope"), None);
    }
}
