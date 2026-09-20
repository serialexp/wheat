//! Repository registry — maps repository names to stores.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use wheat_store::{ContentStore, ImmutableObjectStore, LocalPublicationStore, LooseObjectStore};

/// A single repository's stores.
pub struct RepositoryEntry {
    pub objects: Box<dyn ImmutableObjectStore>,
    pub content: ContentStore,
    pub heads: LocalPublicationStore,
}

/// In-memory registry of repositories available on this server.
pub struct Registry {
    repos: HashMap<String, Arc<RepositoryEntry>>,
}

impl Registry {
    /// Create a registry serving a single repository from a data directory.
    ///
    /// The directory must contain `objects/`, `content/`, and `repository.head`.
    /// The repository name defaults to the directory's file name.
    pub fn single(data_dir: &Path) -> Self {
        let name = data_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("default")
            .to_string();
        Self::single_named(name, data_dir)
    }

    /// Create a registry serving a single repository with an explicit name.
    pub fn single_named(name: String, data_dir: &Path) -> Self {
        let entry = Arc::new(RepositoryEntry {
            objects: Box::new(LooseObjectStore::new(data_dir.join("objects"))),
            content: ContentStore::new(data_dir.join("content")),
            heads: LocalPublicationStore::new(data_dir.join("repository.head")),
        });
        let mut repos = HashMap::new();
        repos.insert(name, entry);
        Self { repos }
    }

    /// Create a registry serving a single repository with pre-built stores.
    pub fn single_with_stores(
        name: String,
        objects: Box<dyn ImmutableObjectStore>,
        content: ContentStore,
        heads: LocalPublicationStore,
    ) -> Self {
        let entry = Arc::new(RepositoryEntry {
            objects,
            content,
            heads,
        });
        let mut repos = HashMap::new();
        repos.insert(name, entry);
        Self { repos }
    }

    /// Create a registry from a directory where each subdirectory is a
    /// repository (named `owner/repo` from the two-level nesting).
    pub fn from_repos_dir(repos_dir: &Path) -> std::io::Result<Self> {
        let mut repos = HashMap::new();
        for owner_entry in std::fs::read_dir(repos_dir)? {
            let owner_entry = owner_entry?;
            let owner_path = owner_entry.path();
            if !owner_path.is_dir() {
                continue;
            }
            let owner_name = owner_entry.file_name();
            let Some(owner_str) = owner_name.to_str() else {
                continue;
            };
            for repo_entry in std::fs::read_dir(&owner_path)? {
                let repo_entry = repo_entry?;
                let repo_path = repo_entry.path();
                if !repo_path.is_dir() {
                    continue;
                }
                let repo_name = repo_entry.file_name();
                let Some(repo_str) = repo_name.to_str() else {
                    continue;
                };
                let name = format!("{owner_str}/{repo_str}");
                let entry = Arc::new(RepositoryEntry {
                    objects: Box::new(LooseObjectStore::new(repo_path.join("objects"))),
                    content: ContentStore::new(repo_path.join("content")),
                    heads: LocalPublicationStore::new(repo_path.join("repository.head")),
                });
                repos.insert(name, entry);
            }
        }
        Ok(Self { repos })
    }

    /// Look up a repository by name.
    pub fn get(&self, name: &str) -> Option<Arc<RepositoryEntry>> {
        self.repos.get(name).cloned()
    }

    /// List all repository names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.repos.keys().map(String::as_str)
    }

    /// Number of repositories in the registry.
    pub fn len(&self) -> usize {
        self.repos.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
    }
}
