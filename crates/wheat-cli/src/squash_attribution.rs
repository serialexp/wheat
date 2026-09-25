//! Deterministic revision metadata for squash operations.
//!
//! This module has no command-layer dependencies so squash execution can use
//! one canonical attribution policy regardless of where its checkpoints came
//! from.

use std::collections::BTreeSet;

use anyhow::{Result, anyhow};
use wheat_core::{
    Actor, RevisionMetadata, Timestamp,
    message::{fold_messages, parse_coauthor_trailer},
};

use crate::authoring_metadata::validate_revision_message;

/// Whether squash credits superseded checkpoint authors with `Co-authored-by` trailers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Coauthors {
    Credit,
    /// `--no-coauthors`: for example when the superseded author is the squasher under an
    /// old identity. Trailers already present in the folded messages are kept.
    Omit,
}

/// Builds squash metadata from a collapsed checkpoint suffix, oldest first.
///
/// The notes of every checkpoint are folded into one message with
/// [`fold_messages`], so a change's title note and later notes survive the collapse. The
/// latest checkpoint's provenance is retained. The squasher becomes both author and
/// committer at `now`. Unless `coauthors` is [`Coauthors::Omit`], each distinct
/// non-squasher checkpoint author that is not already represented by an equivalent trailer
/// is appended in deterministic `(name, email)` byte order.
pub(crate) fn attributed_squash_metadata(
    suffix: &[&RevisionMetadata],
    squasher: &Actor,
    now: Timestamp,
    coauthors: Coauthors,
) -> Result<RevisionMetadata> {
    let latest = suffix
        .last()
        .copied()
        .ok_or_else(|| anyhow!("squash suffix must contain the latest checkpoint"))?;
    let mut message = fold_messages(suffix.iter().map(|metadata| metadata.message.as_str()));
    // Bound the folded text before appending trailers: a long suffix of notes must fail as a
    // typed limit, never by silently truncating.
    validate_revision_message(&message, false)?;

    if coauthors == Coauthors::Credit {
        let existing_trailers = existing_coauthors(&message);
        let authors = suffix
            .iter()
            .map(|metadata| &metadata.author)
            .filter(|author| *author != squasher)
            .map(actor_key)
            .filter(|author| !existing_trailers.contains(author))
            .collect::<BTreeSet<_>>();
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

    fn checkpoint(message: &str, author: &Actor) -> RevisionMetadata {
        RevisionMetadata {
            message: message.to_owned(),
            author: author.clone(),
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

    fn squash(suffix: &[RevisionMetadata], squasher: &Actor) -> RevisionMetadata {
        let refs = suffix.iter().collect::<Vec<_>>();
        attributed_squash_metadata(&refs, squasher, Timestamp::default(), Coauthors::Credit)
            .unwrap()
    }

    #[test]
    fn replaces_identity_and_times_while_retaining_message_and_provenance() {
        let squasher = actor("Squasher", "squasher@example.invalid");
        let latest = checkpoint("Keep this message", &squasher);
        let now = Timestamp {
            seconds: 42,
            offset_minutes: -300,
        };

        let result =
            attributed_squash_metadata(&[&latest], &squasher, now, Coauthors::Credit).unwrap();

        assert_eq!(result.message, latest.message);
        assert_eq!(result.author, squasher);
        assert_eq!(result.committer, squasher);
        assert_eq!(result.authored_at, now);
        assert_eq!(result.committed_at, now);
        assert_eq!(result.provenance, latest.provenance);
    }

    #[test]
    fn folds_every_checkpoint_note_oldest_first() {
        let squasher = actor("Squasher", "squasher@example.invalid");
        let suffix = [
            checkpoint("Title\n\nBody", &squasher),
            checkpoint("", &squasher),
            checkpoint("Fix the typo", &squasher),
            checkpoint("Title\n\nBody", &squasher),
        ];
        assert_eq!(
            squash(&suffix, &squasher).message,
            "Title\n\nBody\n\nFix the typo"
        );
    }

    #[test]
    fn a_title_note_followed_by_empty_checkpoints_keeps_the_title() {
        let squasher = actor("Squasher", "squasher@example.invalid");
        let suffix = [
            checkpoint("Initial import", &squasher),
            checkpoint("", &squasher),
            checkpoint("", &squasher),
        ];
        assert_eq!(squash(&suffix, &squasher).message, "Initial import");
    }

    #[test]
    fn omits_the_squasher_and_deduplicates_superseded_authors() {
        let squasher = actor("Squasher", "squasher@example.invalid");
        let alice = actor("Alice", "alice@example.invalid");
        let suffix = [
            checkpoint("", &squasher),
            checkpoint("", &alice),
            checkpoint("", &alice),
            checkpoint("Subject", &squasher),
        ];

        assert_eq!(
            squash(&suffix, &squasher).message,
            "Subject\n\nCo-authored-by: Alice <alice@example.invalid>"
        );
    }

    #[test]
    fn no_coauthors_skips_new_trailers_but_keeps_existing_ones() {
        let squasher = actor("Bart", "bart@new.invalid");
        let old_self = actor("Bart", "bart@old.invalid");
        let alice = actor("Alice", "alice@example.invalid");
        let first = checkpoint("Title", &old_self);
        let second = checkpoint(
            "Note\n\nCo-authored-by: Alice <alice@example.invalid>",
            &alice,
        );

        let result = attributed_squash_metadata(
            &[&first, &second],
            &squasher,
            Timestamp::default(),
            Coauthors::Omit,
        )
        .unwrap();

        assert_eq!(
            result.message,
            "Title\n\nNote\n\nCo-authored-by: Alice <alice@example.invalid>"
        );
        assert_eq!(result.author, squasher);
    }

    #[test]
    fn recognizes_equivalent_existing_trailers_without_repeating_them() {
        let alice = actor("Alice", "alice@example.invalid");
        let bob = actor("Bob", "bob@example.invalid");
        let squasher = actor("Squasher", "squasher@example.invalid");
        let suffix = [
            checkpoint("", &alice),
            checkpoint("", &bob),
            checkpoint(
                "Subject\n\nco-authored-by: Alice <alice@example.invalid>",
                &alice,
            ),
        ];

        assert_eq!(
            squash(&suffix, &squasher).message,
            "Subject\n\nco-authored-by: Alice <alice@example.invalid>\nCo-authored-by: Bob <bob@example.invalid>"
        );
    }

    #[test]
    fn appends_a_trailer_block_after_exactly_one_blank_separator() {
        let alice = actor("Alice", "alice@example.invalid");
        let squasher = actor("Squasher", "squasher@example.invalid");

        assert_eq!(
            squash(&[checkpoint("Subject\n\nBody", &alice)], &squasher).message,
            "Subject\n\nBody\n\nCo-authored-by: Alice <alice@example.invalid>"
        );
    }

    #[test]
    fn orders_new_trailers_by_name_then_email() {
        let squasher = actor("Squasher", "squasher@example.invalid");
        let suffix = [
            checkpoint("", &actor("Zoe", "zoe@example.invalid")),
            checkpoint("", &actor("Alice", "z@example.invalid")),
            checkpoint("Subject", &actor("Alice", "a@example.invalid")),
        ];

        assert_eq!(
            squash(&suffix, &squasher).message,
            "Subject\n\nCo-authored-by: Alice <a@example.invalid>\nCo-authored-by: Alice <z@example.invalid>\nCo-authored-by: Zoe <zoe@example.invalid>"
        );
    }

    #[test]
    fn rejects_a_folded_message_that_exceeds_the_revision_message_limit() {
        let squasher = actor("Squasher", "squasher@example.invalid");
        let half = "x".repeat(600 * 1024);
        let other = "y".repeat(600 * 1024);
        let first = checkpoint(&half, &squasher);
        let second = checkpoint(&other, &squasher);

        let error = attributed_squash_metadata(
            &[&first, &second],
            &squasher,
            Timestamp::default(),
            Coauthors::Credit,
        )
        .unwrap_err();

        assert!(error.to_string().contains("revision message exceeds"));
    }

    #[test]
    fn rejects_a_trailer_addition_that_exceeds_the_revision_message_limit() {
        let squasher = actor("Squasher", "squasher@example.invalid");
        let alice = actor("Alice", "alice@example.invalid");
        let latest = checkpoint(&"x".repeat(1024 * 1024), &alice);

        let error = attributed_squash_metadata(
            &[&latest],
            &squasher,
            Timestamp::default(),
            Coauthors::Credit,
        )
        .unwrap_err();

        assert!(error.to_string().contains("revision message exceeds"));
    }
}
