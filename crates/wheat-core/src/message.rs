//! Folding several revision messages into one commit-style message.
//!
//! A logical change carries its title and description, and each of its revisions may carry
//! a note. Squash folds a checkpoint suffix's notes into one replacement revision, and Git
//! projection folds a change's title, description, and notes into one commit message. Both
//! use [`fold_messages`] so the two agree on what a combined message looks like.

/// Parses one `Co-authored-by: Name <email>` trailer line, case-insensitively by key.
#[must_use]
pub fn parse_coauthor_trailer(line: &str) -> Option<(String, String)> {
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

/// Joins message parts, oldest first, into one message.
///
/// Each part's `Co-authored-by` trailer lines are lifted out and collected, in first-seen
/// order and without repeating an equivalent `(name, email)`, into one trailer block at the
/// end, where Git hosts look for them. The remaining text of each part is trimmed; empty
/// parts and exact repeats of an earlier part are skipped. Parts are separated by one blank
/// line.
#[must_use]
pub fn fold_messages<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let mut bodies: Vec<String> = Vec::new();
    let mut trailers: Vec<&'a str> = Vec::new();
    let mut trailer_keys = std::collections::BTreeSet::new();
    for part in parts {
        let mut body = String::new();
        for line in part.lines() {
            if let Some(key) = parse_coauthor_trailer(line) {
                if trailer_keys.insert(key) {
                    trailers.push(line.trim());
                }
                continue;
            }
            body.push_str(line.trim_end());
            body.push('\n');
        }
        let body = body.trim_matches('\n');
        if !body.is_empty() && !bodies.iter().any(|existing| existing == body) {
            bodies.push(body.to_owned());
        }
    }
    let mut message = bodies.join("\n\n");
    if !trailers.is_empty() {
        if !message.is_empty() {
            message.push_str("\n\n");
        }
        message.push_str(&trailers.join("\n"));
    }
    message
}

/// Removes a leading line equal to `title`, and the blank lines after it.
///
/// A change created with `-m "Title\n\nBody"` records the whole text as its first revision's
/// message and its first line as the change title; this recovers the body.
#[must_use]
pub fn strip_title_line<'a>(message: &'a str, title: &str) -> &'a str {
    let (first, rest) = message.split_once('\n').unwrap_or((message, ""));
    if first.trim() == title.trim() {
        rest.trim_start_matches(['\n', '\r'])
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_parts_in_order_and_skips_empty_and_repeated_ones() {
        assert_eq!(
            fold_messages(["Title", "", "  \n", "Body\n", "Title", "Note"]),
            "Title\n\nBody\n\nNote"
        );
    }

    #[test]
    fn lifts_trailers_into_one_deduplicated_block_at_the_end() {
        assert_eq!(
            fold_messages([
                "Title\n\nCo-authored-by: Alice <a@example.invalid>",
                "Note\nco-authored-by: Alice <a@example.invalid>\nCo-authored-by: Bob <b@example.invalid>",
            ]),
            "Title\n\nNote\n\nCo-authored-by: Alice <a@example.invalid>\nCo-authored-by: Bob <b@example.invalid>"
        );
        assert_eq!(
            fold_messages(["Co-authored-by: Alice <a@example.invalid>"]),
            "Co-authored-by: Alice <a@example.invalid>"
        );
    }

    #[test]
    fn keeps_blank_lines_inside_a_part_but_trims_its_edges() {
        assert_eq!(
            fold_messages(["\n\nTitle  \n\nBody line\n\n"]),
            "Title\n\nBody line"
        );
    }

    #[test]
    fn nothing_folds_to_an_empty_message() {
        assert_eq!(fold_messages(["", "\n"]), "");
        assert_eq!(fold_messages(std::iter::empty()), "");
    }

    #[test]
    fn strips_only_a_leading_title_line() {
        assert_eq!(strip_title_line("Title\n\nBody", "Title"), "Body");
        assert_eq!(strip_title_line("Title", "Title"), "");
        assert_eq!(
            strip_title_line("Other\n\nTitle", "Title"),
            "Other\n\nTitle"
        );
    }

    #[test]
    fn parses_only_well_formed_coauthor_trailers() {
        assert_eq!(
            parse_coauthor_trailer("Co-Authored-By:  Alice Smith  <a@example.invalid> "),
            Some(("Alice Smith".into(), "a@example.invalid".into()))
        );
        assert_eq!(parse_coauthor_trailer("Signed-off-by: A <a@x>"), None);
        assert_eq!(parse_coauthor_trailer("Co-authored-by: <a@x>"), None);
        assert_eq!(parse_coauthor_trailer("Co-authored-by: A a@x"), None);
    }
}
