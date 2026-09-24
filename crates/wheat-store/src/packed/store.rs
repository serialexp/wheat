//! Public object/content adapters backed by immutable packed segments.
use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;
use thiserror::Error;
use wheat_core::{
    ContentId, ObjectId, PhysicalContentGeneration, RepositoryEnvelopeId, validate_canonical_object,
};

use super::{
    catalog::{CatalogError, LooseMetadataRecord, MetadataRepackInput, PackedCatalog},
    generation_reader::GenerationReader,
    segment::{CONTENT_NAMESPACE, METADATA_NAMESPACE, SegmentRecord},
    trace::{PackedStoreTrace, PackedStoreTraceCounters},
};
use crate::content_store::ContentBackend;
use crate::{
    ContentGenerationIndex, ContentGenerationManifest, ContentRepackLimits, ContentRepackPlan,
    ContentStore, ContentStoreError, Durability, ImmutableObjectStore, LocalPublicationStore,
    LooseObjectStore, ObjectKind, ObjectStoreError, PreparedContentGeneration, PublicationStore,
    RepositoryHead, RepositoryReader, StoredObject, content_generation_index_path,
    content_generation_shard_manifest_path, content_generation_shard_pack_path,
    content_repack::{plan_content_repack, prepare_content_repack},
    metadata_repack::{HistoryPlan, HistoryPlanLimits, MetadataDeltaPlan, plan_history_deltas},
    read_content_generation,
};

const DEFAULT_LOOSE_THRESHOLD: usize = 128 * 1024;
const DEFAULT_REPACK_MAX_RECORDS: usize = 1_000_000;
const DEFAULT_REPACK_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct RepackOptions {
    /// Hard bound on authenticated packed records inventoried in one run.
    pub max_records: usize,
    /// Approximate uncompressed logical bytes per replacement segment.
    pub target_segment_bytes: u64,
    pub content_limits: ContentRepackLimits,
}

impl Default for RepackOptions {
    fn default() -> Self {
        Self {
            max_records: DEFAULT_REPACK_MAX_RECORDS,
            target_segment_bytes: DEFAULT_REPACK_SEGMENT_BYTES,
            content_limits: ContentRepackLimits::default(),
        }
    }
}

#[derive(Debug, Error)]
pub enum RepackError {
    #[error("repack options require non-zero limits")]
    InvalidOptions,
    #[error("repack is unsupported for deferred storage or pending maintenance")]
    Unsupported,
    #[error("repack inventory exceeds its {limit} record limit")]
    InventoryLimit { limit: usize },
    #[error("repack plan is stale; logical or physical authority changed")]
    StaleAuthority,
    #[error("repack repository authority failed: {0}")]
    Authority(String),
    #[error(transparent)]
    Content(#[from] ContentStoreError),
    #[error("repack authority is corrupt: {0}")]
    Corrupt(String),
    #[error("repack I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug)]
pub struct RepackPlan {
    physical_authority: super::catalog::CatalogPhysicalAuthority,
    repository_head: RepositoryHead,
    options: RepackOptions,
    prepared_content: ContentRepackPlan,
    content_before: ContentGenerationStats,
    metadata_deltas: MetadataDeltaPlan,
    loose_metadata: Vec<LooseMetadataRecord>,
    pub estimated_replacement_structural_bytes: u64,
    pub estimated_replacement_content_bytes: u64,
    pub estimated_locator_manifest_bytes: u64,
    pub estimated_temporary_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepackReport {
    pub no_op: bool,
    pub physical_generation_before: u64,
    pub physical_generation_after: u64,
    pub segments_before: usize,
    pub segments_after: usize,
    pub physical_bytes_before: u64,
    pub physical_bytes_after: u64,
    pub repository_generation: u64,
    pub envelope_before: RepositoryEnvelopeId,
    pub envelope_after: RepositoryEnvelopeId,
    pub structural_record_count: u64,
    pub content_record_count: u64,
    pub full_content_before: u64,
    pub full_content_after: u64,
    pub delta_content_before: u64,
    pub delta_content_after: u64,
    pub max_delta_depth_before: u8,
    pub max_delta_depth_after: u8,
    /// Metadata records stored as in-segment deltas after the repack.
    pub metadata_delta_records: u64,
    pub max_metadata_delta_depth: u8,
    /// Loose metadata objects moved into segments (their files are removed).
    pub loose_metadata_absorbed: u64,
    pub estimated_temporary_bytes: u64,
    pub bytes_written: u64,
    pub bytes_retired: u64,
    pub validated: bool,
}

#[derive(Clone)]
pub struct PackedObjectStore {
    engine: Arc<PackedEngine>,
}

struct PackedEngine {
    catalog: Arc<Mutex<PackedCatalog>>,
    generation_reader: Mutex<Option<GenerationReader>>,
    objects: LooseObjectStore,
    content: ContentStore,
    root: PathBuf,
    content_root: PathBuf,
    loose_threshold: usize,
    durability: Durability,
    trace: Option<Arc<PackedStoreTraceCounters>>,
}

impl fmt::Debug for PackedObjectStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PackedObjectStore")
            .field("loose_threshold", &self.engine.loose_threshold)
            .finish_non_exhaustive()
    }
}

impl PackedEngine {
    /// Runs `lookup` against the published content generation's reader, or returns
    /// `None` when no generation is published. The catalog lock is held throughout so a
    /// repack on this handle cannot retire the generation's pack mid-lookup.
    fn with_generation<T>(
        &self,
        id: ContentId,
        lookup: impl FnOnce(&mut GenerationReader) -> Result<Option<T>, ContentStoreError>,
    ) -> Result<Option<T>, ContentStoreError> {
        let catalog = self.catalog.lock().map_err(|_| {
            ContentStoreError::Io(io::Error::other("packed catalog mutex poisoned"))
        })?;
        let generation = catalog.content_generation();
        if generation.generation == 0 && generation.manifest == [0; 32] {
            return Ok(None);
        }
        let mut cached = self.generation_reader.lock().map_err(|_| {
            ContentStoreError::Io(io::Error::other("content generation reader mutex poisoned"))
        })?;
        if cached
            .as_ref()
            .is_none_or(|reader| !reader.matches(generation))
        {
            *cached = None;
            *cached = Some(
                GenerationReader::open(&self.root, generation)
                    .map_err(|reason| ContentStoreError::InvalidFormat { id, reason })?,
            );
        }
        let result = lookup(cached.as_mut().expect("generation reader initialized"));
        drop(cached);
        drop(catalog);
        result
    }
}

impl fmt::Debug for PackedEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PackedEngine")
            .field("loose_threshold", &self.loose_threshold)
            .finish_non_exhaustive()
    }
}

impl PackedObjectStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ObjectStoreError> {
        Self::open_with_options(root, DEFAULT_LOOSE_THRESHOLD, Durability::Durable, false)
    }

    /// Opens storage for an isolated transaction whose entire directory remains
    /// invisible until [`crate::sync_filesystem`] and atomic installation.
    pub fn open_deferred(root: impl AsRef<Path>) -> Result<Self, ObjectStoreError> {
        Self::open_with_options(root, DEFAULT_LOOSE_THRESHOLD, Durability::Deferred, false)
    }

    /// Opens deferred storage with constant-memory diagnostic counters enabled.
    pub fn open_deferred_traced(
        root: impl AsRef<Path>,
    ) -> Result<(Self, PackedStoreTrace), ObjectStoreError> {
        let store =
            Self::open_with_options(root, DEFAULT_LOOSE_THRESHOLD, Durability::Deferred, true)?;
        let trace = PackedStoreTrace {
            counters: Arc::downgrade(store.engine.trace.as_ref().expect("trace was enabled")),
        };
        Ok((store, trace))
    }

    pub fn open_with_loose_threshold(
        root: impl AsRef<Path>,
        loose_threshold: usize,
    ) -> Result<Self, ObjectStoreError> {
        Self::open_with_options(root, loose_threshold, Durability::Durable, false)
    }

    fn open_with_options(
        root: impl AsRef<Path>,
        loose_threshold: usize,
        durability: Durability,
        collect_trace: bool,
    ) -> Result<Self, ObjectStoreError> {
        let root = root.as_ref();
        let trace = collect_trace.then(|| Arc::new(PackedStoreTraceCounters::default()));
        let catalog = if let Some(trace) = trace.clone() {
            PackedCatalog::open_with_trace(root.join("packed"), durability, trace)
        } else {
            PackedCatalog::open(root.join("packed"), durability)
        }
        .map_err(object_catalog_error)?;
        let content_generation = catalog.content_generation();
        let content_manifest = validate_content_generation(root, content_generation)
            .map_err(|error| ObjectStoreError::Integrity(error.to_string()))?;
        // The catalog's process lock remains held by `catalog`. Cleanup follows
        // recovery and validation, but never mutates catalog authority.
        retire_repacked_content(root, content_generation, content_manifest.as_ref())
            .map_err(|error| ObjectStoreError::Integrity(error.to_string()))?;
        drop(content_manifest);
        Ok(Self {
            engine: Arc::new(PackedEngine {
                catalog: Arc::new(Mutex::new(catalog)),
                generation_reader: Mutex::new(None),
                objects: LooseObjectStore::with_durability(root.join("loose-objects"), durability),
                content: ContentStore::with_durability(root.join("loose-content"), durability),
                root: root.to_path_buf(),
                content_root: root.join("loose-content"),
                loose_threshold,
                durability,
                trace,
            }),
        })
    }

    #[must_use]
    pub fn content_store(&self) -> ContentStore {
        ContentStore::with_backend(
            self.engine.content_root.clone(),
            self.engine.clone(),
            self.engine.durability,
        )
    }

    /// Explicitly closes every packed-store file handle and releases its process lock.
    ///
    /// Closing consumes this handle and fails while another object-store clone or a content
    /// adapter still references the same engine. Callers performing a terminal filesystem sync
    /// must drop those handles first so close-time SQLite errors are observable before the sync.
    #[cfg(test)]
    pub(crate) fn test_fail_close_before_finalize(&self) -> Result<(), ObjectStoreError> {
        self.catalog()?
            .test_set_close_failpoint(super::catalog::CloseFailpoint::BeforeFinalize);
        Ok(())
    }

    pub fn close(self) -> Result<(), ObjectStoreError> {
        self.close_with_trace().map(|_| ())
    }

    /// Closes the store and returns its final trace snapshot when tracing was enabled.
    pub fn close_with_trace(
        self,
    ) -> Result<Option<super::PackedStoreTraceSnapshot>, ObjectStoreError> {
        let engine = Arc::try_unwrap(self.engine).map_err(|_| {
            ObjectStoreError::Backend(
                "StillReferenced: packed object/content store handles remain live".into(),
            )
        })?;
        let catalog = Arc::try_unwrap(engine.catalog)
            .map_err(|_| {
                ObjectStoreError::Backend(
                    "StillReferenced: packed catalog handle remains live".into(),
                )
            })?
            .into_inner()
            .map_err(|_| ObjectStoreError::Backend("packed catalog mutex poisoned".into()))?;
        catalog.close().map_err(object_catalog_error)?;
        Ok(engine.trace.map(|trace| trace.snapshot()))
    }

    /// Plan a whole-repository repack against exact authenticated logical and
    /// physical authority. The returned plan is intentionally single-use.
    pub async fn plan_repack(
        &self,
        publications: &impl PublicationStore,
        options: RepackOptions,
    ) -> Result<RepackPlan, RepackError> {
        if options.max_records == 0 || options.target_segment_bytes == 0 {
            return Err(RepackError::InvalidOptions);
        }
        let repository_head = publications
            .read()
            .map_err(|error| RepackError::Authority(error.to_string()))?;
        let content = self.content_store();
        let reader = RepositoryReader::new(self, &content);
        let envelope = reader
            .validate_envelope(repository_head.envelope)
            .await
            .map_err(|error| RepackError::Authority(error.to_string()))?;
        let (physical_authority, segments, physical_bytes, records, mut inventory) = {
            let mut catalog = self
                .catalog()
                .map_err(|error| RepackError::Corrupt(error.to_string()))?;
            let authority = catalog.physical_authority().map_err(repack_catalog_error)?;
            let (segments, bytes, records) =
                catalog.physical_stats().map_err(repack_catalog_error)?;
            let inventory = catalog.packed_content_ids().map_err(repack_catalog_error)?;
            (authority, segments, bytes, records, inventory)
        };
        let loose_ids = list_loose_content_ids(&self.engine.content_root)?;
        for id in &loose_ids {
            crate::content_store::resolve_delta_chain(
                *id,
                options.content_limits.max_record_logical_bytes,
                |current| {
                    if current == *id {
                        self.engine.content.read_local_raw_envelope(current)
                    } else {
                        self.engine.get_raw_envelope(current)
                    }
                },
            )?;
        }
        inventory.extend(loose_ids);
        let current_manifest =
            read_content_manifest(&self.engine.root, physical_authority.content)?;
        // The selected generation may be the sole physical home of an ID after
        // redundant segment copies are retired. Its authenticated inventory is
        // authoritative for every subsequent repack, not only this generation.
        if let Some(manifest) = &current_manifest {
            inventory.extend(manifest.entries.iter().map(|entry| entry.id));
        }
        inventory.sort_unstable();
        inventory.dedup();
        // `validate_envelope` verified the whole state, so history planning
        // can read its records directly instead of hydrating every view.
        let HistoryPlan {
            metadata: metadata_deltas,
            content: content_history,
        } = plan_history_deltas(
            self,
            envelope.state,
            HistoryPlanLimits {
                metadata_proposals: options.max_records,
                content_pairs: options.content_limits.max_history_pairs,
                content_tips: options.content_limits.max_content_ids,
            },
        )
        .await
        .map_err(|error| RepackError::Corrupt(error.to_string()))?;
        let loose_metadata = self.loose_metadata_inventory()?;
        let prepared_content = plan_content_repack(
            &self.content_store(),
            current_manifest.as_ref(),
            crate::ContentRepackInput {
                content_ids: inventory,
                tips: content_history.tips.into_iter().collect(),
                history: content_history.pairs.into_iter().collect(),
            },
            options.content_limits,
        )
        .map_err(|error| RepackError::Corrupt(error.to_string()))?;
        let content_before = ContentGenerationStats::of(current_manifest.as_ref())?;
        drop(current_manifest);
        let content_bytes = match &prepared_content {
            ContentRepackPlan::NoOp { .. } => 0,
            ContentRepackPlan::Replace(analysis) => analysis.estimated_bytes(),
        };
        let structural_bytes = loose_metadata
            .iter()
            .try_fold(physical_bytes, |sum, record| {
                sum.checked_add(record.logical_length)
                    .ok_or_else(|| RepackError::Corrupt("structural estimate overflow".into()))
            })?;
        let locator_bytes = records
            .checked_mul(80)
            .ok_or_else(|| RepackError::Corrupt("locator estimate overflow".into()))?;
        let temporary = structural_bytes
            .checked_add(content_bytes)
            .and_then(|value| value.checked_add(locator_bytes))
            .ok_or_else(|| RepackError::Corrupt("temporary estimate overflow".into()))?;
        let _ = segments;
        Ok(RepackPlan {
            physical_authority,
            repository_head,
            options,
            prepared_content,
            content_before,
            metadata_deltas,
            loose_metadata,
            estimated_replacement_structural_bytes: structural_bytes,
            estimated_replacement_content_bytes: content_bytes,
            estimated_locator_manifest_bytes: locator_bytes,
            estimated_temporary_bytes: temporary,
        })
    }

    pub fn execute_repack(
        &self,
        publications: &LocalPublicationStore,
        plan: RepackPlan,
    ) -> Result<RepackReport, RepackError> {
        publications.with_repack_authority(plan.repository_head, || self.execute_bound_repack(plan))
    }

    fn execute_bound_repack(&self, plan: RepackPlan) -> Result<RepackReport, RepackError> {
        // Bind authority before expensive generation, then generate/stage only
        // after the caller's capacity gate has admitted execution.
        {
            let catalog = self
                .catalog()
                .map_err(|error| RepackError::Corrupt(error.to_string()))?;
            if catalog.physical_authority().map_err(repack_catalog_error)?
                != plan.physical_authority
            {
                return Err(RepackError::StaleAuthority);
            }
        }
        let RepackPlan {
            physical_authority,
            repository_head,
            options,
            prepared_content,
            content_before,
            metadata_deltas,
            loose_metadata,
            estimated_replacement_content_bytes,
            estimated_temporary_bytes,
            ..
        } = plan;
        // Each manifest copy is dropped as soon as the next step owns one: the
        // prepared generation is staged, then released before the staged
        // generation is read back and authenticated.
        let replacement_content = match prepared_content {
            ContentRepackPlan::NoOp { .. } => {
                if physical_authority.content == super::catalog::no_content_generation() {
                    return Err(RepackError::Corrupt(
                        "content without a selected generation cannot produce a no-op content plan"
                            .into(),
                    ));
                }
                physical_authority.content
            }
            ContentRepackPlan::Replace(analysis) => {
                let prepared = prepare_content_repack(&self.content_store(), analysis)
                    .map_err(|error| RepackError::Corrupt(error.to_string()))?;
                stage_content_generation(&self.engine.root, &prepared)?
            }
        };
        let mut catalog = self
            .catalog()
            .map_err(|error| RepackError::Corrupt(error.to_string()))?;
        // Authenticate the staged replacement before the catalog may omit segment
        // content records. The manifest passed below is capability data derived
        // from this full pack scan, never a caller-supplied generation token.
        let validated_content =
            validate_content_generation(&self.engine.root, replacement_content)?;
        let loose_objects = &self.engine.objects;
        let mut read_loose = |digest: [u8; 32]| {
            loose_objects
                .read_verified(ObjectId::from_digest(digest))
                .map(|object| object.canonical_bytes.to_vec())
                .map_err(|_| CatalogError::Corrupt("loose metadata vanished or changed"))
        };
        let report = match catalog.repack_structural(
            super::catalog::CatalogRepackOptions {
                max_records: options.max_records,
                target_segment_bytes: options.target_segment_bytes,
            },
            replacement_content,
            validated_content.as_ref().map(|manifest| {
                super::catalog::ValidatedContentGeneration::after_pack_verification(
                    replacement_content,
                    manifest,
                )
            }),
            physical_authority,
            MetadataRepackInput {
                bases: &metadata_deltas.bases,
                loose: &loose_metadata,
                read_loose: &mut read_loose,
            },
        ) {
            Ok(report) => report,
            Err(error) => {
                drop(catalog);
                // Stale authority is known to precede combined-head publication;
                // remove only this authenticated staged generation. Other errors
                // may be post-commit interruption and are recovered on reopen.
                if matches!(error, CatalogError::RepackStaleAuthority) {
                    remove_staged_content_generation(&self.engine.root, replacement_content)?;
                }
                return Err(repack_catalog_error(error));
            }
        };
        // This runs while `catalog` still holds the process-exclusive catalog
        // lock. It intentionally does not alter catalog authority; failures are
        // returned after the new head is durable and retried by `open`.
        retire_repacked_content(
            &self.engine.root,
            replacement_content,
            validated_content.as_ref(),
        )?;
        let content_after = ContentGenerationStats::of(validated_content.as_ref())?;
        drop(validated_content);
        // Same lock and timing: every absorbed object is in the durable head's
        // segments, so its loose file is only a redundant copy now.
        let absorbed = report
            .absorbed_loose
            .iter()
            .map(|digest| ObjectId::from_digest(*digest))
            .collect::<Vec<_>>();
        self.engine
            .objects
            .retire(&absorbed)
            .map_err(|error| RepackError::Corrupt(error.to_string()))?;
        // The old generation's packs are retired only when a new one replaced it.
        let content_bytes_retired = if replacement_content == physical_authority.content {
            0
        } else {
            content_before.bytes
        };
        Ok(RepackReport {
            no_op: report.no_op,
            physical_generation_before: report.generation_before,
            physical_generation_after: report.generation_after,
            segments_before: report.segments_before,
            segments_after: report.segments_after,
            physical_bytes_before: report
                .physical_bytes_before
                .checked_add(content_before.bytes)
                .ok_or_else(|| RepackError::Corrupt("report byte count overflow".into()))?,
            physical_bytes_after: report
                .physical_bytes_after
                .checked_add(content_after.bytes)
                .ok_or_else(|| RepackError::Corrupt("report byte count overflow".into()))?,
            repository_generation: repository_head.generation,
            envelope_before: repository_head.envelope,
            envelope_after: repository_head.envelope,
            structural_record_count: report.structural_records,
            content_record_count: content_after
                .full
                .checked_add(content_after.delta)
                .ok_or_else(|| RepackError::Corrupt("report record count overflow".into()))?,
            full_content_before: content_before.full,
            full_content_after: content_after.full,
            delta_content_before: content_before.delta,
            delta_content_after: content_after.delta,
            max_delta_depth_before: content_before.max_depth,
            max_delta_depth_after: content_after.max_depth,
            metadata_delta_records: report.delta_records,
            max_metadata_delta_depth: report.max_delta_depth,
            loose_metadata_absorbed: absorbed.len() as u64,
            estimated_temporary_bytes,
            bytes_written: report
                .bytes_written
                .checked_add(estimated_replacement_content_bytes)
                .ok_or_else(|| RepackError::Corrupt("report written byte count overflow".into()))?,
            bytes_retired: report
                .bytes_retired
                .checked_add(content_bytes_retired)
                .ok_or_else(|| RepackError::Corrupt("report retired byte count overflow".into()))?,
            validated: true,
        })
    }

    /// Every loose metadata object, verified, as repack inventory.
    fn loose_metadata_inventory(&self) -> Result<Vec<LooseMetadataRecord>, RepackError> {
        let loose = &self.engine.objects;
        let ids = loose
            .list_ids()
            .map_err(|error| RepackError::Corrupt(error.to_string()))?;
        let mut records = Vec::with_capacity(ids.len());
        for id in ids {
            let object = loose
                .read_verified(id)
                .map_err(|error| RepackError::Corrupt(error.to_string()))?;
            records.push(LooseMetadataRecord {
                digest: *id.as_bytes(),
                kind: object.kind as u16,
                logical_length: object.canonical_bytes.len() as u64,
            });
        }
        Ok(records)
    }

    fn catalog(&self) -> Result<MutexGuard<'_, PackedCatalog>, ObjectStoreError> {
        self.engine
            .catalog
            .lock()
            .map_err(|_| ObjectStoreError::Backend("packed catalog mutex poisoned".into()))
    }
}

fn repack_catalog_error(error: CatalogError) -> RepackError {
    match error {
        CatalogError::Io(error) => RepackError::Io(error),
        CatalogError::RepackUnsupported | CatalogError::Poisoned => RepackError::Unsupported,
        CatalogError::RepackInventoryLimit { limit } => RepackError::InventoryLimit { limit },
        CatalogError::RepackStaleAuthority => RepackError::StaleAuthority,
        error => RepackError::Corrupt(error.to_string()),
    }
}

fn list_loose_content_ids(root: &Path) -> Result<Vec<ContentId>, RepackError> {
    let mut ids = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(ids),
        Err(error) => return Err(error.into()),
    };
    for prefix in entries {
        let prefix = prefix?;
        if !prefix.file_type()?.is_dir() {
            continue;
        }
        let prefix_name = prefix.file_name().to_string_lossy().into_owned();
        if prefix_name.len() != 2 {
            continue;
        }
        for entry in fs::read_dir(prefix.path())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let encoded = format!("{prefix_name}{}", entry.file_name().to_string_lossy());
            if let Ok(id) = encoded.parse() {
                ids.push(id);
            }
        }
    }
    Ok(ids)
}

/// Authenticates every shard pack of the generation selected by the catalog
/// before it becomes authoritative for retirement. Each shard is one
/// sequential, bounded-buffer scan: it neither seeks per entry nor
/// materializes envelopes, so a 206k-entry / 120 MiB generation costs
/// O(pack bytes + entries) I/O and O(64 KiB) scratch. Returns the
/// authenticated manifest so callers need not read it a second time.
fn validate_content_generation(
    root: &Path,
    generation: PhysicalContentGeneration,
) -> Result<Option<ContentGenerationManifest>, RepackError> {
    let Some(manifest) = read_content_manifest(root, generation)? else {
        return Ok(None);
    };
    if manifest.generation != generation.generation {
        return Err(RepackError::Corrupt(
            "content manifest generation differs from catalog authority".into(),
        ));
    }
    let generations = root.join("content-generations");
    let mut header_bytes = [0; crate::codec::ENVELOPE_HEADER_LEN];
    let mut payload = [0u8; 64 * 1024];
    for shard in 0..manifest.shard_count() {
        let path = content_generation_shard_pack_path(&generations, manifest.generation, shard);
        let pack_length = fs::metadata(&path)
            .map_err(|error| RepackError::Corrupt(format!("content generation pack: {error}")))?
            .len();
        let mut pack = BufReader::with_capacity(64 * 1024, File::open(path)?);
        let mut expected_offset = 0u64;
        // Manifest entries are ordered by ContentId for lookups, whereas
        // envelopes are packed in dependency/production order. Sort references,
        // not bodies, before the one sequential authentication pass.
        let mut physical = manifest.shard_entries(shard).iter().collect::<Vec<_>>();
        physical.sort_unstable_by_key(|entry| entry.pack_offset);

        for entry in physical {
            // Manifest decoding proves IDs are strictly ordered and unique; this
            // offset-order scan proves exact physical coverage without gaps/overlap.
            if entry.pack_offset != expected_offset {
                return Err(RepackError::Corrupt(
                    "content generation entries are not contiguous".into(),
                ));
            }
            let length = usize::try_from(entry.physical_length).map_err(|_| {
                RepackError::Corrupt("content envelope exceeds address space".into())
            })?;
            if length < header_bytes.len() {
                return Err(RepackError::Corrupt(
                    "content envelope is shorter than its header".into(),
                ));
            }
            pack.read_exact(&mut header_bytes)?;
            let header = crate::codec::parse_envelope_header(&header_bytes).map_err(|error| {
                RepackError::Corrupt(format!("content envelope header: {error}"))
            })?;
            header
                .validate(
                    entry.physical_length,
                    crate::codec::HARD_LOGICAL_RECORD_BYTES,
                )
                .map_err(|error| {
                    RepackError::Corrupt(format!("content envelope metadata: {error}"))
                })?;
            if header.logical_length != entry.logical_length {
                return Err(RepackError::Corrupt(
                    "content envelope logical length differs from manifest".into(),
                ));
            }
            let mut digest = blake3::Hasher::new();
            digest.update(&header_bytes);
            let mut remaining = length - header_bytes.len();
            let mut delta_base = None;
            while remaining != 0 {
                let take = remaining.min(payload.len());
                pack.read_exact(&mut payload[..take])?;
                digest.update(&payload[..take]);
                if delta_base.is_none() && header.codec.is_delta() {
                    let base_len = crate::delta::BASE_REFERENCE_SIZE;
                    if take >= base_len {
                        delta_base = Some(ContentId::from_digest(
                            payload[..base_len].try_into().unwrap(),
                        ));
                    }
                }
                remaining -= take;
            }
            if digest.finalize().as_bytes() != &entry.envelope_digest {
                return Err(RepackError::Corrupt(
                    "content envelope digest differs from manifest".into(),
                ));
            }
            if delta_base != entry.delta_base {
                return Err(RepackError::Corrupt(
                    "content delta base differs from manifest".into(),
                ));
            }
            if let Some(base) = entry.delta_base {
                // A base may live in any shard of the same generation.
                let base_entry = manifest
                    .entries
                    .binary_search_by_key(&base, |candidate| candidate.id)
                    .ok()
                    .map(|index| &manifest.entries[index])
                    .ok_or_else(|| {
                        RepackError::Corrupt("content delta base is absent from generation".into())
                    })?;
                if base_entry.delta_depth.checked_add(1) != Some(entry.delta_depth) {
                    return Err(RepackError::Corrupt(
                        "content delta depth differs from its base".into(),
                    ));
                }
            } else if entry.delta_depth != 0 || header.codec.is_delta() {
                return Err(RepackError::Corrupt(
                    "content delta metadata is inconsistent".into(),
                ));
            }
            expected_offset = expected_offset
                .checked_add(entry.physical_length)
                .ok_or_else(|| RepackError::Corrupt("content pack bounds overflow".into()))?;
        }
        if expected_offset != pack_length {
            return Err(RepackError::Corrupt(
                "content generation pack length mismatch".into(),
            ));
        }
    }
    Ok(Some(manifest))
}

pub(super) fn read_content_manifest(
    root: &Path,
    generation: PhysicalContentGeneration,
) -> Result<Option<ContentGenerationManifest>, RepackError> {
    if generation.generation == 0 && generation.manifest == [0; 32] {
        return Ok(None);
    }
    read_content_generation(&root.join("content-generations"), generation)
        .map(Some)
        .map_err(|error| match error {
            crate::ContentRepackError::Io(error) => RepackError::Io(error),
            error => RepackError::Corrupt(error.to_string()),
        })
}

/// Write `prepared` as a new content generation and return its authority.
fn stage_content_generation(
    root: &Path,
    prepared: &PreparedContentGeneration,
) -> Result<PhysicalContentGeneration, RepackError> {
    let corrupt = |error: crate::ContentRepackError| RepackError::Corrupt(error.to_string());
    let manifest = &prepared.manifest;
    manifest.verify().map_err(corrupt)?;
    let directory = root.join("content-generations");
    fs::create_dir_all(&directory)?;
    remove_partial_generation(&directory, manifest.generation)?;
    let generation_dir = directory.join(manifest.generation.to_string());
    fs::create_dir_all(&generation_dir)?;
    // Production is grouped by shard, so each pack is written once, in
    // order, and made durable before the next is opened: one fsync per
    // shard, independent of the item count within it.
    let mut open: Option<(usize, File, u64)> = None;
    let mut finished = 0usize;
    for envelope in prepared.envelopes().map_err(corrupt)? {
        let envelope = envelope.map_err(corrupt)?;
        let index = manifest
            .entries
            .binary_search_by_key(&envelope.id, |entry| entry.id)
            .map_err(|_| {
                RepackError::Corrupt("staged content ID is absent from manifest".into())
            })?;
        let entry = &manifest.entries[index];
        let shard = ContentGenerationManifest::shard_of(index);
        if open
            .as_ref()
            .is_none_or(|(current, _, _)| *current != shard)
        {
            if let Some((_, pack, _)) = open.take() {
                pack.sync_all()?;
                finished += 1;
            }
            if shard != finished {
                return Err(RepackError::Corrupt(
                    "staged content is not grouped by shard".into(),
                ));
            }
            let path = content_generation_shard_pack_path(&directory, manifest.generation, shard);
            open = Some((shard, File::create(path)?, 0));
        }
        let (_, pack, pack_offset) = open.as_mut().expect("a shard pack is open");
        if entry.pack_offset != *pack_offset
            || u64::try_from(envelope.bytes.len()).ok() != Some(entry.physical_length)
            || blake3::hash(&envelope.bytes).as_bytes() != &entry.envelope_digest
        {
            return Err(RepackError::Corrupt(
                "staged content envelope differs from manifest".into(),
            ));
        }
        pack.write_all(&envelope.bytes)?;
        *pack_offset = pack_offset
            .checked_add(entry.physical_length)
            .ok_or_else(|| RepackError::Corrupt("content pack offset overflow".into()))?;
    }
    if let Some((_, pack, _)) = open.take() {
        pack.sync_all()?;
        finished += 1;
    }
    if finished != manifest.shard_count() {
        return Err(RepackError::Corrupt(
            "staged content does not cover every shard".into(),
        ));
    }
    for shard in 0..manifest.shard_count() {
        let path = content_generation_shard_manifest_path(&directory, manifest.generation, shard);
        let mut file = File::create(path)?;
        file.write_all(&manifest.encode_shard(shard).map_err(corrupt)?)?;
        file.sync_all()?;
    }
    File::open(&generation_dir)?.sync_all()?;
    // The index names every shard by digest; writing it last means an
    // authenticated index implies its shards were already durable.
    let index = manifest.index().map_err(corrupt)?;
    if index.manifest_digest != manifest.manifest_digest {
        return Err(RepackError::Corrupt(
            "staged content index differs from manifest".into(),
        ));
    }
    let encoded = index.encoded().map_err(corrupt)?;
    let digest = *blake3::hash(&encoded).as_bytes();
    let mut file = File::create(content_generation_index_path(
        &directory,
        manifest.generation,
        &digest,
    ))?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    File::open(&directory)?.sync_all()?;
    Ok(PhysicalContentGeneration {
        generation: manifest.generation,
        manifest: digest,
    })
}

/// Parse `{generation}-{hex digest}.manifest`: a top-level index file name.
fn parse_index_file_name(name: &str) -> Option<(u64, &str)> {
    let (generation, digest) = name.strip_suffix(".manifest")?.split_once('-')?;
    let generation = generation.parse::<u64>().ok()?;
    (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some((generation, digest))
}

/// Read `path` as an index for `generation`, authenticated by the digest its
/// file name carries.
fn authenticated_index(
    path: &Path,
    generation: u64,
    digest: &str,
) -> Result<Option<ContentGenerationIndex>, RepackError> {
    let encoded = fs::read(path)?;
    if hex::encode(blake3::hash(&encoded).as_bytes()) != digest {
        return Ok(None);
    }
    Ok(ContentGenerationIndex::decode(&encoded)
        .ok()
        .filter(|index| index.generation == generation))
}

/// Whether `path` is a generation directory holding nothing but shard files
/// `index` names. Anything else there is debris that is not provably ours.
fn generation_dir_is_owned(
    path: &Path,
    index: &ContentGenerationIndex,
) -> Result<bool, RepackError> {
    if !path.is_dir() {
        return Ok(false);
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let owned = entry.file_type()?.is_file()
            && entry.file_name().to_str().is_some_and(|name| {
                name.strip_prefix("shard-")
                    .and_then(|rest| {
                        rest.strip_suffix(".manifest")
                            .or_else(|| rest.strip_suffix(".pack"))
                    })
                    .filter(|number| number.len() == 6)
                    .and_then(|number| number.parse::<usize>().ok())
                    .is_some_and(|shard| shard < index.shards.len())
            });
        if !owned {
            return Ok(false);
        }
    }
    Ok(true)
}

fn authenticated_generation_dir(directory: &Path, generation: u64) -> Result<bool, RepackError> {
    let path = directory.join(generation.to_string());
    if !path.is_dir() {
        return Ok(false);
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some((named, digest)) = parse_index_file_name(&name) else {
            continue;
        };
        if named != generation {
            continue;
        }
        if let Some(index) = authenticated_index(&entry.path(), generation, digest)?
            && generation_dir_is_owned(&path, &index)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn remove_partial_generation(directory: &Path, generation: u64) -> Result<(), RepackError> {
    let path = directory.join(generation.to_string());
    if path.exists() {
        if !authenticated_generation_dir(directory, generation)? {
            return Err(RepackError::Corrupt(format!(
                "refusing to delete unauthenticated content generation {generation}"
            )));
        }
        fs::remove_dir_all(&path)?;
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

fn remove_staged_content_generation(
    root: &Path,
    generation: PhysicalContentGeneration,
) -> Result<(), RepackError> {
    let directory = root.join("content-generations");
    if generation.generation != 0
        && read_content_manifest(root, generation)?.is_some()
        && authenticated_generation_dir(&directory, generation.generation)?
    {
        fs::remove_dir_all(directory.join(generation.generation.to_string()))?;
        let manifest =
            content_generation_index_path(&directory, generation.generation, &generation.manifest);
        if manifest.exists() {
            fs::remove_file(manifest)?;
        }
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

/// Retires only content whose replacement is already named by the committed
/// catalog head. Callers hold the packed catalog process lock throughout;
/// this function never changes catalog authority.
///
/// `manifest` is the authenticated manifest of `retained`, as
/// [`validate_content_generation`] returned it.
fn retire_repacked_content(
    root: &Path,
    retained: PhysicalContentGeneration,
    manifest: Option<&ContentGenerationManifest>,
) -> Result<(), RepackError> {
    let Some(manifest) = manifest else {
        return Ok(());
    };
    if manifest.generation != retained.generation {
        return Err(RepackError::Corrupt(
            "retained content manifest names another generation".into(),
        ));
    }

    // A post-repack writer may add content after this manifest was staged. Only
    // IDs covered by this immutable manifest can lose their loose fallback.
    // The manifest is decoded once; directory syncs are batched by the fixed
    // two-hex-digit loose-content shard rather than performed per file.
    let loose_root = root.join("loose-content");
    let mut changed_prefixes = HashSet::new();
    for entry in &manifest.entries {
        let encoded = entry.id.to_string();
        let prefix = &encoded[..2];
        let path = loose_root.join(prefix).join(&encoded[2..]);
        match fs::remove_file(&path) {
            Ok(()) => {
                changed_prefixes.insert(prefix.to_owned());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    for prefix in changed_prefixes {
        File::open(loose_root.join(prefix))?.sync_all()?;
    }

    let directory = root.join("content-generations");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut retired = false;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some((generation, digest)) = parse_index_file_name(&name) else {
            continue;
        };
        if generation == retained.generation {
            continue;
        }
        let Some(index) = authenticated_index(&entry.path(), generation, digest)? else {
            continue;
        };
        let generation_dir = directory.join(generation.to_string());
        if generation_dir.exists() {
            // An existing directory must hold only this index's shards before
            // it is removed; foreign debris is not evidence that this is ours.
            // If a prior cleanup already removed the directory, the
            // authenticated stale index is still safe to finish retiring.
            if !generation_dir_is_owned(&generation_dir, &index)? {
                continue;
            }
            fs::remove_dir_all(generation_dir)?;
        }
        fs::remove_file(entry.path())?;
        retired = true;
    }
    if retired {
        File::open(&directory)?.sync_all()?;
    }
    Ok(())
}

/// What the repack report needs from one content generation, so the
/// generation's manifest need not be kept for it.
#[derive(Clone, Copy, Debug, Default)]
struct ContentGenerationStats {
    full: u64,
    delta: u64,
    max_depth: u8,
    bytes: u64,
}

impl ContentGenerationStats {
    fn of(manifest: Option<&ContentGenerationManifest>) -> Result<Self, RepackError> {
        let mut stats = Self::default();
        for entry in manifest.map_or(&[][..], |manifest| &manifest.entries) {
            if entry.delta_base.is_some() {
                stats.delta += 1;
            } else {
                stats.full += 1;
            }
            stats.max_depth = stats.max_depth.max(entry.delta_depth);
            stats.bytes = stats
                .bytes
                .checked_add(entry.physical_length)
                .ok_or_else(|| RepackError::Corrupt("content byte count overflow".into()))?;
        }
        Ok(stats)
    }
}

fn object_catalog_error(error: CatalogError) -> ObjectStoreError {
    match error {
        CatalogError::Io(error) => ObjectStoreError::Io(error),
        CatalogError::Database(error) => ObjectStoreError::Backend(error.to_string()),
        error => ObjectStoreError::Integrity(error.to_string()),
    }
}
fn content_catalog_error(id: ContentId, error: CatalogError) -> ContentStoreError {
    match error {
        CatalogError::Io(error) => ContentStoreError::Io(error),
        CatalogError::Database(error) => ContentStoreError::Io(io::Error::other(error.to_string())),
        error => ContentStoreError::InvalidFormat {
            id,
            reason: error.to_string(),
        },
    }
}

#[async_trait]
impl ImmutableObjectStore for PackedObjectStore {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        self.put_batch(&[(id, bytes.to_vec())], 1).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        if let Some(trace) = &self.engine.trace {
            trace.object_put_batch(
                objects.len(),
                objects.iter().map(|(_, bytes)| bytes.len()).sum(),
            );
        }
        let mut unique = BTreeMap::new();
        for (id, bytes) in objects {
            let canonical_kind: ObjectKind = validate_canonical_object(bytes)?.into();
            let actual = ObjectId::of_canonical_bytes(bytes);
            if actual != *id {
                return Err(ObjectStoreError::Corrupt {
                    expected: *id,
                    actual,
                });
            }
            if let Some(previous) = unique.insert(*id, (canonical_kind, bytes))
                && previous.1 != bytes
            {
                return Err(ObjectStoreError::Corrupt {
                    expected: *id,
                    actual,
                });
            }
        }
        let mut packed = Vec::new();
        let mut loose = Vec::new();
        for (id, (kind, bytes)) in unique {
            if bytes.len() > self.engine.loose_threshold {
                loose.push((id, bytes.clone()));
            } else {
                packed.push((id, kind, bytes));
            }
        }
        if !loose.is_empty() {
            self.engine.objects.put_batch(&loose, concurrency).await?;
        }
        if !packed.is_empty() {
            let records = packed
                .iter()
                .map(|(id, kind, data)| SegmentRecord {
                    namespace: METADATA_NAMESPACE,
                    kind: *kind as u16,
                    digest: *id.as_bytes(),
                    bytes: data,
                })
                .collect::<Vec<_>>();
            self.catalog()?
                .put_records(&records)
                .map_err(object_catalog_error)?;
        }
        Ok(())
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        let Some((stored_kind, bytes)) = self
            .catalog()?
            .get(METADATA_NAMESPACE, *id.as_bytes())
            .map_err(object_catalog_error)?
        else {
            // Oversized metadata bypasses packed segments. Probe the loose
            // fallback only after the in-memory locator says the ID is not packed;
            // the common packed path must not issue one failed filesystem lookup
            // per logical object read.
            let object = self.engine.objects.get(id).await?;
            if let Some(trace) = &self.engine.trace {
                trace.object_get(object.kind as u8, object.canonical_bytes.len());
            }
            return Ok(object);
        };
        // SegmentReader authenticated the immutable segment and verified this
        // payload against the requested object digest before returning it.
        let kind = ObjectKind::from_code(stored_kind).ok_or_else(|| {
            ObjectStoreError::Integrity("packed metadata has an unknown object kind".into())
        })?;
        if let Some(trace) = &self.engine.trace {
            trace.object_get(kind as u8, bytes.len());
        }
        Ok(StoredObject {
            id,
            kind,
            canonical_bytes: bytes.into(),
        })
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            result.push(match self.get(*id).await {
                Ok(_) => true,
                Err(ObjectStoreError::Missing(_)) => false,
                Err(e) => return Err(e),
            });
        }
        Ok(result)
    }
}

#[async_trait]
impl ContentBackend for PackedEngine {
    fn put_many(&self, contents: &[(ContentId, Vec<u8>)]) -> Result<(), ContentStoreError> {
        if let Some(trace) = &self.trace {
            trace.content_put_batch(
                contents.len(),
                contents.iter().map(|(_, bytes)| bytes.len()).sum(),
            );
        }
        let mut packed = Vec::new();
        for (id, bytes) in contents {
            let actual = ContentId::of(bytes);
            if actual != *id {
                return Err(ContentStoreError::Corrupt {
                    expected: *id,
                    actual,
                });
            }
            if bytes.len() > self.loose_threshold {
                self.content.put_many([(*id, bytes.clone())], 1)?;
            } else {
                packed.push((*id, bytes));
            }
        }
        if !packed.is_empty() {
            let records = packed
                .iter()
                .map(|(id, bytes)| SegmentRecord {
                    namespace: CONTENT_NAMESPACE,
                    kind: 0,
                    digest: *id.as_bytes(),
                    bytes,
                })
                .collect::<Vec<_>>();
            self.catalog
                .lock()
                .map_err(|_| {
                    ContentStoreError::Io(io::Error::other("packed catalog mutex poisoned"))
                })?
                .put_records(&records)
                .map_err(|error| content_catalog_error(packed[0].0, error))?;
        }
        Ok(())
    }

    fn get_bounded(&self, id: ContentId, max: usize) -> Result<Vec<u8>, ContentStoreError> {
        let generation = self
            .catalog
            .lock()
            .map_err(|_| ContentStoreError::Io(io::Error::other("packed catalog mutex poisoned")))?
            .content_generation();
        if generation.generation != 0 || generation.manifest != [0; 32] {
            return crate::content_store::resolve_delta_chain(id, max, |current_id| {
                self.get_raw_envelope(current_id)
            });
        }
        let value = self
            .catalog
            .lock()
            .map_err(|_| ContentStoreError::Io(io::Error::other("packed catalog mutex poisoned")))?
            .get_bounded(CONTENT_NAMESPACE, *id.as_bytes(), max)
            .map_err(|error| match error {
                CatalogError::Segment(super::segment::SegmentError::ReadLimit { minimum }) => {
                    ContentStoreError::TooLarge { id, max, minimum }
                }
                error => content_catalog_error(id, error),
            })?;
        if let Some((_, bytes)) = value.as_ref().filter(|(_, bytes)| bytes.len() > max) {
            return Err(ContentStoreError::TooLarge {
                id,
                max,
                minimum: bytes.len(),
            });
        }
        let Some((_kind, bytes)) = value else {
            // Not in a packed segment — could be oversized raw content or a
            // delta envelope in the loose content directory. Use
            // resolve_delta_chain with our own get_raw_envelope so that a
            // delta stored loose can reference a base in a packed segment.
            let bytes = crate::content_store::resolve_delta_chain(id, max, |current_id| {
                self.get_raw_envelope(current_id)
            })?;
            if let Some(trace) = &self.trace {
                trace.content_get(bytes.len());
            }
            return Ok(bytes);
        };
        // SegmentReader authenticated the immutable segment and verified this
        // payload against the requested content digest before returning it.
        if let Some(trace) = &self.trace {
            trace.content_get(bytes.len());
        }
        Ok(bytes)
    }

    fn get_raw_envelope(
        &self,
        id: ContentId,
    ) -> Result<(crate::codec::EnvelopeHeader, Vec<u8>), ContentStoreError> {
        if let Some(record) = self.with_generation(id, |reader| reader.read(id))? {
            return Ok(record);
        }
        // Content not yet repacked lives in segments as raw bytes, not envelopes.
        let value = self
            .catalog
            .lock()
            .map_err(|_| ContentStoreError::Io(io::Error::other("packed catalog mutex poisoned")))?
            .get_bounded(CONTENT_NAMESPACE, *id.as_bytes(), usize::MAX)
            .map_err(|error| content_catalog_error(id, error))?;
        if let Some((_kind, bytes)) = value {
            let header = crate::codec::EnvelopeHeader {
                codec: crate::codec::Codec::Raw,
                stored_length: bytes.len() as u64,
                logical_length: bytes.len() as u64,
            };
            return Ok((header, bytes));
        }
        // Fall back to loose content store (which uses envelope files).
        self.content.read_local_raw_envelope(id)
    }

    /// Answers from indexed metadata in the same order reads resolve content: the
    /// published content generation's manifest, the segment index, then a loose
    /// envelope's header. No payload is read.
    fn logical_length(&self, id: ContentId) -> Result<u64, ContentStoreError> {
        if let Some(length) = self.with_generation(id, |reader| reader.logical_length(id))? {
            return Ok(length);
        }
        let length = self
            .catalog
            .lock()
            .map_err(|_| ContentStoreError::Io(io::Error::other("packed catalog mutex poisoned")))?
            .logical_length(CONTENT_NAMESPACE, *id.as_bytes())
            .map_err(|error| content_catalog_error(id, error))?;
        match length {
            Some(length) => Ok(length),
            None => self.content.read_local_logical_length(id),
        }
    }

    fn put_raw_envelopes(
        &self,
        envelopes: &[(ContentId, Vec<u8>)],
    ) -> Result<(), ContentStoreError> {
        // Delta envelopes are stored in the loose content directory. The read
        // path (get_bounded) resolves delta chains via get_raw_envelope which
        // checks both the packed catalog and loose files, so a delta whose base
        // lives in a packed segment resolves correctly.
        for (id, envelope) in envelopes {
            self.content.put_raw_envelope(*id, envelope.clone())?;
        }
        Ok(())
    }

    fn supports_raw_envelopes(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn traced_deferred_store_counts_storage_work_without_extending_its_lifetime() {
        let root = tempfile::tempdir().unwrap();
        let (store, trace) = PackedObjectStore::open_deferred_traced(root.path()).unwrap();
        let bytes = wheat_core::DirectoryNode::default().canonical_bytes();
        let id = ObjectId::of_canonical_bytes(&bytes);
        store.put_if_absent(id, &bytes).await.unwrap();
        store.get(id).await.unwrap();

        let snapshot = trace.snapshot().unwrap();
        assert_eq!(snapshot.object_put_attempts, 1);
        assert_eq!(snapshot.object_gets, 1);
        assert!(snapshot.catalog_gets >= 2);
        assert_eq!(snapshot.deferred_appends, 1);
        assert_eq!(snapshot.deferred_index_transactions, 1);
        assert_eq!(snapshot.locator_recounts, 0);

        let final_snapshot = store.close_with_trace().unwrap().unwrap();
        assert_eq!(final_snapshot.object_put_attempts, 1);
        assert_eq!(final_snapshot.bulk_finalizations, 1);
        assert_eq!(final_snapshot.locator_recounts, 0);
        assert!(trace.snapshot().is_none());
    }

    #[test]
    fn delta_across_packed_and_loose_resolves_through_chain() {
        let root = tempfile::tempdir().unwrap();
        let store = PackedObjectStore::open_deferred(root.path()).unwrap();
        let content = store.content_store();

        // Base content stored raw → goes to packed segment via put_many backend.
        let base = b"hello world this is the base content for delta testing purposes here";
        let base_id = content.put(base).unwrap();
        assert_eq!(content.get(base_id).unwrap(), base);

        // Build a chain up to depth 14 (one below MAX_DELTA_CHAIN_DEPTH).
        let mut prev_id = base_id;
        let mut versions: Vec<(wheat_core::ContentId, Vec<u8>)> = Vec::new();
        for i in 1..=14 {
            let data = format!(
                "hello world this is version {i:03} content for delta testing purposes here"
            );
            let data = data.into_bytes();
            let id = wheat_core::ContentId::of(&data);
            content.put_with_base(id, &data, prev_id).unwrap();
            let read_back = content.get(id).unwrap();
            assert_eq!(read_back, data, "depth-{i} delta read failed");
            versions.push((id, data));
            prev_id = id;
        }

        // Verify all versions are still readable.
        for (i, (id, expected)) in versions.iter().enumerate() {
            let read_back = content.get(*id).unwrap();
            assert_eq!(&read_back, expected, "re-read of depth-{} failed", i + 1);
        }
    }

    #[test]
    fn deferred_close_failure_is_returned_by_packed_store() {
        let root = tempfile::tempdir().unwrap();
        let store = PackedObjectStore::open_deferred(root.path()).unwrap();
        store.test_fail_close_before_finalize().unwrap();
        let error = store.close().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("test failure before deferred bulk finalization")
        );
        PackedObjectStore::open_deferred(root.path())
            .unwrap()
            .close()
            .unwrap();
    }
}
