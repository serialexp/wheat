//! The catalog's record of loose files, so a store's loose bytes are known without
//! listing its loose directories.
//!
//! A loose file is written or removed outside any catalog transaction, so each one
//! is first recorded as pending in a transaction synced to disk, then written or
//! removed, then settled by looking at what is on disk. A crash in between leaves
//! the row pending, and open settles it the same way: the cost of a crash is one
//! look per file in flight. A new index records nothing yet; open fills it with one
//! listing of the loose directories.

use rusqlite::{Connection, OptionalExtension, params};

use super::{CatalogError, PackedCatalog, i64_value, u64_value};
use crate::Durability;

/// Which loose directory a file is in.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum LooseKind {
    Object,
    Content,
}

impl LooseKind {
    fn code(self) -> i64 {
        match self {
            Self::Object => 0,
            Self::Content => 1,
        }
    }

    fn from_code(code: i64) -> Result<Self, CatalogError> {
        match code {
            0 => Ok(Self::Object),
            1 => Ok(Self::Content),
            _ => Err(CatalogError::Corrupt("loose file kind")),
        }
    }

    /// The checkpoint column holding the kind's settled bytes.
    fn total(self) -> &'static str {
        match self {
            Self::Object => "loose_object_bytes",
            Self::Content => "loose_content_bytes",
        }
    }
}

/// Bytes the loose directories hold, by kind.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct LooseBytes {
    pub(crate) objects: u64,
    pub(crate) content: u64,
}

/// Adds `delta` to the kind's total.
fn add_to_total(index: &Connection, kind: LooseKind, delta: i64) -> Result<(), CatalogError> {
    if delta != 0 {
        index.execute(
            &format!(
                "UPDATE checkpoint SET {total}={total}+?1 WHERE singleton=1",
                total = kind.total()
            ),
            params![delta],
        )?;
    }
    Ok(())
}

impl PackedCatalog {
    /// Runs `f` with commits synced to disk, for a pending row that must be durable
    /// before the file it names changes.
    fn fully_synced<T>(
        &mut self,
        f: impl FnOnce(&mut Connection) -> Result<T, CatalogError>,
    ) -> Result<T, CatalogError> {
        if self.durability != Durability::Durable {
            return f(&mut self.index);
        }
        self.index.pragma_update(None, "synchronous", "FULL")?;
        let result = f(&mut self.index);
        self.index.pragma_update(None, "synchronous", "NORMAL")?;
        result
    }

    /// Records that loose files of `ids` are about to be written. Returns the IDs
    /// this call recorded, which the caller settles with [`Self::settle_loose`] once
    /// its writes are done, whether they succeeded or not. An ID already recorded,
    /// stored or pending another writer, is not the caller's to settle.
    pub(crate) fn intend_loose_writes(
        &mut self,
        kind: LooseKind,
        ids: &[[u8; 32]],
    ) -> Result<Vec<[u8; 32]>, CatalogError> {
        self.fully_synced(|index| {
            let transaction = index.transaction()?;
            let mut recorded = Vec::new();
            {
                let mut insert = transaction.prepare_cached(
                    "INSERT INTO loose_file(kind,digest,length,pending) VALUES(?1,?2,0,1) \
                     ON CONFLICT DO NOTHING",
                )?;
                for id in ids {
                    if insert.execute(params![kind.code(), id.as_slice()])? == 1 {
                        recorded.push(*id);
                    }
                }
            }
            transaction.commit()?;
            Ok(recorded)
        })
    }

    /// Records that the loose files of `ids` are about to be removed: each stored one
    /// turns pending and leaves the total. Returns those, for the caller to settle
    /// with [`Self::settle_loose`] once the removals are done.
    pub(crate) fn intend_loose_removals(
        &mut self,
        kind: LooseKind,
        ids: &[[u8; 32]],
    ) -> Result<Vec<[u8; 32]>, CatalogError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.fully_synced(|index| {
            let transaction = index.transaction()?;
            let mut recorded = Vec::new();
            let mut removed = 0_u64;
            {
                let mut mark = transaction.prepare_cached(
                    "UPDATE loose_file SET pending=1 \
                     WHERE kind=?1 AND digest=?2 AND pending=0 RETURNING length",
                )?;
                for id in ids {
                    let length = mark
                        .query_row(params![kind.code(), id.as_slice()], |row| {
                            row.get::<_, i64>(0)
                        })
                        .optional()?;
                    if let Some(length) = length {
                        removed = removed
                            .checked_add(u64_value(length)?)
                            .ok_or(CatalogError::Corrupt("loose byte total overflow"))?;
                        recorded.push(*id);
                    }
                }
            }
            add_to_total(&transaction, kind, -i64_value(removed)?)?;
            transaction.commit()?;
            Ok(recorded)
        })
    }

    /// Settles pending files by what is on disk: each `Some(length)` is stored and
    /// counted, each `None` forgotten.
    pub(crate) fn settle_loose(
        &mut self,
        kind: LooseKind,
        files: &[([u8; 32], Option<u64>)],
    ) -> Result<(), CatalogError> {
        if files.is_empty() {
            return Ok(());
        }
        let transaction = self.index.transaction()?;
        let mut added = 0_u64;
        {
            let mut store = transaction.prepare_cached(
                "UPDATE loose_file SET length=?3,pending=0 \
                 WHERE kind=?1 AND digest=?2 AND pending=1",
            )?;
            let mut forget = transaction.prepare_cached(
                "DELETE FROM loose_file WHERE kind=?1 AND digest=?2 AND pending=1",
            )?;
            for (id, length) in files {
                match length {
                    Some(length) => {
                        if store.execute(params![
                            kind.code(),
                            id.as_slice(),
                            i64_value(*length)?
                        ])? == 1
                        {
                            added = added
                                .checked_add(*length)
                                .ok_or(CatalogError::Corrupt("loose byte total overflow"))?;
                        }
                    }
                    None => {
                        forget.execute(params![kind.code(), id.as_slice()])?;
                    }
                }
            }
        }
        add_to_total(&transaction, kind, i64_value(added)?)?;
        transaction.commit()?;
        Ok(())
    }

    /// Files left pending by writes or removals that did not settle.
    pub(crate) fn pending_loose(&self) -> Result<Vec<(LooseKind, [u8; 32])>, CatalogError> {
        let mut query = self
            .index
            .prepare("SELECT kind,digest FROM loose_file WHERE pending=1")?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        rows.map(|row| {
            let (kind, digest) = row?;
            Ok((LooseKind::from_code(kind)?, super::digest32(digest)?))
        })
        .collect()
    }

    /// The stored loose files of `kind`.
    pub(crate) fn loose_ids(&self, kind: LooseKind) -> Result<Vec<[u8; 32]>, CatalogError> {
        let mut query = self
            .index
            .prepare("SELECT digest FROM loose_file WHERE kind=?1 AND pending=0")?;
        let rows = query.query_map(params![kind.code()], |row| row.get::<_, Vec<u8>>(0))?;
        rows.map(|digest| super::digest32(digest?)).collect()
    }

    /// Whether the loose files are recorded. A new index records none until
    /// [`Self::record_loose`] lists them.
    pub(crate) fn loose_recorded(&self) -> Result<bool, CatalogError> {
        let recorded: i64 = self.index.query_row(
            "SELECT loose_recorded FROM checkpoint WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        Ok(recorded == 1)
    }

    /// Records `files`, every loose file on disk with its length, in place of
    /// whatever was recorded.
    pub(crate) fn record_loose(
        &mut self,
        files: &[(LooseKind, [u8; 32], u64)],
    ) -> Result<(), CatalogError> {
        let transaction = self.index.transaction()?;
        transaction.execute("DELETE FROM loose_file", [])?;
        let mut totals = LooseBytes::default();
        {
            let mut insert = transaction.prepare_cached(
                "INSERT INTO loose_file(kind,digest,length,pending) VALUES(?1,?2,?3,0)",
            )?;
            for (kind, id, length) in files {
                insert.execute(params![kind.code(), id.as_slice(), i64_value(*length)?])?;
                let total = match kind {
                    LooseKind::Object => &mut totals.objects,
                    LooseKind::Content => &mut totals.content,
                };
                *total = total
                    .checked_add(*length)
                    .ok_or(CatalogError::Corrupt("loose byte total overflow"))?;
            }
        }
        transaction.execute(
            "UPDATE checkpoint SET loose_recorded=1,loose_object_bytes=?1,loose_content_bytes=?2 \
             WHERE singleton=1",
            params![i64_value(totals.objects)?, i64_value(totals.content)?],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// The settled loose bytes, by kind.
    pub(crate) fn loose_bytes(&self) -> Result<LooseBytes, CatalogError> {
        let (objects, content): (i64, i64) = self.index.query_row(
            "SELECT loose_object_bytes,loose_content_bytes FROM checkpoint WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(LooseBytes {
            objects: u64_value(objects)?,
            content: u64_value(content)?,
        })
    }
}
