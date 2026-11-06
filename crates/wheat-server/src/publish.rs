//! Publish protocol handler — receive objects, validate, commit.

use tracing::{Instrument as _, debug, debug_span, info, warn};

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use wheat_core::{
    ContentId, ObjectId, PublishReceiptRecord, RepositoryEnvelopeId, VerifiedContent,
};
use wheat_protocol::*;
use wheat_store::{
    DeltaStep, ImmutableObjectStore, ManifestArrivals, MerkleTreeStore, ObjectStoreError,
    RepositoryHead, RepositoryReader, RepositoryStage, StructuralDelta, StructuralDeltaError,
    StructuralDeltaLimits, TreeDiffLimits,
};

use crate::access::Principal;
use crate::blocking::drive;
use crate::commit::{self, Accepted, CommitFailure};
use crate::connection::ConnectionError;
use crate::error_text::chain;
use crate::hooks::{Announcement, ProposedPublish, PublishHooks};
use crate::naming::Named;
use crate::network::Network;
use crate::receipt::PublishKey;
use crate::registry::RepositoryEntry;
use crate::transport::Transport;
use crate::wire::{
    connection_error_scope, digest_to_content_id, digest_to_object_id, publish_error_status,
    publish_published, publish_rejected_status, publish_result, publish_stale_status, recv, send,
    send_error,
};

// A blob the protocol takes as bytes is one the store keeps whole; a longer one
// comes as its manifest.
const _: () = assert!(MAX_CONTENT_SIZE as u64 == wheat_core::CHUNKED_CONTENT_THRESHOLD);

/// What a client pushed unasked straight after its `PublishRequest`: objects of its
/// own walk from the expected head, and small added blobs. The publish walks take
/// what they need from here before asking; the rest is dropped with the publish.
#[derive(Default)]
pub struct Pushed {
    objects: HashMap<ObjectId, Vec<u8>>,
    contents: HashMap<ContentId, Vec<u8>>,
}

/// Reads the push that follows every `PublishRequest`, up to its `PublishPushEnd`.
/// With `keep` false, for a publish refused before it starts, the bytes are only
/// counted. A push past [`MAX_PUSHED_BYTES`], a blob over [`MAX_PUSHED_BLOB_SIZE`],
/// counts that differ from what arrived, or any other message before the end break
/// the protocol: the client is told why and the connection ends.
#[tracing::instrument(name = "publish_push", level = "debug", skip_all)]
pub async fn receive_push(
    ws: &mut impl Transport,
    op_id: u64,
    keep: bool,
) -> Result<Pushed, ConnectionError> {
    match read_push(ws, op_id, keep).await? {
        Ok(pushed) => Ok(pushed),
        Err(violation) => {
            warn!(op_id, violation, "publish: broken push");
            send_error(ws, op_id, 1, connection_error_scope(), false, violation).await?;
            Err(ConnectionError::Protocol(violation))
        }
    }
}

/// [`receive_push`] up to the first rule the push breaks, which it returns.
async fn read_push(
    ws: &mut impl Transport,
    op_id: u64,
    keep: bool,
) -> Result<Result<Pushed, &'static str>, ConnectionError> {
    let mut pushed = Pushed::default();
    let (mut objects, mut contents, mut bytes) = (0u64, 0u64, 0usize);
    loop {
        match recv(ws).await? {
            WheatProtocolMessage::ObjectBatch(batch) if batch.op_id == op_id => {
                for object in batch.objects {
                    bytes = bytes.saturating_add(object.data.len());
                    if bytes > MAX_PUSHED_BYTES {
                        return Ok(Err(OVER_PUSH_LIMIT));
                    }
                    objects += 1;
                    let id = digest_to_object_id(&object.id)?;
                    if keep {
                        pushed.objects.entry(id).or_insert(object.data);
                    }
                }
            }
            WheatProtocolMessage::ContentBatch(batch) if batch.op_id == op_id => {
                for blob in batch.contents {
                    if blob.data.len() > MAX_PUSHED_BLOB_SIZE {
                        return Ok(Err("a publish pushed a blob over MAX_PUSHED_BLOB_SIZE"));
                    }
                    bytes = bytes.saturating_add(blob.data.len());
                    if bytes > MAX_PUSHED_BYTES {
                        return Ok(Err(OVER_PUSH_LIMIT));
                    }
                    contents += 1;
                    let id = digest_to_content_id(&blob.id)?;
                    if keep {
                        pushed.contents.entry(id).or_insert(blob.data);
                    }
                }
            }
            WheatProtocolMessage::PublishPushEnd(end) if end.op_id == op_id => {
                if end.pushed_objects != objects || end.pushed_contents != contents {
                    return Ok(Err("PublishPushEnd counts differ from what was pushed"));
                }
                debug!(op_id, objects, contents, bytes, "publish: push received");
                return Ok(Ok(pushed));
            }
            _ => {
                return Ok(Err(
                    "only ObjectBatch and ContentBatch may come before PublishPushEnd",
                ));
            }
        }
    }
}

const OVER_PUSH_LIMIT: &str = "a publish pushed more than MAX_PUSHED_BYTES";

/// Who publishes to which repository, and the embedder's hooks to run around it.
#[derive(Clone, Copy)]
pub struct Publisher<'a> {
    pub repository: &'a str,
    pub principal: &'a Principal,
    pub hooks: Option<&'a Arc<dyn PublishHooks>>,
}

/// Handle a publish request whose push has been read, against `current_head` and the
/// `receipt` it holds for this publish's key. Returns the new head if publication succeeded.
#[tracing::instrument(name = "publish", level = "debug", skip_all)]
pub async fn handle_publish(
    ws: &mut impl Transport,
    entry: &Arc<RepositoryEntry>,
    current_head: &RepositoryHead,
    receipt: Option<&PublishReceiptRecord>,
    req: PublishRequestOutput,
    key: PublishKey,
    pushed: Pushed,
    publisher: Publisher<'_>,
) -> Result<Option<RepositoryHead>, ConnectionError> {
    let op_id = req.op_id;
    let force = req.force;
    let force_labels = req.force_labels;
    debug!(op_id, force, force_labels, "publish: starting");

    // A publish sent again, after the connection it was first sent on was lost, gets
    // the first sending's answer, and nothing is transferred again.
    match commit::previous(entry, *current_head, receipt, &key).await {
        Ok(None) => {}
        Ok(Some(previous)) => return answer(ws, op_id, key, Ok(previous)).await,
        Err(failure) => return answer(ws, op_id, key, Err(failure)).await,
    }

    // The head the client last saw. A forced publish replaces the head, so it must
    // be exactly this one. Any other publish is joined with whatever was published
    // since, at commit.
    let expected = RepositoryHead {
        generation: req.expected_generation,
        envelope: RepositoryEnvelopeId::from_digest(
            req.expected_envelope
                .bytes
                .as_slice()
                .try_into()
                .map_err(|_| ConnectionError::Protocol("invalid expected envelope ID"))?,
        ),
    };
    if force && expected != *current_head {
        return stale(
            ws,
            op_id,
            current_head,
            "a forced publish must name the current head",
        )
        .await;
    }

    let proposed_envelope_id = RepositoryEnvelopeId::from_digest(
        req.envelope_id
            .bytes
            .as_slice()
            .try_into()
            .map_err(|_| ConnectionError::Protocol("invalid envelope ID"))?,
    );

    // Uploaded objects and content are staged and stored together in one durable
    // write once the publish is accepted, rather than one per round of the walk. A
    // refused publish leaves nothing behind. Only a publish larger than the stage is
    // written early, in stage-sized batches, and what they hold is harmless if it
    // is then refused. The stage owns its handle to the store, so the store work
    // runs on blocking threads, a batch at a time.
    let staged = Arc::new(RepositoryStage::new(
        entry.network.clone(),
        STAGED_OBJECTS,
        STAGED_BYTES,
    ));

    // --- Object transfer phase ---
    let received = receive_objects(
        ws,
        &staged,
        op_id,
        current_head,
        proposed_envelope_id,
        pushed.objects,
    )
    .await;
    match received {
        Ok(None) => {}
        Ok(Some(refusal)) => return reject(ws, op_id, current_head, &refusal).await,
        Err(error) => return upload_failed(ws, op_id, key, current_head, error).await,
    }

    // --- Content transfer phase ---
    // Every stamp the publish's content gets is newer than this (see
    // `crate::naming`).
    let planned = tokio::time::Instant::now();
    let plan = {
        let (staged, network) = (Arc::clone(&staged), entry.network.clone());
        let current = current_head.envelope;
        let pushed = pushed.contents;
        drive(move || async move {
            plan_content(&staged, &network, current, proposed_envelope_id, pushed).await
        })
        .await
        .map_err(ConnectionError::from)
        .flatten()
    };
    let plan = match plan {
        Ok(plan) => plan,
        Err(error) => return upload_failed(ws, op_id, key, current_head, error).await,
    };
    let Some(plan) = plan else {
        return reject(ws, op_id, current_head, "content identity mismatch").await;
    };

    // Scoped, so the plan is not held through the commit. What the publish relies
    // on and uploads is kept only for a network that is told it.
    let relied = {
        let ContentPlan {
            missing,
            base_map,
            relied_on,
            bases,
            uploaded,
        } = plan;
        let mut manifests = Vec::new();
        let received = receive_content(
            ws,
            &staged,
            op_id,
            missing.into(),
            &base_map,
            &mut manifests,
        )
        .await;
        match received {
            Ok(None) => {}
            Ok(Some(refusal)) => return reject(ws, op_id, current_head, &refusal).await,
            Err(error) => return upload_failed(ws, op_id, key, current_head, error).await,
        }
        (relied_on, bases, manifests, uploaded)
    };

    // Nothing more is needed; the client may commit.
    send(
        ws,
        &WheatProtocolMessage::ContentComplete(ContentCompleteOutput {
            message_type: 0x4a,
            op_id,
        }),
    )
    .await?;

    // --- Commit phase ---
    loop {
        let msg = recv(ws).await?;
        match msg {
            WheatProtocolMessage::PublishCommit(pc) => {
                if pc.op_id == op_id {
                    debug!(op_id, "publish: commit requested");
                    break;
                }
            }
            _ => {
                warn!(op_id, "publish: unexpected message waiting for commit");
            }
        }
    }

    // The embedder's check, before the publish is joined onto the head.
    let announcement = match publisher.hooks {
        None => None,
        Some(hooks) => {
            let read = {
                let staged = Arc::clone(&staged);
                drive(move || async move {
                    RepositoryReader::new(&*staged, staged.content())
                        .label_changes(Some(expected.envelope), proposed_envelope_id)
                        .await
                        .map_err(|e| e.to_string())
                })
                .await
            };
            let labels = match read {
                Ok(Ok(labels)) => labels,
                Err(error) => {
                    return upload_failed(ws, op_id, key, current_head, error.into()).await;
                }
                Ok(Err(e)) => {
                    let message = format!("cannot compare with the head this publish names: {e}");
                    return reject(ws, op_id, current_head, &message).await;
                }
            };
            let publish = ProposedPublish {
                repository: publisher.repository.to_owned(),
                principal: publisher.principal.clone(),
                expected,
                proposed: proposed_envelope_id,
                force,
                force_labels,
                labels,
                received: staged.received(),
            };
            let checked = hooks
                .check(&publish)
                .instrument(debug_span!("publish_hook_check"))
                .await;
            if let Err(refusal) = checked {
                return reject(ws, op_id, current_head, &refusal.message).await;
            }
            Some(Announcement {
                hooks: Arc::clone(hooks),
                publish,
            })
        }
    };

    // The content the publish relies on without writing it, named before the
    // publish joins the commit.
    let named = match (&entry.network.naming, relied) {
        (Some(naming), (relied_on, bases, manifests, uploaded)) => {
            let needed = {
                let network = entry.network.clone();
                drive(move || async move {
                    network
                        .content
                        .needed_to_read(relied_on.into_iter().chain(manifests), bases)
                })
                .await
            };
            let named = match needed {
                Err(error) => {
                    return upload_failed(ws, op_id, key, current_head, error.into()).await;
                }
                Ok(Ok(needed)) => naming
                    .named(&needed)
                    .instrument(debug_span!("publish_name", ids = needed.len()))
                    .await
                    .map(|()| needed)
                    .map_err(|e| e.to_string()),
                Ok(Err(e)) => Err(e.to_string()),
            };
            let mut ids = match named {
                Ok(ids) => ids,
                Err(e) => {
                    warn!(op_id, error = %e, "publish: naming its content failed");
                    let failed = CommitFailure::Failed {
                        head: *current_head,
                        message: format!("naming the published content failed: {e}; publish again"),
                    };
                    return answer(ws, op_id, key, Err(failed)).await;
                }
            };
            ids.extend(uploaded);
            Some(Named {
                since: planned,
                ids,
            })
        }
        (None, _) => None,
    };

    send(
        ws,
        &WheatProtocolMessage::PublishProgress(PublishProgressOutput {
            message_type: 0x48,
            op_id,
            phase: "validating".to_string(),
            percent: 255, // indeterminate
        }),
    )
    .await?;
    commit(
        ws,
        entry,
        &staged,
        op_id,
        key,
        expected,
        proposed_envelope_id,
        force,
        force_labels,
        announcement,
        named,
    )
    .await
}

/// Makes `proposed` the head, joined with whatever was published since `expected`,
/// and tells the client the new head.
///
/// So writers of unrelated changes never have to fetch and try again. Publishes join
/// one at a time, onto the last accepted publish even before it is durable, and are
/// then made durable in batches (see [`crate::commit`]). The head the client is told
/// is its batch's, which includes every publish in the batch.
#[tracing::instrument(name = "publish_commit", level = "debug", skip_all)]
async fn commit(
    ws: &mut impl Transport,
    entry: &Arc<RepositoryEntry>,
    staged: &Arc<Staged>,
    op_id: u64,
    key: PublishKey,
    expected: RepositoryHead,
    proposed: RepositoryEnvelopeId,
    force: bool,
    force_labels: bool,
    announcement: Option<Announcement>,
    named: Option<Named>,
) -> Result<Option<RepositoryHead>, ConnectionError> {
    let accepted = commit::accept(
        entry,
        staged,
        key,
        expected.envelope,
        proposed,
        force,
        force_labels,
        announcement,
        named,
    )
    .await;
    answer(ws, op_id, key, accepted).await
}

/// Tells the client how its publish ended: the head it was published in, or why it
/// was not.
async fn answer(
    ws: &mut impl Transport,
    op_id: u64,
    key: PublishKey,
    accepted: Result<Accepted, CommitFailure>,
) -> Result<Option<RepositoryHead>, ConnectionError> {
    let (repeated, outcome) = match accepted {
        Ok(accepted) => (accepted.repeated(), accepted.durable().await),
        Err(refused) => (false, Err(refused)),
    };
    match outcome {
        Ok(head) => {
            info!(
                op_id,
                generation = head.generation,
                repeated,
                "publish: committed"
            );
            send(
                ws,
                &publish_published(op_id, repeated, key.receipt_id(head), head),
            )
            .await?;
            Ok(Some(head))
        }
        Err(CommitFailure::Stale { head, message }) => stale(ws, op_id, &head, &message).await,
        Err(CommitFailure::Rejected { head, message }) => {
            info!(op_id, message, "publish: validation failed");
            reject(ws, op_id, &head, &message).await
        }
        Err(CommitFailure::Failed { head, message }) => {
            warn!(op_id, message, "publish: commit failed");
            send(
                ws,
                &publish_result(
                    op_id,
                    publish_error_status(),
                    head.generation,
                    head.envelope.as_bytes(),
                    &message,
                ),
            )
            .await?;
            Ok(None)
        }
        Err(CommitFailure::Internal(message)) => Err(ConnectionError::Internal(message)),
    }
}

/// Answers a publish whose upload failed. A failure of the store (an early stage
/// write, a manifest, a read) leaves nothing of the publish kept, so the client is
/// told to send it again; a broken connection or protocol ends the session.
async fn upload_failed(
    ws: &mut impl Transport,
    op_id: u64,
    key: PublishKey,
    head: &RepositoryHead,
    error: ConnectionError,
) -> Result<Option<RepositoryHead>, ConnectionError> {
    let ConnectionError::Internal(message) = error else {
        return Err(error);
    };
    let failed = CommitFailure::Failed {
        head: *head,
        message: format!("{message}; publish again"),
    };
    answer(ws, op_id, key, Err(failed)).await
}

/// Send the publish back as stale: the client must fetch first.
async fn stale(
    ws: &mut impl Transport,
    op_id: u64,
    head: &RepositoryHead,
    message: &str,
) -> Result<Option<RepositoryHead>, ConnectionError> {
    info!(op_id, message, "publish: stale");
    send(
        ws,
        &publish_result(
            op_id,
            publish_stale_status(),
            head.generation,
            head.envelope.as_bytes(),
            message,
        ),
    )
    .await?;
    Ok(None)
}

/// Refuse the publish: tell the client, leave the head where it is.
async fn reject(
    ws: &mut impl Transport,
    op_id: u64,
    current_head: &RepositoryHead,
    message: &str,
) -> Result<Option<RepositoryHead>, ConnectionError> {
    info!(op_id, message, "publish: refused");
    send(
        ws,
        &publish_result(
            op_id,
            publish_rejected_status(),
            current_head.generation,
            current_head.envelope.as_bytes(),
            message,
        ),
    )
    .await?;
    Ok(None)
}

/// Writers used to store one message's worth of uploaded objects.
const OBJECT_WRITE_CONCURRENCY: usize = 8;

/// A publish's uploaded objects and content, held until the publish is accepted.
/// Reads see them before they are stored.
type Staged = RepositoryStage<Network>;

/// Most objects one publish stages before writing them early.
const STAGED_OBJECTS: usize = 65_536;
/// Most object bytes, and separately most content bytes, one publish stages before
/// writing them early, so a publish never holds every uploaded blob in memory.
const STAGED_BYTES: usize = 64 * 1024 * 1024;

/// Gets the objects of `proposed` that the store lacks, until it has them all.
///
/// Walks from the current head to `proposed` ([`StructuralDelta`]). Each batch of
/// objects the walk finds missing is taken from `pushed` where the client pushed
/// them; the rest become one `ObjectsNeeded`, and the walk goes on only once every
/// object of that batch has arrived. `ObjectsComplete` follows when the walk ends:
/// the store then holds all of `proposed` that the current head does not already
/// cover. Returns why the publish is refused, if it is.
#[tracing::instrument(name = "publish_objects", level = "debug", skip_all)]
async fn receive_objects(
    ws: &mut impl Transport,
    staged: &Arc<Staged>,
    op_id: u64,
    current_head: &RepositoryHead,
    proposed: RepositoryEnvelopeId,
    pushed: HashMap<ObjectId, Vec<u8>>,
) -> Result<Option<String>, ConnectionError> {
    let mut walk = Walk {
        delta: StructuralDelta::new(
            Arc::clone(staged),
            Some(current_head.envelope),
            proposed,
            StructuralDeltaLimits::default(),
        ),
        staged: Arc::clone(staged),
        pushed,
    };
    let mut rounds = 0u32;
    loop {
        // One job per stretch of the walk, the walk going to it and back.
        let (back, advanced) = drive(move || async move {
            let advanced = walk.advance().await;
            (walk, advanced)
        })
        .await?;
        walk = back;
        let ids = match advanced? {
            Advanced::Done => break,
            Advanced::Refused(refusal) => return Ok(Some(refusal)),
            Advanced::Missing(ids) => ids,
        };
        send(
            ws,
            &WheatProtocolMessage::ObjectsNeeded(ObjectsNeededOutput {
                message_type: 0x41,
                op_id,
                ids: ids
                    .iter()
                    .map(|id| Digest256 {
                        bytes: id.as_bytes().to_vec(),
                    })
                    .collect(),
            }),
        )
        .await?;
        if let Some(refusal) = receive_round(ws, staged, op_id, &ids).await? {
            return Ok(Some(refusal));
        }
        rounds += 1;
        walk.delta.resume();
    }
    debug!(op_id, rounds, stats = ?walk.delta.stats(), "publish: objects complete");
    send(
        ws,
        &WheatProtocolMessage::ObjectsComplete(ObjectsCompleteOutput {
            message_type: 0x42,
            op_id,
        }),
    )
    .await?;
    Ok(None)
}

/// A publish's walk from the current head to the proposed one, with the objects the
/// client pushed for it.
struct Walk {
    delta: StructuralDelta<Arc<Staged>>,
    staged: Arc<Staged>,
    pushed: HashMap<ObjectId, Vec<u8>>,
}

/// How far [`Walk::advance`] got.
enum Advanced {
    /// The store holds everything the proposed head adds.
    Done,
    /// The client must send these before the walk goes on.
    Missing(Vec<ObjectId>),
    /// Why the publish is refused.
    Refused(String),
}

impl Walk {
    /// Walks on until the walk ends or finds objects that neither the store nor the
    /// push has, staging those the push has.
    async fn advance(&mut self) -> Result<Advanced, ConnectionError> {
        loop {
            let step = match self.delta.next().await {
                Ok(step) => step,
                // Reading the store, or the current head's side of the walk, failed.
                Err(
                    error @ (StructuralDeltaError::Store(_)
                    | StructuralDeltaError::Collection(_)
                    | StructuralDeltaError::OldLineage(_)),
                ) => {
                    return Err(ConnectionError::Internal(format!(
                        "publish walk failed: {error}"
                    )));
                }
                Err(error) => {
                    return Ok(Advanced::Refused(format!(
                        "the proposed envelope is invalid: {error}"
                    )));
                }
            };
            let mut ids = match step {
                None => return Ok(Advanced::Done),
                // Already in the store.
                Some(DeltaStep::Object(_)) => continue,
                Some(DeltaStep::Missing(ids)) => ids,
            };
            let mut claimed = Vec::new();
            ids.retain(|id| match self.pushed.remove(id) {
                Some(data) => {
                    claimed.push((*id, data));
                    false
                }
                None => true,
            });
            if !claimed.is_empty()
                && let Some(refusal) = stage_objects(&self.staged, &claimed).await?
            {
                return Ok(Advanced::Refused(refusal));
            }
            if !ids.is_empty() {
                return Ok(Advanced::Missing(ids));
            }
            self.delta.resume();
        }
    }
}

/// Takes one round of uploads: each object of `ids` once, whole in an
/// `ObjectBatch` or in `ObjectChunk`s, and nothing else. Returns why the round is
/// refused, if it is.
async fn receive_round(
    ws: &mut impl Transport,
    staged: &Arc<Staged>,
    op_id: u64,
    ids: &[ObjectId],
) -> Result<Option<String>, ConnectionError> {
    let mut receiver = TransferReceiver::new(ids.iter().copied(), MAX_OBJECT_SIZE);
    while !receiver.is_complete() {
        match recv(ws).await? {
            WheatProtocolMessage::ObjectBatch(batch) if batch.op_id == op_id => {
                let mut objects = Vec::with_capacity(batch.objects.len());
                for object in batch.objects {
                    let id = digest_to_object_id(&object.id)?;
                    if let Err(error) = receiver.accept_whole(id, object.data.len()) {
                        return Ok(Some(error.to_string()));
                    }
                    objects.push((id, object.data));
                }
                if let Some(refusal) = store_received(staged, objects).await? {
                    return Ok(Some(refusal));
                }
            }
            WheatProtocolMessage::ObjectChunk(chunk) if chunk.op_id == op_id => {
                let id = digest_to_object_id(&chunk.object_id)?;
                let object =
                    match receiver.accept_chunk(id, chunk.offset, chunk.data, chunk.is_final) {
                        Ok(Some(object)) => object,
                        Ok(None) => continue,
                        Err(error) => return Ok(Some(error.to_string())),
                    };
                if let Some(refusal) = store_received(staged, vec![object]).await? {
                    return Ok(Some(refusal));
                }
            }
            _ => {
                return Ok(Some(
                    "only ObjectBatch and ObjectChunk may answer ObjectsNeeded".to_owned(),
                ));
            }
        }
    }
    Ok(None)
}

/// [`stage_objects`] for objects received in a round, as one job.
async fn store_received(
    staged: &Arc<Staged>,
    objects: Vec<(ObjectId, Vec<u8>)>,
) -> Result<Option<String>, ConnectionError> {
    let staged = Arc::clone(staged);
    drive(move || async move { stage_objects(&staged, &objects).await }).await?
}

/// Stages uploaded objects. Returns why they are refused when one is not the object
/// its ID names, or they are not valid Wheat objects.
async fn stage_objects(
    staged: &Staged,
    objects: &[(ObjectId, Vec<u8>)],
) -> Result<Option<String>, ConnectionError> {
    if let Some((id, _)) = objects
        .iter()
        .find(|(id, data)| ObjectId::of_canonical_bytes(data) != *id)
    {
        return Ok(Some(format!("object identity mismatch for {id}")));
    }
    match staged.put_batch(objects, OBJECT_WRITE_CONCURRENCY).await {
        Ok(()) => Ok(None),
        Err(error @ ObjectStoreError::InvalidFormat(_)) => Ok(Some(error.to_string())),
        Err(error) => Err(ConnectionError::Internal(format!(
            "failed to store objects: {error}"
        ))),
    }
}

/// Gets the content of `missing` from the client, until the stage has it all.
///
/// Asks in rounds of at most `MAX_WANT_IDS`, one `ContentNeeded` each, and stages
/// each blob as it arrives; the stage bounds what the session holds. A round's
/// answer may leave out blobs it sends as manifests once they pass
/// `MAX_ANSWER_MANIFEST_BYTES`; those are asked for again. Before anything new, the
/// parts and chunks the round's manifests name are asked for, in rounds that must
/// be answered in full, and each such blob is then checked and its manifest stored.
/// Returns why the publish is refused, if it is.
#[tracing::instrument(name = "publish_content", level = "debug", skip_all)]
async fn receive_content(
    ws: &mut impl Transport,
    staged: &Arc<Staged>,
    op_id: u64,
    mut queue: VecDeque<ContentId>,
    base_map: &Arc<HashMap<ContentId, ContentId>>,
    manifests: &mut Vec<ContentId>,
) -> Result<Option<String>, ConnectionError> {
    let mut arrivals = ManifestArrivals::new(MAX_ANSWER_MANIFEST_BYTES);
    // Parts and chunks are stored whole.
    let no_bases = Arc::default();
    loop {
        // The pieces of the held manifests come before anything new.
        let pieces = !arrivals.is_empty();
        let round: Vec<ContentId> = if pieces {
            let (back, next) = {
                let staged = Arc::clone(staged);
                drive(move || async move {
                    let next = arrivals.next_round(staged.content(), MAX_WANT_IDS);
                    (arrivals, next)
                })
                .await?
            };
            arrivals = back;
            let next = next.map_err(|e| {
                ConnectionError::Internal(format!("content lookup failed: {}", chain(&e)))
            })?;
            if next.is_empty() {
                let held = arrivals.take();
                manifests.extend(held.iter().map(|(id, _)| *id));
                if let Some(refusal) = store_manifests(staged, held).await? {
                    return Ok(Some(refusal));
                }
                continue;
            }
            next
        } else if queue.is_empty() {
            return Ok(None);
        } else {
            let take = queue.len().min(MAX_WANT_IDS);
            queue.drain(..take).collect()
        };

        send(
            ws,
            &WheatProtocolMessage::ContentNeeded(ContentNeededOutput {
                message_type: 0x43,
                op_id,
                content_ids: round
                    .iter()
                    .map(|id| Digest256 {
                        bytes: id.as_bytes().to_vec(),
                    })
                    .collect(),
            }),
        )
        .await?;
        let mut receiver = TransferReceiver::new(round.iter().copied(), MAX_CONTENT_SIZE);
        loop {
            let received: Vec<(ContentId, Vec<u8>)> = match recv(ws).await? {
                WheatProtocolMessage::ContentBatch(batch) if batch.op_id == op_id => {
                    let mut blobs = Vec::with_capacity(batch.contents.len());
                    for blob in batch.contents {
                        let id = digest_to_content_id(&blob.id)?;
                        if let Err(e) = receiver.accept_whole(id, blob.data.len()) {
                            return Ok(Some(e.to_string()));
                        }
                        blobs.push((id, blob.data));
                    }
                    blobs
                }
                WheatProtocolMessage::ContentChunk(chunk) if chunk.op_id == op_id => {
                    let id = digest_to_content_id(&chunk.content_id)?;
                    match receiver.accept_chunk(id, chunk.offset, chunk.data, chunk.is_final) {
                        Ok(finished) => finished.into_iter().collect(),
                        Err(e) => return Ok(Some(e.to_string())),
                    }
                }
                WheatProtocolMessage::ContentManifest(sent) if sent.op_id == op_id => {
                    if pieces {
                        return Ok(Some("a part or chunk was sent as a manifest".to_owned()));
                    }
                    let id = digest_to_content_id(&sent.content_id)?;
                    if let Err(e) = receiver.accept_whole(id, sent.manifest.len()) {
                        return Ok(Some(e.to_string()));
                    }
                    if let Err(e) = arrivals.hold(id, &sent.manifest) {
                        return Ok(Some(e.to_string()));
                    }
                    Vec::new()
                }
                WheatProtocolMessage::ContentUploadComplete(cuc) if cuc.op_id == op_id => {
                    debug!(op_id, "publish: content upload round complete");
                    let left = match receiver.finish_partial() {
                        Ok(left) => left,
                        Err(e) => return Ok(Some(e.to_string())),
                    };
                    if !left.is_empty() {
                        if pieces {
                            return Ok(Some(format!(
                                "{} requested parts or chunks were not sent",
                                left.len()
                            )));
                        }
                        if left.len() == round.len() {
                            return Ok(Some("a content round was answered with nothing".into()));
                        }
                        // Left out to keep the answer's manifests in bounds: ask again
                        // once the held ones are stored.
                        for id in round.iter().rev().filter(|id| left.contains(id)) {
                            queue.push_front(*id);
                        }
                    }
                    break;
                }
                _ => {
                    warn!(op_id, "publish: unexpected message during content upload");
                    Vec::new()
                }
            };
            let base_map = if pieces { &no_bases } else { base_map };
            if !stage_content(staged, base_map, received).await? {
                return Ok(Some("content identity mismatch".into()));
            }
        }
    }
}

/// Checks each blob of `manifests` against its ID and stores its manifest, as one
/// job. Returns why the publish is refused when a blob's chunks are not its bytes.
async fn store_manifests(
    staged: &Arc<Staged>,
    manifests: Vec<(ContentId, wheat_core::ContentChunkManifestRecord)>,
) -> Result<Option<String>, ConnectionError> {
    let staged = Arc::clone(staged);
    drive(move || async move {
        for (id, manifest) in &manifests {
            match staged.put_manifest(*id, manifest).await {
                Ok(()) => {}
                Err(wheat_store::RepositoryStoreError::Content(
                    error @ (wheat_store::ContentStoreError::Corrupt { .. }
                    | wheat_store::ContentStoreError::InvalidFormat { .. }),
                )) => {
                    return Ok(Some(format!(
                        "content {id} does not match its chunks: {error}"
                    )));
                }
                Err(error) => {
                    return Err(ConnectionError::Internal(format!(
                        "failed to store the manifest of {id}: {}",
                        chain(&error)
                    )));
                }
            }
        }
        Ok(None)
    })
    .await?
}

/// The content a publish still has to ask for, and the delta base of each.
struct ContentPlan {
    missing: Vec<ContentId>,
    base_map: Arc<HashMap<ContentId, ContentId>>,
    /// When the network is told what publishes rely on ([`crate::naming`]): the
    /// content it adds that was stored already, the bases of what it uploads, and
    /// what it uploads, pushed or asked for.
    relied_on: Vec<ContentId>,
    bases: Vec<ContentId>,
    uploaded: Vec<ContentId>,
}

/// Finds the content `proposed` adds over `current` that `network` lacks, and stages
/// what of it the client `pushed`. `None` when a pushed blob is not the content its
/// ID names.
async fn plan_content(
    staged: &Staged,
    network: &Network,
    current: RepositoryEnvelopeId,
    proposed: RepositoryEnvelopeId,
    mut pushed: HashMap<ContentId, Vec<u8>>,
) -> Result<Option<ContentPlan>, ConnectionError> {
    let reader = RepositoryReader::new(staged, staged.content());
    let envelope_record = reader.envelope(proposed).await.map_err(|e| {
        ConnectionError::Internal(format!("failed to read proposed envelope: {}", chain(&e)))
    })?;
    let state = reader.state(envelope_record.state).await.map_err(|e| {
        ConnectionError::Internal(format!("failed to read proposed state: {}", chain(&e)))
    })?;
    let current_envelope = reader.envelope(current).await.map_err(|e| {
        ConnectionError::Internal(format!("failed to read current envelope: {}", chain(&e)))
    })?;
    let current_state = reader.state(current_envelope.state).await.map_err(|e| {
        ConnectionError::Internal(format!("failed to read current state: {}", chain(&e)))
    })?;

    // A head is only accepted once all its content is stored, so only content the
    // current head lacks can be missing (transition validation relies on the same).
    // Bulk-check those: for S3 backends this does one LIST call instead of one GET
    // per item.
    let candidates = wheat_store::ContentSetStore::new(staged)
        .added(current_state.required_content, state.required_content)
        .await
        .map_err(|e| {
            ConnectionError::Internal(format!("failed to compare content sets: {}", chain(&e)))
        })?;
    let existing = network.content.existing_ids(&candidates).map_err(|e| {
        ConnectionError::Internal(format!("content existence check failed: {}", chain(&e)))
    })?;
    let missing: Vec<ContentId> = candidates
        .iter()
        .filter(|id| !existing.contains(id))
        .copied()
        .collect();
    let relied_on: Vec<ContentId> = if network.naming.is_some() {
        existing.into_iter().collect()
    } else {
        Vec::new()
    };
    if missing.is_empty() {
        return Ok(Some(ContentPlan {
            missing,
            base_map: Arc::default(),
            relied_on,
            bases: Vec::new(),
            uploaded: Vec::new(),
        }));
    }

    // Diff the old and new trees to find which files changed, then use
    // the old ContentId as a delta base for each new ContentId. This
    // produces much smaller stored objects for typical edits.
    let base_map = compute_delta_base_map(staged, &reader, &current_state, &state).await;
    let (bases, uploaded) = if network.naming.is_some() {
        let bases = missing
            .iter()
            .filter_map(|id| base_map.get(id).copied())
            .collect();
        (bases, missing.clone())
    } else {
        Default::default()
    };

    // Take what the client pushed; ask only for the rest. What the walk did not
    // need is dropped with `pushed`, rather than held to the commit.
    let mut asked = Vec::with_capacity(missing.len());
    for id in missing {
        let Some(data) = pushed.remove(&id) else {
            asked.push(id);
            continue;
        };
        let Ok(content) = VerifiedContent::check(id, data) else {
            return Ok(None);
        };
        let base = base_map.get(&id).copied();
        staged.put_content([(content, base)]).await.map_err(|e| {
            ConnectionError::Internal(format!("failed to store content: {}", chain(&e)))
        })?;
    }
    Ok(Some(ContentPlan {
        missing: asked,
        base_map: Arc::new(base_map),
        relied_on,
        bases,
        uploaded,
    }))
}

/// Stages content received in a round, as one job. Returns false when a blob is not
/// the content its ID names.
async fn stage_content(
    staged: &Arc<Staged>,
    base_map: &Arc<HashMap<ContentId, ContentId>>,
    received: Vec<(ContentId, Vec<u8>)>,
) -> Result<bool, ConnectionError> {
    if received.is_empty() {
        return Ok(true);
    }
    let (staged, base_map) = (Arc::clone(staged), Arc::clone(base_map));
    drive(move || async move {
        let Ok(received) = received
            .into_iter()
            .map(|(id, data)| VerifiedContent::check(id, data))
            .collect::<Result<Vec<_>, _>>()
        else {
            return Ok(false);
        };
        let blobs = received.into_iter().map(|content| {
            let base = base_map.get(&content.id()).copied();
            (content, base)
        });
        staged.put_content(blobs).await.map_err(|e| {
            ConnectionError::Internal(format!("failed to store content batch: {}", chain(&e)))
        })?;
        Ok(true)
    })
    .await?
}

/// Diff old and new Merkle trees to find `new_content → old_content` base
/// mappings for delta compression. Falls back to an empty map on any error
/// (delta compression is an optimization, not a correctness requirement).
async fn compute_delta_base_map(
    objects: &Staged,
    reader: &RepositoryReader<'_, Staged>,
    old_state: &wheat_core::RepositoryStateRecord,
    new_state: &wheat_core::RepositoryStateRecord,
) -> HashMap<ContentId, ContentId> {
    let mut base_map = HashMap::new();

    let old_view = match reader.view(old_state.base_view).await {
        Ok(v) => v,
        Err(_) => return base_map,
    };
    let new_view = match reader.view(new_state.base_view).await {
        Ok(v) => v,
        Err(_) => return base_map,
    };

    if old_view.tree == new_view.tree {
        return base_map; // No tree changes.
    }

    // Walk the diff — O(changed files), skips identical subtrees.
    let trees = MerkleTreeStore::new(objects);
    let mut diff_cursor =
        trees.diff_cursor(old_view.tree, new_view.tree, TreeDiffLimits::default());
    while let Ok(Some(diff)) = diff_cursor.next().await {
        if let (Some(old_value), Some(new_value)) = (&diff.old, &diff.new)
            && old_value.content != new_value.content
        {
            base_map.insert(new_value.content, old_value.content);
        }
    }

    base_map
}
