//! Pool garbage collection's sweep of a packed store, and the pins that keep what it
//! removes readable for readers that began before it
//! (`docs/design/wheat-network-maintenance.md`, "Open question 8").
//!
//! The store's readers share an [`Epoch`]; a pin is a hold on the current one. A
//! sweep publishes the segments it rewrote, moves the loose objects it removes into
//! a directory of its own under [`RETIRED_DIRECTORY`], hands both to the epoch that
//! was current, and starts a new one. Pins taken before then read what was removed
//! through the old epoch, and the files go when the last of them is dropped. A crash
//! leaves segment files no catalog names and the retired directory, which the next
//! open removes.

use super::super::catalog::{Keep, RetiredSegments, build_collection};
use super::*;
use crate::{
    CollectableStructure, ObjectMarks, StorePin, SweepReport, structure_gc::RemovedObjects,
};

/// Where a sweep moves the loose objects it removes, under the store's directory.
pub(crate) const RETIRED_DIRECTORY: &str = "retired";

/// What the store's readers read since the last sweep, and, once a sweep replaced
/// it, what that sweep removed.
#[derive(Default)]
pub(super) struct Epoch {
    /// Set by the sweep that ends the epoch. A sweep holds it from before it
    /// publishes until what it removed is in place, so a reader that missed an
    /// object meanwhile waits here rather than finding nothing.
    retired: Mutex<Option<Retired>>,
}

/// What a sweep removed, readable until the epoch before it is dropped.
struct Retired {
    segments: RetiredSegments,
    /// The loose objects removed, moved into a directory removed with this.
    loose: Option<(LooseObjectStore, tempfile::TempDir)>,
}

impl RemovedObjects for Epoch {
    fn removed_object(&self, id: ObjectId) -> Result<Option<StoredObject>, ObjectStoreError> {
        let mut retired = self.retired.lock().map_err(|_| {
            ObjectStoreError::Backend("a sweep failed while it held the retired store".into())
        })?;
        let Some(retired) = retired.as_mut() else {
            return Ok(None);
        };
        if let Some(payload) = retired
            .segments
            .get(METADATA_NAMESPACE, *id.as_bytes())
            .map_err(object_catalog_error)?
        {
            let kind = ObjectKind::from_code(payload.kind()).ok_or_else(|| {
                ObjectStoreError::Integrity("packed metadata has an unknown object kind".into())
            })?;
            return Ok(Some(StoredObject {
                id,
                kind,
                canonical_bytes: payload.into_bytes().into(),
            }));
        }
        if let Some((loose, _)) = &retired.loose {
            match loose.read_verified(id) {
                Ok(object) => return Ok(Some(object)),
                Err(ObjectStoreError::Missing(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }
}

impl PackedEngine {
    fn current_epoch(&self) -> Arc<Epoch> {
        // The lock guards a pointer swap only, so a poisoned one holds a whole value.
        self.epoch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn start_epoch(&self) {
        *self
            .epoch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(Epoch::default());
    }
}

/// Removes what a crash left of the retired directory: no pin outlives the process.
pub(super) fn remove_retired(root: &Path) -> io::Result<()> {
    match fs::remove_dir_all(root.join(RETIRED_DIRECTORY)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn marks_error(error: rusqlite::Error) -> ObjectStoreError {
    ObjectStoreError::Backend(format!("the sweep's marks failed: {error}"))
}

impl CollectableStructure for PackedObjectStore {
    fn pin(&self) -> StorePin {
        StorePin::new(self.engine.current_epoch())
    }

    /// Rewrites the sealed segments keeping the marked objects and every content
    /// record, then retires the unmarked loose objects. The rewrite runs without the
    /// catalog's lock, so reads go on beside it; the lock is held to publish.
    #[tracing::instrument(level = "debug", skip_all)]
    fn sweep(&self, marks: &ObjectMarks) -> Result<SweepReport, ObjectStoreError> {
        let sources = self
            .catalog()?
            .begin_collection()
            .map_err(object_catalog_error)?;
        marks.begin().map_err(marks_error)?;
        let mut keep = |record: &ListedRecord| -> Result<Keep, CatalogError> {
            if record.namespace == METADATA_NAMESPACE && !marks.contains(&record.digest)? {
                return Ok(Keep::Drop);
            }
            Ok(if marks.first_copy(record.namespace, &record.digest)? {
                Keep::Copy
            } else {
                Keep::Duplicate
            })
        };
        let built = tracing::debug_span!("build_collection").in_scope(|| {
            build_collection(
                sources,
                &mut keep,
                RepackOptions::default().target_segment_bytes,
            )
        });
        let committed = marks.commit().map_err(marks_error);
        let built = built.map_err(object_catalog_error)?;
        committed?;
        let counts = built.counts;

        // Readers pinned before now read what this removes through `old`; one that
        // misses an object from here on waits on `retired` until it is in place.
        let old = self.engine.current_epoch();
        let mut retired = old.retired.lock().map_err(|_| {
            ObjectStoreError::Backend("an earlier sweep failed while it held its epoch".into())
        })?;
        let mut catalog = self.catalog()?;
        let content = catalog.content_generation();
        let segments = catalog
            .publish_collection(built, content)
            .map_err(object_catalog_error)?;
        let mut report = SweepReport {
            objects_kept: counts.objects_kept,
            object_bytes_kept: counts.object_bytes_kept,
            objects_dropped: counts.objects_dropped,
            object_bytes_dropped: counts.object_bytes_dropped,
            content_records_kept: counts.content_kept,
            segments_before: segments.segments as u64,
            segment_bytes_before: segments.bytes,
            segments_after: counts.segments_written,
            segment_bytes_after: counts.bytes_written,
            ..SweepReport::default()
        };
        let retired = retired.insert(Retired {
            segments,
            loose: None,
        });
        self.engine.start_epoch();

        let mut unmarked = Vec::new();
        for id in catalog
            .loose_ids(LooseKind::Object)
            .map_err(object_catalog_error)?
        {
            if !marks.contains(&id).map_err(marks_error)? {
                unmarked.push(id);
            }
        }
        if unmarked.is_empty() {
            return Ok(report);
        }
        let recorded = catalog
            .intend_loose_removals(LooseKind::Object, &unmarked)
            .map_err(object_catalog_error)?;
        for (_, length) in self.engine.loose.lengths(LooseKind::Object, &recorded)? {
            if let Some(length) = length {
                report.loose_objects_retired += 1;
                report.loose_object_bytes_retired += length;
            }
        }
        let directory = self.engine.root.join(RETIRED_DIRECTORY);
        fs::create_dir_all(&directory)?;
        let holder = tempfile::Builder::new()
            .prefix("sweep-")
            .tempdir_in(&directory)?;
        let target = LooseObjectStore::with_durability(holder.path(), Durability::Deferred);
        let (target, _) = retired.loose.insert((target, holder));
        let ids = recorded
            .iter()
            .map(|digest| ObjectId::from_digest(*digest))
            .collect::<Vec<_>>();
        let moved = self.engine.objects.retire_into(&ids, target);
        let settled = self
            .engine
            .loose
            .lengths(LooseKind::Object, &recorded)
            .map_err(ObjectStoreError::from)
            .and_then(|files| {
                catalog
                    .settle_loose(LooseKind::Object, &files)
                    .map_err(object_catalog_error)
            });
        moved?;
        settled?;
        Ok(report)
    }
}
