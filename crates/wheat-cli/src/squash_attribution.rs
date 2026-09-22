//! Deterministic revision metadata for squash operations.
//!
//! This module has no command-layer dependencies so squash execution can use
//! one canonical attribution policy regardless of where its checkpoints came
//! from.

use std::collections::BTreeSet;

use anyhow::Result;
use wheat_core::{Actor, RevisionMetadata, Timestamp};

use crate::authoring_metadata::validate_revision_message;

/// Builds squash metadata from the latest revision and its superseded authors.
///
/// The latest message and provenance are retained. The squasher becomes both
/// author and committer at `now`. Each distinct non-squasher superseded author
/// that is not already represented by an equivalent trailer is appended in
/// deterministic `(name, email)` byte order.
#[allow(dead_code)] // Registered for squash command integration; intentionally command-independent.
pub(crate) fn attributed_squash_metadata<'a>(
    latest: &RevisionMetadata,
    superseded_authors: impl IntoIterator<Item = &'a Actor>,
    squasher: &Actor,
    now: Timestamp,
) -> Result<RevisionMetadata> {
    let existing_trailers = existing_coauthors(&latest.message);
    let authors = superseded_authors
        .into_iter()
        .filter(|author| *author != squasher)
        .filter(|author| !existing_trailers.contains(&actor_key(author)))
        .map(actor_key)
        .collect::<BTreeSet<_>>();

    let mut message = latest.message.clone();
    if !authors.is_empty() {
        append_trailer_separator(&mut message);
        for (name, email) in authors {
            if !message.ends_with('\n') && !message.is_empty() {
                message.push('\n');
            }
            message.push_str("Co-authored-by: ");
            message.push_str(&name);
            message.push_str(" <");
            message.push_str(&email);
            message.push('>');
        }
    }
    validate_revision_message(&message, false)?;

    Ok(RevisionMetadata {
        message,
        author: squasher.clone(),
        authored_at: now,
        committer: squasher.clone(),
        committed_at: now,
        provenance: latest.provenance.clone(),
    })
}

fn actor_key(actor: &Actor) -> (String, String) {
    (actor.name.clone(), actor.email.clone())
}

fn existing_coauthors(message: &str) -> BTreeSet<(String, String)> {
    message.lines().filter_map(parse_coauthor_trailer).collect()
}

fn parse_coauthor_trailer(line: &str) -> Option<(String, String)> {
    let (key, value) = line.split_once(':')?;
    if !key.trim().eq_ignore_ascii_case("co-authored-by") {
        return None;
    }
    let value = value.trim();
    let (name, email) = value.rsplit_once('<')?;
    let email = email.strip_suffix('>')?.trim();
    let name = name.trim();
    (!name.is_empty() && !email.is_empty()).then(|| (name.to_owned(), email.to_owned()))
}

fn append_trailer_separator(message: &mut String) {
    if message.is_empty() || message.ends_with("\n\n") || existing_coauthors(message).is_empty() {
        if !message.is_empty() && !message.ends_with("\n\n") {
            if !message.ends_with('\n') {
                message.push('\n');
            }
            message.push('\n');
        }
        return;
    }
    if !message.ends_with('\n') {
        message.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wheat_core::SourceProvenance;

    fn actor(name: &str, email: &str) -> Actor {
        Actor {
            name: name.to_owned(),
            email: email.to_owned(),
        }
    }

    fn latest(message: &str) -> RevisionMetadata {
        RevisionMetadata {
            message: message.to_owned(),
            author: actor("Original", "original@example.invalid"),
            authored_at: Timestamp {
                seconds: 1,
                offset_minutes: 60,
            },
            committer: actor("Original Committer", "committer@example.invalid"),
            committed_at: Timestamp {
                seconds: 2,
                offset_minutes: 120,
            },
            provenance: Some(SourceProvenance {
                system: "git".to_owned(),
                object_id: vec![1],
                parent_object_ids: vec![vec![2]],
            }),
        }
    }

    #[test]
    fn replaces_identity_and_times_while_retaining_message_and_provenance() {
        let latest = latest("Keep this message");
        let squasher = actor("Squasher", "squasher@example.invalid");
        let now = Timestamp {
            seconds: 42,
            offset_minutes: -300,
        };

        let result = attributed_squash_metadata(&latest, [], &squasher, now).unwrap();

        assert_eq!(result.message, latest.message);
        assert_eq!(result.author, squasher);
        assert_eq!(result.committer, squasher);
        assert_eq!(result.authored_at, now);
        assert_eq!(result.committed_at, now);
        assert_eq!(result.provenance, latest.provenance);
    }

    #[test]
    fn omits_the_squasher_and_deduplicates_superseded_authors() {
        let squasher = actor("Squasher", "squasher@example.invalid");
        let alice = actor("Alice", "alice@example.invalid");
        let authors = [&squasher, &alice, &alice, &squasher];

        let result = attributed_squash_metadata(
            &latest("Subject"),
            authors,
            &squasher,
            Timestamp::default(),
        )
        .unwrap();

        assert_eq!(
            result.message,
            "Subject\n\nCo-authored-by: Alice <alice@example.invalid>"
        );
    }

    #[test]
    fn recognizes_equivalent_existing_trailers_without_repeating_them() {
        let alice = actor("Alice", "alice@example.invalid");
        let bob = actor("Bob", "bob@example.invalid");
        let squasher = actor("Squasher", "squasher@example.invalid");
        let result = attributed_squash_metadata(
            &latest("Subject\n\nco-authored-by: Alice <alice@example.invalid>"),
            [&alice, &bob, &alice],
            &squasher,
            Timestamp::default(),
        )
        .unwrap();

        assert_eq!(
            result.message,
            "Subject\n\nco-authored-by: Alice <alice@example.invalid>\nCo-authored-by: Bob <bob@example.invalid>"
        );
    }

    #[test]
    fn appends_a_trailer_block_after_exactly_one_blank_separator() {
        let alice = actor("Alice", "alice@example.invalid");
        let squasher = actor("Squasher", "squasher@example.invalid");

        let result = attributed_squash_metadata(
            &latest("Subject\n\nBody"),
            [&alice],
            &squasher,
            Timestamp::default(),
        )
        .unwrap();

        assert_eq!(
            result.message,
            "Subject\n\nBody\n\nCo-authored-by: Alice <alice@example.invalid>"
        );
    }

    #[test]
    fn orders_new_trailers_by_name_then_email() {
        let squasher = actor("Squasher", "squasher@example.invalid");
        let authors = [
            actor("Zoe", "zoe@example.invalid"),
            actor("Alice", "z@example.invalid"),
            actor("Alice", "a@example.invalid"),
        ];
        let result = attributed_squash_metadata(
            &latest("Subject"),
            authors.iter(),
            &squasher,
            Timestamp::default(),
        )
        .unwrap();

        assert_eq!(
            result.message,
            "Subject\n\nCo-authored-by: Alice <a@example.invalid>\nCo-authored-by: Alice <z@example.invalid>\nCo-authored-by: Zoe <zoe@example.invalid>"
        );
    }

    #[test]
    fn rejects_a_trailer_addition_that_exceeds_the_revision_message_limit() {
        let squasher = actor("Squasher", "squasher@example.invalid");
        let alice = actor("Alice", "alice@example.invalid");
        let message = "x".repeat(1024 * 1024);

        let error = attributed_squash_metadata(
            &latest(&message),
            [&alice],
            &squasher,
            Timestamp::default(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("revision message exceeds"));
    }
}
