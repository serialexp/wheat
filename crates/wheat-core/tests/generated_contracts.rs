use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
};

use proptest::prelude::*;
use wheat_core::*;

#[derive(Default)]
struct Memory(RefCell<BTreeMap<ContentId, Vec<u8>>>);

impl ContentIo for Memory {
    type Error = &'static str;

    fn resolve(&self, id: ContentId) -> Result<Vec<u8>, Self::Error> {
        self.0.borrow().get(&id).cloned().ok_or("missing")
    }

    fn store_generated(&self, id: ContentId, bytes: &[u8]) -> Result<(), Self::Error> {
        self.0.borrow_mut().insert(id, bytes.to_vec());
        Ok(())
    }
}

fn digest(byte: u8) -> [u8; 32] {
    [byte; 32]
}

fn change(byte: u8) -> ChangeId {
    let mut bytes = [0_u8; 16];
    bytes[15] = byte;
    ChangeId::from_bytes(bytes)
}

fn exact(io: &Memory, path: &str, bytes: &[u8]) -> RevisionEdits {
    let content = ContentId::of(bytes);
    io.0.borrow_mut().insert(content, bytes.to_vec());
    let edit = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content,
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn edit_encoding_roundtrips_and_identity_is_exact(
        path in "[a-z][a-z0-9]{0,31}",
        expected in prop::option::of(any::<[u8; 32]>()),
        replacement in prop::option::of(any::<[u8; 32]>()),
        executable in any::<bool>(),
    ) {
        let kind = if executable { FileKind::Executable } else { FileKind::Regular };
        let expected = expected.map(|digest| FileValue { content: ContentId::from_digest(digest), kind });
        let mut replacement = replacement.map(|digest| FileValue { content: ContentId::from_digest(digest), kind });
        if replacement == expected {
            replacement = Some(FileValue { content: ContentId::from_digest([0x5a; 32]), kind });
            if replacement == expected {
                replacement = None;
            }
        }
        let record = EditRecord::new(path, EditOperation::ExactFile { expected, replacement }).unwrap();
        let bytes = record.canonical_bytes();
        let decoded = decode_edit_record(&bytes).unwrap();
        prop_assert_eq!(&decoded, &record);
        prop_assert_eq!(decoded.canonical_bytes(), bytes.clone());
        prop_assert_eq!(record.id(), EditId::of_canonical_bytes(&bytes));
    }

    #[test]
    fn directory_encoding_is_insertion_order_independent(
        entries in prop::collection::btree_map("[a-z][a-z0-9]{0,11}", (any::<[u8; 32]>(), any::<bool>()), 0..24),
        reverse in any::<bool>(),
    ) {
        let mut entries = entries.into_iter().map(|(name, (digest, executable))| (
            name,
            DirectoryEntry::File(FileValue {
                content: ContentId::from_digest(digest),
                kind: if executable { FileKind::Executable } else { FileKind::Regular },
            }),
        )).collect::<Vec<_>>();
        let canonical = DirectoryNode::new(entries.clone()).unwrap();
        let rotation = 1.min(entries.len());
        if reverse { entries.reverse(); } else { entries.rotate_left(rotation); }
        let permuted = DirectoryNode::new(entries).unwrap();
        prop_assert_eq!(permuted.canonical_bytes(), canonical.canonical_bytes());
        prop_assert_eq!(permuted.id(), canonical.id());
        prop_assert_eq!(decode_directory_node(&canonical.canonical_bytes()).unwrap(), canonical);
    }

    #[test]
    fn radix_children_are_sorted_and_roundtrip(
        mut edges in prop::collection::btree_set(any::<u8>(), 2..32),
        prefix in prop::collection::vec(any::<u8>(), 0..16),
    ) {
        let mut children = edges.iter().enumerate().map(|(index, edge)| RevisionMapChild {
            edge: *edge,
            child: RevisionMapId::from_digest(digest(index as u8)),
        }).collect::<Vec<_>>();
        children.reverse();
        let node = RevisionMapNode::new(prefix, None, children, vec![]).unwrap();
        let actual = node.children.iter().map(|child| child.edge).collect::<Vec<_>>();
        prop_assert_eq!(actual, edges.pop_first().into_iter().chain(edges).collect::<Vec<_>>());
        let bytes = node.canonical_bytes();
        prop_assert_eq!(decode_revision_map_node(&bytes).unwrap(), node.clone());
        prop_assert_eq!(node.id(), RevisionMapId::of_canonical_bytes(&bytes));
    }

    #[test]
    fn generated_valid_dag_import_has_a_deterministic_schedule(
        parent_bits in prop::collection::vec(any::<u16>(), 1..10),
    ) {
        let io = Memory::default();
        let ids = (0..parent_bits.len()).map(|index| change(index as u8 + 1)).collect::<Vec<_>>();
        let proposals = ids.iter().enumerate().map(|(index, id)| {
            let dependencies = (0..index)
                .filter(|parent| parent_bits[index] & (1 << (parent % 16)) != 0)
                .map(|parent| ids[parent])
                .collect::<BTreeSet<_>>();
            ChangeProposal {
                id: *id,
                metadata: ChangeMetadata::default(),
                dependencies,
                revision_metadata: RevisionMetadata::default(),
                edits: exact(&io, &format!("path-{index}"), &[index as u8]),
            }
        }).collect::<Vec<_>>();

        let mut first = Repository::new(Tree::new());
        let base = first.base_view().id;
        let first_outcomes = first.import_changes(base, base, proposals.clone(), ApplicationContext::new(&io)).unwrap();
        let mut second = Repository::new(Tree::new());
        let second_outcomes = second.import_changes(base, base, proposals, ApplicationContext::new(&io)).unwrap();

        prop_assert_eq!(&first_outcomes, &second_outcomes);
        prop_assert_eq!(first.canonical_closure(), second.canonical_closure());
        first.validate().unwrap();
        for (index, id) in ids.iter().enumerate() {
            let revision = first.revision(first_outcomes[id].revision_id).unwrap();
            for parent in revision.dependencies() {
                prop_assert!(ids.iter().position(|candidate| candidate == parent).unwrap() < index);
            }
        }
    }
}
