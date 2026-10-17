use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use wheat_core::{
    ApplicationResultRecord, DirectoryEntry, EditOperation, ObjectId, decode_application_record,
    decode_content_set_node, decode_directory_node, decode_edit_record,
    decode_repository_envelope_record, decode_repository_head, decode_repository_state_record,
    decode_retained_set_node, decode_revision_record, decode_view_label_manifest_record,
    decode_view_record,
};
use wheat_store::packed_segment_reader::{self, AuthenticatedSegment, read_loose_envelope};

const MAX_OBJECT_BYTES: usize = 256 * 1024 * 1024;
const METADATA_NAMESPACE: u8 = 1;

#[derive(Default)]
pub(super) struct ContentRoles {
    pub whole: HashSet<[u8; 32]>,
    pub fragment: HashSet<[u8; 32]>,
    pub required: HashSet<[u8; 32]>,
}

impl ContentRoles {
    pub fn whole_only(&self) -> usize {
        self.whole.difference(&self.fragment).count()
    }
    pub fn fragment_only(&self) -> usize {
        self.fragment.difference(&self.whole).count()
    }
    pub fn overlap(&self) -> usize {
        self.whole.intersection(&self.fragment).count()
    }
}

struct ObjectSource {
    segments: Vec<AuthenticatedSegment>,
    locations: HashMap<[u8; 32], usize>,
    loose: HashMap<[u8; 32], PathBuf>,
}

impl ObjectSource {
    fn open(storage: &Path) -> Result<Self> {
        let mut paths = Vec::new();
        let segments = storage.join("packed/segments");
        if segments.is_dir() {
            for entry in fs::read_dir(&segments).context("read packed segments")? {
                let entry = entry?;
                if entry.file_type()?.is_file()
                    && entry.path().extension().is_some_and(|e| e == "segment")
                {
                    paths.push(entry.path());
                }
            }
        }
        paths.sort();
        // The active segment is read first, as it was when it sorted before the
        // sealed segments' directory.
        let active = storage.join("packed/active.segment");
        if active.is_file() {
            paths.insert(0, active.clone());
        }

        let mut readers = Vec::with_capacity(paths.len());
        let mut locations = HashMap::new();
        for path in paths {
            let reader = if path == active {
                packed_segment_reader::open_active_segment(&path)
            } else {
                packed_segment_reader::open_segment(fs::File::open(&path)?)
            }
            .map_err(|error| anyhow::anyhow!("authenticate {}: {error}", path.display()))?;
            let index = readers.len();
            for entry in reader.entries() {
                if entry.namespace == METADATA_NAMESPACE {
                    locations.entry(entry.digest).or_insert(index);
                }
            }
            readers.push(reader);
        }

        let mut loose = HashMap::new();
        let loose_root = storage.join("loose-objects");
        if loose_root.is_dir() {
            for bucket in fs::read_dir(loose_root)? {
                let bucket = bucket?;
                if !bucket.file_type()?.is_dir() {
                    continue;
                }
                for entry in fs::read_dir(bucket.path())? {
                    let entry = entry?;
                    if !entry.file_type()?.is_file() {
                        continue;
                    }
                    let bytes = read_loose_envelope(&entry.path()).map_err(|error| {
                        anyhow::anyhow!("decode {}: {error}", entry.path().display())
                    })?;
                    loose.insert(
                        *ObjectId::of_canonical_bytes(&bytes).as_bytes(),
                        entry.path(),
                    );
                }
            }
        }
        Ok(Self {
            segments: readers,
            locations,
            loose,
        })
    }

    fn read(&mut self, digest: [u8; 32]) -> Result<Vec<u8>> {
        if let Some(&index) = self.locations.get(&digest) {
            let value = self.segments[index]
                .read_entry_bounded(METADATA_NAMESPACE, digest, MAX_OBJECT_BYTES)
                .map_err(|error| anyhow::anyhow!("read object {}: {error}", hex(&digest)))?
                .with_context(|| format!("indexed object {} disappeared", hex(&digest)))?;
            self.segments[index].clear_block_cache();
            return Ok(value);
        }
        if let Some(path) = self.loose.get(&digest) {
            return read_loose_envelope(path)
                .map_err(|error| anyhow::anyhow!("decode {}: {error}", path.display()));
        }
        bail!("retained object {} is missing", hex(&digest))
    }
}

pub(super) fn classify(store_root: &Path) -> Result<ContentRoles> {
    // A store without an initialized head has no retained history to classify.
    let Ok(head_before) = fs::read(store_root.join("repository.head")) else {
        return Ok(ContentRoles::default());
    };
    let envelope = decode_head(&head_before)?;
    let mut source = ObjectSource::open(&store_root.join("storage"))?;
    let envelope = decode_repository_envelope_record(&source.read(envelope)?)?;
    let state = decode_repository_state_record(&source.read(*envelope.state.as_bytes())?)?;
    let labels = decode_view_label_manifest_record(&source.read(*envelope.labels.as_bytes())?)?;

    let mut views = collect_retained(&mut source, *state.retained_views.as_bytes())?;
    views.extend(labels.labels.values().map(|label| *label.view.as_bytes()));
    let revisions = collect_retained(&mut source, *state.retained_revisions.as_bytes())?;
    let applications = collect_retained(&mut source, *state.retained_applications.as_bytes())?;

    let mut roles = ContentRoles {
        required: collect_content_set(&mut source, *state.required_content.as_bytes())?,
        ..ContentRoles::default()
    };
    let mut trees = HashSet::new();
    for view in views {
        let view = decode_view_record(&source.read(view)?)?;
        trees.insert(*view.tree.as_bytes());
    }
    let mut edits = HashSet::new();
    for revision in revisions {
        let revision = decode_revision_record(&source.read(revision)?)?;
        trees.insert(*revision.resulting_tree.as_bytes());
        edits.extend(revision.edits.iter().map(|id| *id.as_bytes()));
    }
    for application in applications {
        let application = decode_application_record(&source.read(application)?)?;
        match application.result {
            ApplicationResultRecord::Clean { tree } => {
                trees.insert(*tree.as_bytes());
            }
            ApplicationResultRecord::Conflicted { partial_tree, .. } => {
                trees.insert(*partial_tree.as_bytes());
            }
        }
        edits.extend(application.applied.keys().map(|id| *id.as_bytes()));
    }

    let mut seen_trees = HashSet::new();
    while let Some(tree) = trees.iter().next().copied() {
        trees.remove(&tree);
        if !seen_trees.insert(tree) {
            continue;
        }
        for entry in decode_directory_node(&source.read(tree)?)?
            .entries
            .into_values()
        {
            match entry {
                DirectoryEntry::File(file) => {
                    roles.whole.insert(*file.content.as_bytes());
                }
                DirectoryEntry::Directory(child) => {
                    trees.insert(*child.as_bytes());
                }
            }
        }
    }
    for edit in edits {
        match decode_edit_record(&source.read(edit)?)?.operation {
            EditOperation::ExactFile {
                expected,
                replacement,
            } => {
                roles.whole.extend(
                    expected
                        .into_iter()
                        .chain(replacement)
                        .map(|v| *v.content.as_bytes()),
                );
            }
            EditOperation::GuardedText {
                before,
                expected,
                replacement,
                after,
                ..
            } => {
                roles.fragment.extend(
                    [before, expected, replacement, after]
                        .into_iter()
                        .map(|id| *id.as_bytes()),
                );
            }
        }
    }
    let head_after = fs::read(store_root.join("repository.head")).ok();
    if head_after.as_deref() != Some(head_before.as_slice()) {
        bail!("repository head changed during analysis; result would be incomplete")
    }
    Ok(roles)
}

/// One radix set node: its prefix, whether a key ends at it, its inline key suffixes
/// and its children.
type SetNode = (Vec<u8>, bool, Vec<Vec<u8>>, Vec<(u8, [u8; 32])>);

fn collect_retained(source: &mut ObjectSource, root: [u8; 32]) -> Result<HashSet<[u8; 32]>> {
    collect_set(source, root, |bytes| {
        let node = decode_retained_set_node(bytes)?;
        let children = node.children.iter();
        let children = children.map(|child| (child.edge, *child.child.as_bytes()));
        Ok((node.prefix, node.present, node.entries, children.collect()))
    })
}

fn collect_content_set(source: &mut ObjectSource, root: [u8; 32]) -> Result<HashSet<[u8; 32]>> {
    collect_set(source, root, |bytes| {
        let node = decode_content_set_node(bytes)?;
        let children = node.children.iter();
        let children = children.map(|child| (child.edge, *child.child.as_bytes()));
        Ok((node.prefix, node.present, node.entries, children.collect()))
    })
}

fn collect_set(
    source: &mut ObjectSource,
    root: [u8; 32],
    decode: impl Fn(&[u8]) -> Result<SetNode>,
) -> Result<HashSet<[u8; 32]>> {
    let mut pending = vec![(root, Vec::new())];
    let mut seen = HashSet::new();
    let mut values = HashSet::new();
    while let Some((id, mut path)) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let (prefix, present, entries, children) = decode(&source.read(id)?)?;
        path.extend_from_slice(&prefix);
        if present {
            if path.len() != 32 {
                bail!("set member has invalid length")
            }
            values.insert(path.as_slice().try_into().expect("length checked"));
        }
        for suffix in entries {
            let mut value = path.clone();
            value.extend_from_slice(&suffix);
            if value.len() != 32 {
                bail!("set member has invalid length")
            }
            values.insert(value.as_slice().try_into().expect("length checked"));
        }
        for (edge, child) in children {
            let mut child_path = path.clone();
            child_path.push(edge);
            pending.push((child, child_path));
        }
    }
    Ok(values)
}

fn decode_head(bytes: &[u8]) -> Result<[u8; 32]> {
    let head =
        decode_repository_head(bytes).context("repository.head is corrupt or unsupported")?;
    Ok(*head.envelope.as_bytes())
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
