//! Mirroring a Wheat remote into a served repository: the repository's head becomes
//! the remote's, fetched incrementally, with the content it relies on named, and
//! replacing whatever was published to the repository meanwhile.

use crate::support;

use std::collections::BTreeSet;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use wheat_core::{
    ChangeId, ChangeMetadata, ChangeProposal, ContentId, Repository, RevisionEdits,
    RevisionMetadata, Tree, VerifiedContent, ViewId, ViewLabel,
};
use wheat_remote::{WheatUrl, parse_wheat_url};
use wheat_server::access::Principal;
use wheat_server::mirror::{MirrorFailure, mirror, mirror_over};
use wheat_server::naming::ContentNaming;
use wheat_server::network::Network;
use wheat_server::registry::{Registry, RepositoryEntry};
use wheat_server::server::{ServerConfig, run_until};
use wheat_server::write::{
    ExpectedLabel::{self, Absent, View},
    LabelChange, ServerWrite, WriteFailure, open_stage, write,
};
use wheat_store::{
    LocalPublicationStore, MerkleTreeStore, PackedObjectStore, RepositoryHead, RepositoryPublisher,
    RepositoryReader,
};

use support::edit;

const UPSTREAM: &str = "test/upstream";

/// Records each naming.
#[derive(Default)]
struct Recording {
    calls: Mutex<Vec<Vec<ContentId>>>,
}

#[async_trait]
impl ContentNaming for Recording {
    fn renew_after(&self) -> Duration {
        Duration::from_secs(3600)
    }

    async fn named(&self, ids: &[ContentId]) -> io::Result<()> {
        self.calls.lock().unwrap().push(ids.to_vec());
        Ok(())
    }
}

/// A repository on a packed store, holding only its base view.
struct Repo {
    _dir: tempfile::TempDir,
    objects: PackedObjectStore,
    base: ViewId,
}

async fn repository() -> (Repo, LocalPublicationStore) {
    let dir = tempfile::tempdir().unwrap();
    let objects = PackedObjectStore::open(dir.path().join("storage")).unwrap();
    let content = objects.content_store();
    let heads = LocalPublicationStore::new(dir.path().join("repository.head"));
    let repo = Repository::new(Tree::new());
    let base = repo.base_view().id;
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repo)
        .await
        .unwrap();
    (
        Repo {
            _dir: dir,
            objects,
            base,
        },
        heads,
    )
}

/// The served remote: its repository, its entry to write to, and its URL.
struct Upstream {
    repo: Repo,
    entry: Arc<RepositoryEntry>,
    addr: SocketAddr,
    url: WheatUrl,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

async fn upstream() -> Upstream {
    upstream_with(ServerConfig::new(), "wheat").await
}

/// [`upstream`], served with `config`, its URL with `scheme`.
async fn upstream_with(config: ServerConfig, scheme: &str) -> Upstream {
    let (repo, heads) = repository().await;
    let entry = RepositoryEntry::in_network(Network::new(Box::new(repo.objects.clone())), heads);
    let registry = Arc::new(Registry::single_entry(UPSTREAM.to_string(), entry));
    let entry = registry.get(UPSTREAM).await.unwrap().unwrap();
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready, ready_rx) = tokio::sync::oneshot::channel();
    let any_port: SocketAddr = "127.0.0.1:0".parse().unwrap();
    tokio::spawn(async move {
        run_until(any_port, registry, config, ready, shutdown_rx)
            .await
            .unwrap();
    });
    let addr = ready_rx.await.unwrap();
    Upstream {
        repo,
        entry,
        addr,
        url: parse_wheat_url(&format!("{scheme}://{addr}/{UPSTREAM}")).unwrap(),
        _shutdown: shutdown,
    }
}

/// The repository mirroring the remote, whose network tells `naming`.
struct Mirror {
    repo: Repo,
    entry: Arc<RepositoryEntry>,
    naming: Arc<Recording>,
}

async fn mirror_repository() -> Mirror {
    let (repo, heads) = repository().await;
    let naming = Arc::new(Recording::default());
    let network = Network::with_naming(Box::new(repo.objects.clone()), naming.clone());
    Mirror {
        entry: Arc::new(RepositoryEntry::in_network(network, heads)),
        repo,
        naming,
    }
}

fn principal() -> Principal {
    Principal {
        name: "gothab".into(),
    }
}

fn id(bytes: &[u8]) -> ContentId {
    VerifiedContent::new(bytes).id()
}

fn change(name: &str, expected: ExpectedLabel, new: ViewId) -> LabelChange {
    LabelChange {
        name: name.to_owned(),
        expected,
        new: Some(ViewLabel::from(new)),
    }
}

fn server_write(key: u8, labels: Vec<LabelChange>) -> ServerWrite {
    ServerWrite {
        principal: principal(),
        key: [key; 16],
        labels,
        force_labels: false,
    }
}

fn proposal(edits: RevisionEdits) -> ChangeProposal {
    ChangeProposal {
        id: ChangeId::new(),
        metadata: ChangeMetadata::default(),
        dependencies: BTreeSet::new(),
        revision_metadata: RevisionMetadata::default(),
        edits,
    }
}

/// Publishes to `entry` a change writing `path` on `view`, moving `main` from
/// `expected` to it; answers its view.
async fn commit(
    entry: &Arc<RepositoryEntry>,
    key: u8,
    view: ViewId,
    expected: ExpectedLabel,
    path: &'static str,
    value: &'static [u8],
) -> ViewId {
    let stage = open_stage(entry).await.unwrap();
    let edited = stage
        .build(move |builder| async move {
            builder.content().put(value).unwrap();
            let mutator = builder.mutator();
            let (prepared, outcome) = mutator
                .create_on_frontier(builder.head(), view, view, proposal(edit(path, value)))
                .await
                .unwrap();
            mutator.publish(prepared).await.unwrap();
            outcome.resulting_view
        })
        .await
        .unwrap();
    stage
        .submit(
            server_write(key, vec![change("main", expected, edited)]),
            [key; 32],
        )
        .await
        .unwrap();
    edited
}

async fn head(entry: &RepositoryEntry) -> RepositoryHead {
    entry.heads.head().await.unwrap()
}

/// The labels `head` holds in `repo`, by name.
async fn labels(repo: &Repo, head: RepositoryHead) -> Vec<(String, ViewId)> {
    let content = repo.objects.content_store();
    let reader = RepositoryReader::new(&repo.objects, &content);
    let envelope = reader.envelope(head.envelope).await.unwrap();
    reader
        .view_label_manifest(envelope.labels)
        .await
        .unwrap()
        .labels
        .into_iter()
        .map(|(name, label)| (name, label.view))
        .collect()
}

/// What `view` holds at `path` in `repo`, read back whole.
async fn file(repo: &Repo, view: ViewId, path: &str) -> Option<Vec<u8>> {
    let content = repo.objects.content_store();
    let tree = RepositoryReader::new(&repo.objects, &content)
        .view(view)
        .await
        .unwrap()
        .tree;
    let value = MerkleTreeStore::new(&repo.objects)
        .lookup_many(tree, &[path])
        .await
        .unwrap()
        .pop()
        .unwrap()?;
    Some(content.get(value.content).unwrap())
}

#[tokio::test]
async fn a_mirror_makes_the_head_the_remotes_with_its_content_named() {
    let upstream = upstream().await;
    let one = commit(
        &upstream.entry,
        1,
        upstream.repo.base,
        Absent,
        "one.txt",
        b"one",
    )
    .await;
    let mirror_repo = mirror_repository().await;

    let mirrored = mirror(&mirror_repo.entry, &principal(), &upstream.url, b"")
        .await
        .unwrap();

    assert!(mirrored.moved);
    let remote = head(&upstream.entry).await;
    assert_eq!(mirrored.head, head(&mirror_repo.entry).await);
    assert_eq!(mirrored.head.envelope, remote.envelope);
    assert_eq!(
        labels(&mirror_repo.repo, mirrored.head).await,
        vec![("main".to_string(), one)]
    );
    let content = mirror_repo.repo.objects.content_store();
    RepositoryReader::new(&mirror_repo.repo.objects, &content)
        .validate_envelope(mirrored.head.envelope)
        .await
        .unwrap();
    assert_eq!(
        file(&mirror_repo.repo, one, "one.txt").await,
        Some(b"one".to_vec())
    );
    assert_eq!(mirrored.content.fetched, 1);
    let named = mirror_repo.naming.calls.lock().unwrap().concat();
    assert!(
        named.contains(&id(b"one")),
        "the mirrored content was named: {named:?}"
    );
}

#[tokio::test]
async fn a_mirror_fetches_what_the_remote_added_since_the_last_and_nothing_when_current() {
    let upstream = upstream().await;
    let one = commit(
        &upstream.entry,
        1,
        upstream.repo.base,
        Absent,
        "one.txt",
        b"one",
    )
    .await;
    let mirror_repo = mirror_repository().await;
    let first = mirror(&mirror_repo.entry, &principal(), &upstream.url, b"")
        .await
        .unwrap();

    let again = mirror(&mirror_repo.entry, &principal(), &upstream.url, b"")
        .await
        .unwrap();
    assert!(!again.moved, "the remote has not moved");
    assert_eq!(again.head, first.head);
    assert_eq!(again.objects.pushed + again.objects.pulled, 0);

    let two = commit(&upstream.entry, 2, one, View(one), "two.txt", b"two").await;
    mirror_repo.naming.calls.lock().unwrap().clear();
    let second = mirror(&mirror_repo.entry, &principal(), &upstream.url, b"")
        .await
        .unwrap();
    assert!(second.moved);
    assert_eq!(second.head.envelope, head(&upstream.entry).await.envelope);
    assert!(second.objects.incremental, "{:?}", second.objects);
    assert_eq!(second.content.fetched, 1, "only two.txt's content is new");
    assert_eq!(
        file(&mirror_repo.repo, two, "two.txt").await,
        Some(b"two".to_vec())
    );
    assert_eq!(
        file(&mirror_repo.repo, two, "one.txt").await,
        Some(b"one".to_vec())
    );
    let named = mirror_repo.naming.calls.lock().unwrap().concat();
    assert!(named.contains(&id(b"two")), "{named:?}");
    assert!(
        !named.contains(&id(b"one")),
        "what the head already relied on is not named again: {named:?}"
    );
}

#[tokio::test]
async fn a_mirror_replaces_what_was_published_to_the_repository() {
    let upstream = upstream().await;
    let one = commit(
        &upstream.entry,
        1,
        upstream.repo.base,
        Absent,
        "one.txt",
        b"one",
    )
    .await;
    let mirror_repo = mirror_repository().await;
    mirror(&mirror_repo.entry, &principal(), &upstream.url, b"")
        .await
        .unwrap();
    // A local change, and a local label, that the remote does not have.
    let local = commit(&mirror_repo.entry, 9, one, View(one), "local.txt", b"local").await;
    write(
        &mirror_repo.entry,
        server_write(10, vec![change("local", Absent, local)]),
    )
    .await
    .unwrap();
    assert_ne!(
        head(&mirror_repo.entry).await.envelope,
        head(&upstream.entry).await.envelope
    );

    let replaced = mirror(&mirror_repo.entry, &principal(), &upstream.url, b"")
        .await
        .unwrap();

    assert!(replaced.moved);
    assert_eq!(replaced.head.envelope, head(&upstream.entry).await.envelope);
    assert_eq!(
        labels(&mirror_repo.repo, replaced.head).await,
        vec![("main".to_string(), one)],
        "main moved back and the local label is gone"
    );
}

#[tokio::test]
async fn an_unreachable_remote_leaves_the_head() {
    let mirror_repo = mirror_repository().await;
    let before = head(&mirror_repo.entry).await;
    // A port nothing listens on: bound, then closed.
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let url = parse_wheat_url(&format!("wheat://{addr}/{UPSTREAM}")).unwrap();

    let failure = mirror(&mirror_repo.entry, &principal(), &url, b"")
        .await
        .unwrap_err();

    assert!(matches!(failure, MirrorFailure::Remote(_)), "{failure:?}");
    assert_eq!(head(&mirror_repo.entry).await, before);
}

#[tokio::test]
async fn a_quiesced_network_refuses_a_mirror() {
    let upstream = upstream().await;
    let mirror_repo = mirror_repository().await;
    let held = wheat_server::quiesce::Quiesced::hold(Arc::clone(&mirror_repo.entry)).await;

    let failure = mirror(&mirror_repo.entry, &principal(), &upstream.url, b"")
        .await
        .unwrap_err();

    assert!(
        matches!(failure, MirrorFailure::Write(WriteFailure::Quiesced { .. })),
        "{failure:?}"
    );
    drop(held);
}
#[tokio::test]
async fn two_mirrors_of_the_same_move_answer_the_same_head() {
    let upstream = upstream().await;
    commit(
        &upstream.entry,
        1,
        upstream.repo.base,
        Absent,
        "one.txt",
        b"one",
    )
    .await;
    let mirror_repo = mirror_repository().await;
    let before = head(&mirror_repo.entry).await;

    let gothab = principal();
    let (first, second) = tokio::join!(
        mirror(&mirror_repo.entry, &gothab, &upstream.url, b""),
        mirror(&mirror_repo.entry, &gothab, &upstream.url, b""),
    );

    // Whichever joins second finds the first's receipt, or the head already moved.
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(first.head, second.head);
    assert_eq!(first.head, head(&mirror_repo.entry).await);
    assert_eq!(first.head.generation, before.generation + 1, "moved once");
}
/// `url` with its authority replaced by `authority`.
fn at(url: &WheatUrl, scheme: &str, authority: &str) -> WheatUrl {
    parse_wheat_url(&format!("{scheme}://{authority}/{}", url.repo_name)).unwrap()
}

#[tokio::test]
async fn a_mirror_over_the_callers_connection_never_resolves_the_url() {
    let upstream = upstream().await;
    commit(
        &upstream.entry,
        1,
        upstream.repo.base,
        Absent,
        "one.txt",
        b"one",
    )
    .await;
    let mirror_repo = mirror_repository().await;
    // `.invalid` never resolves: only the caller's connection reaches the server.
    let url = at(
        &upstream.url,
        "wheat",
        &format!("upstream.invalid:{}", upstream.addr.port()),
    );
    let asked = Arc::new(Mutex::new(None));
    let (addr, record) = (upstream.addr, Arc::clone(&asked));

    let mirrored = mirror_over(
        &mirror_repo.entry,
        &principal(),
        &url,
        b"",
        move |url| async move {
            *record.lock().unwrap() = Some(url);
            tokio::net::TcpStream::connect(addr).await
        },
    )
    .await
    .unwrap();

    assert!(mirrored.moved);
    assert_eq!(mirrored.head.envelope, head(&upstream.entry).await.envelope);
    assert_eq!(asked.lock().unwrap().as_ref(), Some(&url));
}

#[tokio::test]
async fn a_connection_the_caller_refuses_leaves_the_head() {
    let upstream = upstream().await;
    let mirror_repo = mirror_repository().await;
    let before = head(&mirror_repo.entry).await;

    let failure = mirror_over(
        &mirror_repo.entry,
        &principal(),
        &upstream.url,
        b"",
        |_| async {
            Err::<tokio::net::TcpStream, _>(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "resolves to a private address",
            ))
        },
    )
    .await
    .unwrap_err();

    match failure {
        MirrorFailure::Remote(message) => {
            assert!(
                message.contains("resolves to a private address"),
                "{message}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(head(&mirror_repo.entry).await, before);
}

#[tokio::test]
async fn over_tls_the_certificate_is_checked_against_the_urls_host_not_the_address() {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert, generated.cert.pem()).unwrap();
    std::fs::write(&key, generated.signing_key.serialize_pem()).unwrap();
    // SAFETY: nextest runs each test in a process of its own, and nothing else in
    // this one reads the environment yet: the client reads its roots on first use.
    unsafe { std::env::set_var("SSL_CERT_FILE", &cert) };
    let mut config = ServerConfig::new();
    config.tls = Some(wheat_server::tls::load_pem(&cert, &key).unwrap());
    let upstream = upstream_with(config, "wheat+tls").await;
    let one = commit(
        &upstream.entry,
        1,
        upstream.repo.base,
        Absent,
        "one.txt",
        b"one",
    )
    .await;
    let mirror_repo = mirror_repository().await;
    let addr = upstream.addr;
    let to_server = move |_| async move { tokio::net::TcpStream::connect(addr).await };

    // Connected to 127.0.0.1, the certificate for `localhost` is the URL host's.
    let localhost = at(
        &upstream.url,
        "wheat+tls",
        &format!("localhost:{}", addr.port()),
    );
    let mirrored = mirror_over(&mirror_repo.entry, &principal(), &localhost, b"", to_server)
        .await
        .unwrap();
    assert_eq!(mirrored.head.envelope, head(&upstream.entry).await.envelope);

    // The same connection, for a URL naming another host, fails the check.
    commit(&upstream.entry, 2, one, View(one), "two.txt", b"two").await;
    let elsewhere = at(
        &upstream.url,
        "wheat+tls",
        &format!("upstream.invalid:{}", addr.port()),
    );
    let failure = mirror_over(&mirror_repo.entry, &principal(), &elsewhere, b"", to_server)
        .await
        .unwrap_err();
    match failure {
        MirrorFailure::Remote(message) => assert!(
            message.contains("certificate not valid for name \"upstream.invalid\""),
            "{message}"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(head(&mirror_repo.entry).await, mirrored.head);
}
