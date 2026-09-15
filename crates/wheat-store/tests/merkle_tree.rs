use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use bytes::Bytes;
use object_store::memory::InMemory;
use wheat_core::{ContentId, DirectoryEntry, DirectoryNode, FileKind, FileValue, ObjectId};
use wheat_store::{
    ImmutableObjectStore, LooseObjectStore, MerkleTreeError, MerkleTreeStore, ObjectKind,
    ObjectStoreAdapter, ObjectStoreError, StoredObject, TreeDiff, TreeDiffLimits, TreeEdit,
    TreeFile, TreePathLimits, TreePathSelection, TreeSelectionLimits, TreeWalkLimits,
};

struct CountingStore<S> {
    inner: S,
    reads: AtomicUsize,
}

impl<S> CountingStore<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            reads: AtomicUsize::new(0),
        }
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl<S: ImmutableObjectStore> ImmutableObjectStore for CountingStore<S> {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, canonical_bytes).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

struct MalformedTreeStore;

#[async_trait]
impl ImmutableObjectStore for MalformedTreeStore {
    async fn put_if_absent(
        &self,
        _id: ObjectId,
        _canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        unreachable!()
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        Ok(StoredObject {
            id,
            kind: ObjectKind::Tree,
            canonical_bytes: Bytes::from_static(b"not a canonical directory node"),
        })
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        Ok(vec![true; ids.len()])
    }
}

fn content(value: &str) -> ContentId {
    ContentId::of(value.as_bytes())
}

fn file(value: &str) -> FileValue {
    FileValue {
        content: content(value),
        kind: FileKind::Regular,
    }
}

fn edit(path: &[&str], value: Option<&str>) -> TreeEdit {
    TreeEdit {
        path: path
            .iter()
            .map(|component| (*component).to_owned())
            .collect(),
        value: value.map(file),
    }
}

async fn conformance<S: ImmutableObjectStore>(objects: &S) {
    let trees = MerkleTreeStore::new(objects);
    let empty = trees.empty_root().await.unwrap();
    let initial = trees
        .apply_edits(
            empty,
            &[
                edit(&["src", "auth.rs"], Some("auth-v1")),
                edit(&["src", "lib.rs"], Some("lib")),
                edit(&["docs", "readme.md"], Some("docs")),
            ],
        )
        .await
        .unwrap();
    let initial_root = trees.read_node(initial).await.unwrap();
    let initial_docs = match initial_root.entries["docs"] {
        DirectoryEntry::Directory(id) => id,
        _ => panic!("docs is a directory"),
    };
    let initial_src = match initial_root.entries["src"] {
        DirectoryEntry::Directory(id) => id,
        _ => panic!("src is a directory"),
    };

    let updated = trees
        .apply_edits(initial, &[edit(&["src", "auth.rs"], Some("auth-v2"))])
        .await
        .unwrap();
    let updated_root = trees.read_node(updated).await.unwrap();
    let updated_docs = match updated_root.entries["docs"] {
        DirectoryEntry::Directory(id) => id,
        _ => panic!("docs is a directory"),
    };
    let updated_src = match updated_root.entries["src"] {
        DirectoryEntry::Directory(id) => id,
        _ => panic!("src is a directory"),
    };

    assert_ne!(initial, updated);
    assert_ne!(initial_src, updated_src);
    assert_eq!(initial_docs, updated_docs);
    assert_eq!(
        trees
            .lookup(updated, &["src".into(), "auth.rs".into()])
            .await
            .unwrap(),
        Some(file("auth-v2"))
    );
    assert_eq!(
        trees
            .lookup(initial, &["src".into(), "auth.rs".into()])
            .await
            .unwrap(),
        Some(file("auth-v1"))
    );

    let pruned = trees
        .apply_edits(
            updated,
            &[
                edit(&["docs", "readme.md"], None),
                edit(&["src", "auth.rs"], None),
            ],
        )
        .await
        .unwrap();
    let pruned_root = trees.read_node(pruned).await.unwrap();
    assert!(!pruned_root.entries.contains_key("docs"));
    assert_eq!(
        trees
            .lookup(pruned, &["src".into(), "lib.rs".into()])
            .await
            .unwrap(),
        Some(file("lib"))
    );
}

#[tokio::test]
async fn loose_store_reopens_and_structurally_shares_merkle_trees() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    conformance(&objects).await;

    let reopened = LooseObjectStore::new(directory.path());
    let trees = MerkleTreeStore::new(&reopened);
    let root = trees
        .apply_edits(
            trees.empty_root().await.unwrap(),
            &[edit(&["nested", "file"], Some("value"))],
        )
        .await
        .unwrap();
    assert_eq!(
        MerkleTreeStore::new(&reopened)
            .lookup(root, &["nested".into(), "file".into()])
            .await
            .unwrap(),
        Some(file("value"))
    );
}

#[tokio::test]
async fn apache_memory_store_passes_merkle_tree_conformance() {
    let backend = Arc::new(InMemory::new());
    let objects = ObjectStoreAdapter::new(backend, "merkle-conformance");
    conformance(&objects).await;
}

#[tokio::test]
async fn file_kind_changes_tree_identity_and_all_kinds_survive_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let trees = MerkleTreeStore::new(&objects);
    let empty = trees.empty_root().await.unwrap();
    let id = content("same");
    let regular = trees
        .apply_edits(
            empty,
            &[TreeEdit {
                path: vec!["file".into()],
                value: Some(FileValue {
                    content: id,
                    kind: FileKind::Regular,
                }),
            }],
        )
        .await
        .unwrap();
    let executable = trees
        .apply_edits(
            empty,
            &[TreeEdit {
                path: vec!["file".into()],
                value: Some(FileValue {
                    content: id,
                    kind: FileKind::Executable,
                }),
            }],
        )
        .await
        .unwrap();
    assert_ne!(regular, executable);

    let all = trees
        .apply_edits(
            empty,
            &[
                TreeEdit {
                    path: vec!["regular".into()],
                    value: Some(FileValue {
                        content: id,
                        kind: FileKind::Regular,
                    }),
                },
                TreeEdit {
                    path: vec!["executable".into()],
                    value: Some(FileValue {
                        content: id,
                        kind: FileKind::Executable,
                    }),
                },
                TreeEdit {
                    path: vec!["symlink".into()],
                    value: Some(FileValue {
                        content: id,
                        kind: FileKind::Symlink,
                    }),
                },
            ],
        )
        .await
        .unwrap();
    drop(objects);
    let reopened = LooseObjectStore::new(directory.path());
    let trees = MerkleTreeStore::new(&reopened);
    for (path, kind) in [
        ("regular", FileKind::Regular),
        ("executable", FileKind::Executable),
        ("symlink", FileKind::Symlink),
    ] {
        assert_eq!(
            trees.lookup(all, &[path.into()]).await.unwrap(),
            Some(FileValue { content: id, kind })
        );
    }
}

#[tokio::test]
async fn walk_files_is_lexical_nested_and_preserves_all_file_kinds() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let trees = MerkleTreeStore::new(&objects);
    let id = content("same");
    let root = trees
        .apply_edits(
            trees.empty_root().await.unwrap(),
            &[
                TreeEdit {
                    path: vec!["z-last".into()],
                    value: Some(FileValue {
                        content: id,
                        kind: FileKind::Symlink,
                    }),
                },
                TreeEdit {
                    path: vec!["a-dir".into(), "z-executable".into()],
                    value: Some(FileValue {
                        content: id,
                        kind: FileKind::Executable,
                    }),
                },
                TreeEdit {
                    path: vec!["a-dir".into(), "a-regular".into()],
                    value: Some(FileValue {
                        content: id,
                        kind: FileKind::Regular,
                    }),
                },
                TreeEdit {
                    path: vec!["a-file".into()],
                    value: Some(FileValue {
                        content: id,
                        kind: FileKind::Regular,
                    }),
                },
            ],
        )
        .await
        .unwrap();

    assert_eq!(
        trees
            .walk_files(root, TreeWalkLimits::default())
            .await
            .unwrap(),
        vec![
            TreeFile {
                path: "a-dir/a-regular".into(),
                value: FileValue {
                    content: id,
                    kind: FileKind::Regular,
                },
            },
            TreeFile {
                path: "a-dir/z-executable".into(),
                value: FileValue {
                    content: id,
                    kind: FileKind::Executable,
                },
            },
            TreeFile {
                path: "a-file".into(),
                value: FileValue {
                    content: id,
                    kind: FileKind::Regular,
                },
            },
            TreeFile {
                path: "z-last".into(),
                value: FileValue {
                    content: id,
                    kind: FileKind::Symlink,
                },
            },
        ]
    );
}

#[tokio::test]
async fn walk_files_accepts_exact_limits_and_rejects_one_over() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let trees = MerkleTreeStore::new(&objects);
    let root = trees
        .apply_edits(
            trees.empty_root().await.unwrap(),
            &[
                edit(&["a", "one"], Some("one")),
                edit(&["b", "two"], Some("two")),
            ],
        )
        .await
        .unwrap();
    let exact = TreeWalkLimits {
        max_files: 2,
        max_nodes: 3,
        max_entries: 4,
        max_total_path_bytes: 10,
        max_path_bytes: 5,
        max_depth: 1,
        max_node_bytes: 1024,
        max_buffered_entries: 2,
    };
    assert_eq!(trees.walk_files(root, exact).await.unwrap().len(), 2);

    assert!(matches!(
        trees
            .walk_files(
                root,
                TreeWalkLimits {
                    max_files: 1,
                    ..exact
                },
            )
            .await,
        Err(MerkleTreeError::FileLimitExceeded { limit: 1 })
    ));
    assert!(matches!(
        trees
            .walk_files(
                root,
                TreeWalkLimits {
                    max_path_bytes: 4,
                    ..exact
                },
            )
            .await,
        Err(MerkleTreeError::PathBytesExceeded { size: 5, limit: 4 })
    ));
    assert!(matches!(
        trees
            .walk_files(
                root,
                TreeWalkLimits {
                    max_depth: 0,
                    ..exact
                },
            )
            .await,
        Err(MerkleTreeError::DepthExceeded { depth: 1, limit: 0 })
    ));
    assert!(matches!(
        trees
            .walk_files(
                root,
                TreeWalkLimits {
                    max_nodes: 2,
                    ..exact
                },
            )
            .await,
        Err(MerkleTreeError::NodeLimitExceeded { limit: 2 })
    ));
    assert!(matches!(
        trees
            .walk_files(
                root,
                TreeWalkLimits {
                    max_entries: 3,
                    ..exact
                },
            )
            .await,
        Err(MerkleTreeError::EntryLimitExceeded { limit: 3 })
    ));
    assert!(matches!(
        trees
            .walk_files(
                root,
                TreeWalkLimits {
                    max_total_path_bytes: 9,
                    ..exact
                },
            )
            .await,
        Err(MerkleTreeError::PathBytesExceeded { limit: 9, .. })
    ));
    assert!(matches!(
        trees
            .walk_files(
                root,
                TreeWalkLimits {
                    max_buffered_entries: 1,
                    ..exact
                },
            )
            .await,
        Err(MerkleTreeError::BufferedEntriesExceeded { limit: 1 })
    ));
    assert!(matches!(
        trees
            .walk_files(
                root,
                TreeWalkLimits {
                    max_node_bytes: 1,
                    ..exact
                },
            )
            .await,
        Err(MerkleTreeError::NodeBytesExceeded { limit: 1, .. })
    ));
}

#[tokio::test]
async fn walk_path_distinguishes_kinds_is_lexical_and_confines_reads() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let trees = MerkleTreeStore::new(&objects);
    let root = trees
        .apply_edits(
            trees.empty_root().await.unwrap(),
            &[
                edit(&["selected", "z"], Some("z")),
                edit(&["selected", "nested", "b"], Some("b")),
                edit(&["selected", "nested", "a"], Some("a")),
                edit(&["outside", "deep", "unread"], Some("outside")),
                edit(&["top"], Some("top")),
            ],
        )
        .await
        .unwrap();

    let objects = CountingStore::new(objects);
    let trees = MerkleTreeStore::new(&objects);
    assert_eq!(
        trees
            .walk_path(root, &["missing".into()], TreePathLimits::default())
            .await
            .unwrap(),
        TreePathSelection::Missing
    );
    assert_eq!(objects.reads(), 1);

    assert_eq!(
        trees
            .walk_path(root, &["top".into()], TreePathLimits::default())
            .await
            .unwrap(),
        TreePathSelection::File(TreeFile {
            path: "top".into(),
            value: file("top"),
        })
    );
    assert_eq!(objects.reads(), 2);

    assert_eq!(
        trees
            .walk_path(root, &["selected".into()], TreePathLimits::default())
            .await
            .unwrap(),
        TreePathSelection::Directory(vec![
            TreeFile {
                path: "selected/nested/a".into(),
                value: file("a"),
            },
            TreeFile {
                path: "selected/nested/b".into(),
                value: file("b"),
            },
            TreeFile {
                path: "selected/z".into(),
                value: file("z"),
            },
        ])
    );
    // Root, selected, and selected/nested only: outside/deep is never read.
    assert_eq!(objects.reads(), 5);

    let root_selection = trees
        .walk_path(root, &[], TreePathLimits::default())
        .await
        .unwrap();
    let TreePathSelection::Directory(files) = root_selection else {
        panic!("empty path selects the root directory")
    };
    assert_eq!(
        files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        [
            "outside/deep/unread",
            "selected/nested/a",
            "selected/nested/b",
            "selected/z",
            "top",
        ]
    );
}

#[tokio::test]
async fn walk_path_enforces_every_resource_limit() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let trees = MerkleTreeStore::new(&objects);
    let root = trees
        .apply_edits(
            trees.empty_root().await.unwrap(),
            &[
                edit(&["dir", "a"], Some("a")),
                edit(&["dir", "nested", "b"], Some("b")),
            ],
        )
        .await
        .unwrap();
    let exact = TreePathLimits {
        max_files: 2,
        max_nodes: 3,
        max_entries: 4,
        max_path_bytes: 12,
        max_depth: 2,
        max_node_bytes: 16 * 1024 * 1024,
        max_buffered_entries: 2,
    };
    assert!(matches!(
        trees.walk_path(root, &["dir".into()], exact).await,
        Ok(TreePathSelection::Directory(files)) if files.len() == 2
    ));

    for (limits, expected) in [
        (
            TreePathLimits {
                max_files: 1,
                ..exact
            },
            MerkleTreeError::FileLimitExceeded { limit: 1 },
        ),
        (
            TreePathLimits {
                max_nodes: 2,
                ..exact
            },
            MerkleTreeError::NodeLimitExceeded { limit: 2 },
        ),
        (
            TreePathLimits {
                max_entries: 3,
                ..exact
            },
            MerkleTreeError::EntryLimitExceeded { limit: 3 },
        ),
        (
            TreePathLimits {
                max_path_bytes: 11,
                ..exact
            },
            MerkleTreeError::PathBytesExceeded {
                size: 12,
                limit: 11,
            },
        ),
        (
            TreePathLimits {
                max_depth: 1,
                ..exact
            },
            MerkleTreeError::DepthExceeded { depth: 2, limit: 1 },
        ),
        (
            TreePathLimits {
                max_node_bytes: 0,
                ..exact
            },
            MerkleTreeError::NodeBytesExceeded { size: 0, limit: 0 },
        ),
        (
            TreePathLimits {
                max_buffered_entries: 1,
                ..exact
            },
            MerkleTreeError::BufferedEntriesExceeded { limit: 1 },
        ),
    ] {
        let error = trees
            .walk_path(root, &["dir".into()], limits)
            .await
            .unwrap_err();
        assert_eq!(
            std::mem::discriminant(&error),
            std::mem::discriminant(&expected)
        );
        match (error, expected) {
            (
                MerkleTreeError::NodeBytesExceeded { limit: 0, size },
                MerkleTreeError::NodeBytesExceeded { .. },
            ) => assert!(size > 0),
            (actual, expected) => assert_eq!(actual.to_string(), expected.to_string()),
        }
    }
}

#[tokio::test]
async fn walk_path_rejects_invalid_crossing_and_malformed_objects() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let trees = MerkleTreeStore::new(&objects);
    let root = trees
        .apply_edits(
            trees.empty_root().await.unwrap(),
            &[edit(&["file"], Some("x"))],
        )
        .await
        .unwrap();
    assert!(matches!(
        trees
            .walk_path(root, &["file".into(), "child".into()], TreePathLimits::default())
            .await,
        Err(MerkleTreeError::FileInPath(path)) if path == "file"
    ));
    assert!(
        trees
            .walk_path(root, &["bad/name".into()], TreePathLimits::default())
            .await
            .is_err()
    );
    let missing = wheat_core::TreeId::from_digest([7; 32]);
    assert!(matches!(
        trees
            .walk_path(missing, &[], TreePathLimits::default())
            .await,
        Err(MerkleTreeError::Store(ObjectStoreError::Missing(_)))
    ));

    let repository = wheat_core::Repository::new(BTreeMap::new());
    let state = repository.canonical_closure().state;
    objects
        .put_if_absent(state.id.into(), &state.canonical_bytes())
        .await
        .unwrap();
    let wrong_kind = wheat_core::TreeId::from_digest(*state.id.as_bytes());
    assert!(matches!(
        trees
            .walk_path(wrong_kind, &[], TreePathLimits::default())
            .await,
        Err(MerkleTreeError::Store(ObjectStoreError::WrongKind { .. }))
    ));

    let malformed_id = wheat_core::TreeId::from_digest([8; 32]);
    assert!(matches!(
        MerkleTreeStore::new(&MalformedTreeStore)
            .walk_path(malformed_id, &[], TreePathLimits::default())
            .await,
        Err(MerkleTreeError::Decode(_))
    ));
}

#[tokio::test]
async fn select_paths_shares_ancestors_deduplicates_and_skips_unrelated_subtrees() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let trees = MerkleTreeStore::new(&objects);
    let mut edits = Vec::new();
    for index in 0..1_000 {
        edits.push(edit(
            &[&format!("unrelated-{index:04}"), "deep", "leaf"],
            Some("outside"),
        ));
    }
    edits.extend([
        edit(&["selected", "a"], Some("a")),
        edit(&["selected", "nested", "b"], Some("b")),
        edit(&["selected", "nested", "c"], Some("c")),
    ]);
    let root = trees
        .apply_edits(trees.empty_root().await.unwrap(), &edits)
        .await
        .unwrap();

    let objects = CountingStore::new(objects);
    let trees = MerkleTreeStore::new(&objects);
    let files = trees
        .select_paths(
            root,
            &["selected".into(), "selected/nested/b".into()],
            TreeSelectionLimits::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        ["selected/a", "selected/nested/b", "selected/nested/c"]
    );
    // Only root, selected, and selected/nested are read. None of the thousand
    // unrelated directory roots (or their descendants) are fetched.
    assert_eq!(objects.reads(), 3);
}

#[tokio::test]
async fn select_paths_reports_each_missing_selector_even_under_selected_directory() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let trees = MerkleTreeStore::new(&objects);
    let root = trees
        .apply_edits(
            trees.empty_root().await.unwrap(),
            &[edit(&["dir", "file"], Some("file"))],
        )
        .await
        .unwrap();

    for selectors in [
        vec![String::from("absent")],
        vec![String::from("dir"), String::from("dir/absent")],
        vec![String::from("dir/file"), String::from("later")],
    ] {
        let expected = selectors.last().unwrap().clone();
        assert!(matches!(
            trees
                .select_paths(root, &selectors, TreeSelectionLimits::default())
                .await,
            Err(MerkleTreeError::MissingSelector(path)) if path == expected
        ));
    }
}

#[tokio::test]
async fn select_paths_enforces_aggregate_and_selector_limits() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let trees = MerkleTreeStore::new(&objects);
    let root = trees
        .apply_edits(
            trees.empty_root().await.unwrap(),
            &[
                edit(&["dir", "a"], Some("a")),
                edit(&["dir", "nested", "b"], Some("b")),
            ],
        )
        .await
        .unwrap();
    let exact = TreeSelectionLimits {
        max_files: 2,
        max_nodes: 3,
        max_entries: 4,
        max_path_bytes: 17,
        max_depth: 2,
        max_node_bytes: 16 * 1024 * 1024,
        max_buffered_entries: 2,
        max_selectors: 2,
        max_selector_path_bytes: 12,
        max_selector_components: 3,
    };
    assert_eq!(
        trees
            .select_paths(root, &["dir".into()], exact)
            .await
            .unwrap()
            .len(),
        2
    );

    for limits in [
        TreeSelectionLimits {
            max_files: 1,
            ..exact
        },
        TreeSelectionLimits {
            max_nodes: 2,
            ..exact
        },
        TreeSelectionLimits {
            max_entries: 3,
            ..exact
        },
        TreeSelectionLimits {
            max_path_bytes: 16,
            ..exact
        },
        TreeSelectionLimits {
            max_depth: 1,
            ..exact
        },
        TreeSelectionLimits {
            max_node_bytes: 0,
            ..exact
        },
        TreeSelectionLimits {
            max_buffered_entries: 1,
            ..exact
        },
    ] {
        assert!(
            trees
                .select_paths(root, &["dir".into()], limits)
                .await
                .is_err()
        );
    }
    assert!(matches!(
        trees
            .select_paths(
                root,
                &["dir".into(), "dir/a".into()],
                TreeSelectionLimits {
                    max_selectors: 1,
                    ..exact
                }
            )
            .await,
        Err(MerkleTreeError::SelectorLimitExceeded { limit: 1 })
    ));
    assert!(matches!(
        trees
            .select_paths(
                root,
                &["dir".into(), "dir/a".into()],
                TreeSelectionLimits {
                    max_selector_path_bytes: 7,
                    ..exact
                }
            )
            .await,
        Err(MerkleTreeError::SelectorPathBytesExceeded { size: 8, limit: 7 })
    ));
    assert_eq!(
        trees
            .select_paths(root, &["dir".into(), "dir/a".into()], exact)
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(matches!(
        trees
            .select_paths(
                root,
                &["dir".into(), "dir/a".into()],
                TreeSelectionLimits {
                    max_selector_components: 2,
                    ..exact
                }
            )
            .await,
        Err(MerkleTreeError::SelectorComponentLimitExceeded { limit: 2 })
    ));
    assert!(matches!(
        trees
            .select_paths(root, &["dir/a".into(), "dir".into()], exact)
            .await,
        Err(MerkleTreeError::NonCanonicalSelectors)
    ));
}

#[tokio::test]
async fn walk_files_propagates_missing_and_decode_errors() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let trees = MerkleTreeStore::new(&objects);
    let missing = wheat_core::TreeId::from_digest([7; 32]);
    assert!(matches!(
        trees.walk_files(missing, TreeWalkLimits::default()).await,
        Err(MerkleTreeError::Store(ObjectStoreError::Missing(_)))
    ));

    let malformed_id = wheat_core::TreeId::from_digest([8; 32]);
    assert!(matches!(
        MerkleTreeStore::new(&MalformedTreeStore)
            .walk_files(malformed_id, TreeWalkLimits::default())
            .await,
        Err(MerkleTreeError::Decode(_))
    ));
}

async fn collect_diff<S: ImmutableObjectStore>(
    trees: &MerkleTreeStore<'_, S>,
    old: wheat_core::TreeId,
    new: wheat_core::TreeId,
    limits: TreeDiffLimits,
) -> Result<Vec<TreeDiff>, MerkleTreeError> {
    let mut cursor = trees.diff_cursor(old, new, limits);
    let mut changes = Vec::new();
    while let Some(change) = cursor.next().await? {
        changes.push(change);
    }
    Ok(changes)
}

#[tokio::test]
async fn diff_identical_root_performs_zero_reads() {
    let directory = tempfile::tempdir().unwrap();
    let objects = CountingStore::new(LooseObjectStore::new(directory.path()));
    let trees = MerkleTreeStore::new(&objects);
    let root = trees.empty_root().await.unwrap();
    let before = objects.reads();
    assert!(
        collect_diff(&trees, root, root, TreeDiffLimits::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(objects.reads(), before);
}

#[tokio::test]
async fn diff_is_lexical_and_covers_add_remove_modify_and_kind_transitions() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let trees = MerkleTreeStore::new(&objects);
    let empty = trees.empty_root().await.unwrap();
    let old = trees
        .apply_edits(
            empty,
            &[
                edit(&["a"], Some("file-to-dir")),
                edit(&["b", "old"], Some("dir-to-file")),
                edit(&["c"], Some("removed")),
                edit(&["m"], Some("old")),
                edit(&["same"], Some("same")),
            ],
        )
        .await
        .unwrap();
    let new = trees
        .apply_edits(
            empty,
            &[
                edit(&["a", "new"], Some("file-to-dir")),
                edit(&["b"], Some("dir-to-file")),
                edit(&["d"], Some("added")),
                edit(&["m"], Some("new")),
                edit(&["same"], Some("same")),
            ],
        )
        .await
        .unwrap();
    let paths = collect_diff(&trees, old, new, TreeDiffLimits::default())
        .await
        .unwrap()
        .into_iter()
        .map(|change| (change.path, change.old, change.new))
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            ("a".into(), Some(file("file-to-dir")), None),
            ("a/new".into(), None, Some(file("file-to-dir"))),
            ("b".into(), None, Some(file("dir-to-file"))),
            ("b/old".into(), Some(file("dir-to-file")), None),
            ("c".into(), Some(file("removed")), None),
            ("d".into(), None, Some(file("added"))),
            ("m".into(), Some(file("old")), Some(file("new"))),
        ]
    );
}

#[tokio::test]
async fn diff_prunes_shared_subtrees_and_obeys_all_limits() {
    let directory = tempfile::tempdir().unwrap();
    let objects = CountingStore::new(LooseObjectStore::new(directory.path()));
    let trees = MerkleTreeStore::new(&objects);
    let empty = trees.empty_root().await.unwrap();
    let old = trees
        .apply_edits(
            empty,
            &[
                edit(&["shared", "one"], Some("one")),
                edit(&["shared", "two"], Some("two")),
                edit(&["changed", "leaf"], Some("old")),
            ],
        )
        .await
        .unwrap();
    let new = trees
        .apply_edits(old, &[edit(&["changed", "leaf"], Some("new"))])
        .await
        .unwrap();
    let before = objects.reads();
    assert_eq!(
        collect_diff(&trees, old, new, TreeDiffLimits::default())
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        objects.reads() - before,
        4,
        "two roots and changed children"
    );

    let old_node = trees.read_node(old).await.unwrap();
    let new_node = trees.read_node(new).await.unwrap();
    let old_changed = match old_node.entries["changed"] {
        DirectoryEntry::Directory(id) => id,
        _ => panic!("changed is a directory"),
    };
    let new_changed = match new_node.entries["changed"] {
        DirectoryEntry::Directory(id) => id,
        _ => panic!("changed is a directory"),
    };
    let mut max_node_bytes = 0;
    for id in [old, new, old_changed, new_changed] {
        max_node_bytes =
            max_node_bytes.max(objects.get(id.into()).await.unwrap().canonical_bytes.len());
    }
    let exact = TreeDiffLimits {
        max_changes: 1,
        max_nodes: 4,
        max_entries: 6,
        max_path_bytes: 12,
        max_depth: 1,
        max_node_bytes,
        max_buffered_entries: 4,
    };
    assert_eq!(
        collect_diff(&trees, old, new, exact).await.unwrap().len(),
        1
    );

    for (limits, expected) in [
        (
            TreeDiffLimits {
                max_changes: 0,
                ..TreeDiffLimits::default()
            },
            "change",
        ),
        (
            TreeDiffLimits {
                max_nodes: 3,
                ..TreeDiffLimits::default()
            },
            "node",
        ),
        (
            TreeDiffLimits {
                max_entries: 5,
                ..TreeDiffLimits::default()
            },
            "entry",
        ),
        (
            TreeDiffLimits {
                max_path_bytes: 11,
                ..TreeDiffLimits::default()
            },
            "path",
        ),
        (
            TreeDiffLimits {
                max_depth: 0,
                ..TreeDiffLimits::default()
            },
            "depth",
        ),
        (
            TreeDiffLimits {
                max_node_bytes: 1,
                ..TreeDiffLimits::default()
            },
            "bytes",
        ),
        (
            TreeDiffLimits {
                max_buffered_entries: 3,
                ..TreeDiffLimits::default()
            },
            "buffered",
        ),
    ] {
        let mut cursor = trees.diff_cursor(old, new, limits);
        let error = loop {
            match cursor.next().await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("expected {expected} limit error"),
                Err(error) => break error,
            }
        };
        assert_eq!(cursor.next().await.unwrap(), None);
        assert!(
            matches!(
                (&error, expected),
                (MerkleTreeError::ChangeLimitExceeded { .. }, "change")
                    | (MerkleTreeError::NodeLimitExceeded { .. }, "node")
                    | (MerkleTreeError::EntryLimitExceeded { .. }, "entry")
                    | (MerkleTreeError::PathBytesExceeded { .. }, "path")
                    | (MerkleTreeError::DepthExceeded { .. }, "depth")
                    | (MerkleTreeError::NodeBytesExceeded { .. }, "bytes")
                    | (MerkleTreeError::BufferedEntriesExceeded { .. }, "buffered")
            ),
            "expected {expected}, got {error:?}"
        );
    }
}

#[tokio::test]
async fn content_cursor_streams_values_without_path_limits() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path());
    let trees = MerkleTreeStore::new(&objects);
    let value = FileValue {
        content: ContentId::from_digest([7; 32]),
        kind: FileKind::Regular,
    };
    let leaf = trees
        .write_node(
            &DirectoryNode::new([(String::from("file"), DirectoryEntry::File(value))]).unwrap(),
        )
        .await
        .unwrap();
    let mut root = leaf;
    for _ in 0..129 {
        root = trees
            .write_node(
                &DirectoryNode::new([(String::from("dir"), DirectoryEntry::Directory(root))])
                    .unwrap(),
            )
            .await
            .unwrap();
    }
    let mut cursor = trees
        .content_cursor(root, Default::default())
        .await
        .unwrap();
    assert_eq!(cursor.next().await.unwrap(), Some(value));
    assert_eq!(cursor.next().await.unwrap(), None);
}

#[tokio::test]
async fn diff_storage_and_decode_errors_exhaust_the_cursor() {
    let missing = wheat_core::TreeId::from_digest([6; 32]);
    let other = wheat_core::TreeId::from_digest([7; 32]);
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let mut missing_cursor =
        MerkleTreeStore::new(&objects).diff_cursor(missing, other, TreeDiffLimits::default());
    assert!(matches!(
        missing_cursor.next().await,
        Err(MerkleTreeError::Store(ObjectStoreError::Missing(_)))
    ));
    assert_eq!(missing_cursor.next().await.unwrap(), None);

    let mut malformed_cursor = MerkleTreeStore::new(&MalformedTreeStore).diff_cursor(
        missing,
        other,
        TreeDiffLimits::default(),
    );
    assert!(matches!(
        malformed_cursor.next().await,
        Err(MerkleTreeError::Decode(_))
    ));
    assert_eq!(malformed_cursor.next().await.unwrap(), None);
}

#[tokio::test]
async fn missing_and_wrong_kind_nodes_are_reported() {
    let directory = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(directory.path());
    let trees = MerkleTreeStore::new(&objects);
    let missing = wheat_core::TreeId::from_digest([9; 32]);
    assert!(matches!(
        trees.read_node(missing).await,
        Err(MerkleTreeError::Store(
            wheat_store::ObjectStoreError::Missing(_)
        ))
    ));

    let repository = wheat_core::Repository::new(BTreeMap::new());
    let state = repository.canonical_closure().state;
    objects
        .put_if_absent(state.id.into(), &state.canonical_bytes())
        .await
        .unwrap();
    let forged_tree = wheat_core::TreeId::from_digest(*state.id.as_bytes());
    assert!(matches!(
        trees.read_node(forged_tree).await,
        Err(MerkleTreeError::Store(
            wheat_store::ObjectStoreError::WrongKind { .. }
        ))
    ));
    assert!(!trees.contains_node(missing).await.unwrap());
    let _: ObjectId = forged_tree.into();
}
