use std::{collections::BTreeMap, sync::Arc};

use object_store::memory::InMemory;
use wheat_core::{ContentId, DirectoryEntry, ObjectId};
use wheat_store::{
    ImmutableObjectStore, LooseObjectStore, MerkleTreeError, MerkleTreeStore, ObjectStoreAdapter,
    TreeEdit,
};

fn content(value: &str) -> ContentId {
    ContentId::of(value.as_bytes())
}

fn edit(path: &[&str], value: Option<&str>) -> TreeEdit {
    TreeEdit {
        path: path
            .iter()
            .map(|component| (*component).to_owned())
            .collect(),
        content: value.map(content),
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
        Some(content("auth-v2"))
    );
    assert_eq!(
        trees
            .lookup(initial, &["src".into(), "auth.rs".into()])
            .await
            .unwrap(),
        Some(content("auth-v1"))
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
        Some(content("lib"))
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
        Some(content("value"))
    );
}

#[tokio::test]
async fn apache_memory_store_passes_merkle_tree_conformance() {
    let backend = Arc::new(InMemory::new());
    let objects = ObjectStoreAdapter::new(backend, "merkle-conformance");
    conformance(&objects).await;
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
