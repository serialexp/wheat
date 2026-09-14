use proptest::prelude::*;
use wheat_core::*;
use wheat_store::*;

fn exact(path: &str, bytes: &[u8]) -> RevisionEdits {
    let edit = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: ContentId::of(bytes),
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let id = edit.id();
    RevisionEdits {
        edits: [(id, edit)].into(),
        transition: EditTransition {
            added: [id].into(),
            ..EditTransition::default()
        },
    }
}

async fn reference_and_persisted_case(path: String, bytes: Vec<u8>, change: ChangeId) {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    content.put(&bytes).unwrap();
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let initial_repository = Repository::new(Tree::new());
    let base = initial_repository.base_view().id;
    let initial = RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&initial_repository)
        .await
        .unwrap();

    let edits = exact(&path, &bytes);
    let mut reference = initial_repository;
    let expected = reference
        .create_change_with_id(
            base,
            base,
            change,
            [],
            edits.clone(),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let proposal = ChangeProposal {
        id: change,
        metadata: ChangeMetadata::default(),
        dependencies: Default::default(),
        revision_metadata: RevisionMetadata::default(),
        edits,
    };
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let (prepared, actual) = mutator.create(initial, base, base, proposal).await.unwrap();
    assert_eq!(actual, expected);
    assert_eq!(prepared.state(), reference.canonical_closure().state.id);
    let head = mutator.publish(prepared).await.unwrap();
    let state = RepositoryReader::new(&objects, &content)
        .envelope(head.envelope)
        .await
        .unwrap()
        .state;
    let hydrated = RepositoryReader::new(&objects, &content)
        .hydrate(state)
        .await
        .unwrap();
    assert_eq!(hydrated.canonical_closure(), reference.canonical_closure());
}

async fn loose_and_packed_case(entries: Vec<(String, [u8; 32])>) {
    let loose_dir = tempfile::tempdir().unwrap();
    let packed_dir = tempfile::tempdir().unwrap();
    let loose = LooseObjectStore::new(loose_dir.path());
    let packed = PackedObjectStore::open(packed_dir.path()).unwrap();
    let node = DirectoryNode::new(entries.into_iter().map(|(name, digest)| {
        (
            name,
            DirectoryEntry::File(FileValue {
                content: ContentId::from_digest(digest),
                kind: FileKind::Regular,
            }),
        )
    }))
    .unwrap();
    let bytes = node.canonical_bytes();
    let id: ObjectId = node.id().into();
    loose.put_if_absent(id, &bytes).await.unwrap();
    packed.put_if_absent(id, &bytes).await.unwrap();
    assert_eq!(loose.get(id).await.unwrap(), packed.get(id).await.unwrap());
    assert_eq!(
        loose.exists_many(&[id]).await.unwrap(),
        packed.exists_many(&[id]).await.unwrap()
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn generated_reference_and_persisted_mutations_match(
        path in "[a-z][a-z0-9]{0,15}",
        bytes in prop::collection::vec(any::<u8>(), 0..128),
        change_bytes in any::<[u8; 16]>(),
    ) {
        tokio::runtime::Runtime::new().unwrap().block_on(reference_and_persisted_case(path, bytes, ChangeId::from_bytes(change_bytes)));
    }

    #[test]
    fn generated_loose_and_packed_objects_have_read_parity(
        entries in prop::collection::btree_map("[a-z][a-z0-9]{0,11}", any::<[u8; 32]>(), 0..24),
    ) {
        tokio::runtime::Runtime::new().unwrap().block_on(loose_and_packed_case(entries.into_iter().collect()));
    }
}
