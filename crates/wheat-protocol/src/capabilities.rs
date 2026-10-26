//! Capability negotiation: the optional modes one session uses.
//!
//! `ServerHello` offers the capabilities the server supports. The client answers in
//! `ClientHello` with the ones it will use, all from the offer, and from then on both
//! sides hold the same set. A new optional message, field or behaviour is switched on
//! by a capability, so peers that predate it keep working without a protocol version
//! bump.
//!
//! A capability name is 1 to [`MAX_CAPABILITY_BYTES`] bytes of lowercase ASCII
//! letters, digits, `-` and `.`, and a list holds at most [`MAX_CAPABILITIES`]
//! distinct names.

use std::collections::BTreeSet;

/// The capabilities this build supports: what its server offers and what its client
/// uses when offered. None yet.
pub const SUPPORTED_CAPABILITIES: &[&str] = &[];

/// Most capabilities one hello carries.
pub const MAX_CAPABILITIES: usize = 64;

/// Longest capability name, in bytes.
pub const MAX_CAPABILITY_BYTES: usize = 64;

/// The capabilities a session uses.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Capabilities(BTreeSet<String>);

impl Capabilities {
    pub fn contains(&self, name: &str) -> bool {
        self.0.contains(name)
    }

    /// The names, for `ClientHello`.
    pub fn to_vec(&self) -> Vec<String> {
        self.0.iter().cloned().collect()
    }
}

/// The capabilities a client uses from the server's `offered`: those in `supported`.
/// Refuses an offer that breaks the list rules.
pub fn select_capabilities(
    offered: &[String],
    supported: &[&str],
) -> Result<Capabilities, CapabilityError> {
    let offered = checked(offered)?;
    Ok(Capabilities(
        offered
            .into_iter()
            .filter(|name| supported.contains(&name.as_str()))
            .collect(),
    ))
}

/// The capabilities a session uses, from the client's `requested`. Refuses a list
/// that breaks the list rules or names a capability the server did not offer.
pub fn accept_capabilities(
    requested: &[String],
    offered: &[&str],
) -> Result<Capabilities, CapabilityError> {
    let requested = checked(requested)?;
    if let Some(name) = requested
        .iter()
        .find(|name| !offered.contains(&name.as_str()))
    {
        return Err(CapabilityError::NotOffered(name.clone()));
    }
    Ok(Capabilities(requested))
}

fn checked(names: &[String]) -> Result<BTreeSet<String>, CapabilityError> {
    if names.len() > MAX_CAPABILITIES {
        return Err(CapabilityError::TooMany(names.len()));
    }
    let mut set = BTreeSet::new();
    for name in names {
        let valid = !name.is_empty()
            && name.len() <= MAX_CAPABILITY_BYTES
            && name
                .bytes()
                .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.'));
        if !valid {
            return Err(CapabilityError::InvalidName(name.clone()));
        }
        if !set.insert(name.clone()) {
            return Err(CapabilityError::Duplicate(name.clone()));
        }
    }
    Ok(set)
}

/// A capability list that breaks the rules.
#[derive(Debug, Eq, PartialEq)]
pub enum CapabilityError {
    /// More than [`MAX_CAPABILITIES`] names.
    TooMany(usize),
    /// A name that is empty, too long, or has a character outside the set.
    InvalidName(String),
    /// A name listed twice.
    Duplicate(String),
    /// A name the server did not offer.
    NotOffered(String),
}

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapabilityError::TooMany(count) => write!(
                f,
                "{count} capabilities, more than the {MAX_CAPABILITIES} allowed"
            ),
            CapabilityError::InvalidName(name) => {
                write!(f, "capability name {name:?} is not allowed")
            }
            CapabilityError::Duplicate(name) => write!(f, "capability {name:?} is listed twice"),
            CapabilityError::NotOffered(name) => {
                write!(f, "capability {name:?} was not offered")
            }
        }
    }
}

impl std::error::Error for CapabilityError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_client_uses_the_offered_capabilities_it_supports() {
        let selected = select_capabilities(
            &names(&["resume", "lazy-content.v2"]),
            &["lazy-content.v2", "x"],
        )
        .unwrap();
        assert_eq!(selected.to_vec(), names(&["lazy-content.v2"]));
        assert!(selected.contains("lazy-content.v2"));
        assert!(!selected.contains("x"));
    }

    #[test]
    fn a_server_accepts_only_what_it_offered() {
        let accepted = accept_capabilities(&names(&["resume"]), &["resume", "other"]).unwrap();
        assert_eq!(accepted.to_vec(), names(&["resume"]));
        assert_eq!(
            accept_capabilities(&names(&["other"]), &["resume"]),
            Err(CapabilityError::NotOffered("other".to_owned()))
        );
    }

    #[test]
    fn lists_that_break_the_rules_are_refused() {
        let too_many: Vec<String> = (0..=MAX_CAPABILITIES).map(|i| format!("c{i}")).collect();
        assert_eq!(
            select_capabilities(&too_many, &[]),
            Err(CapabilityError::TooMany(MAX_CAPABILITIES + 1))
        );
        for name in [
            "",
            "Upper",
            "white space",
            &"x".repeat(MAX_CAPABILITY_BYTES + 1),
        ] {
            assert_eq!(
                accept_capabilities(&names(&[name]), &[name]),
                Err(CapabilityError::InvalidName(name.to_owned())),
                "{name:?}"
            );
        }
        assert_eq!(
            select_capabilities(&names(&["a", "a"]), &["a"]),
            Err(CapabilityError::Duplicate("a".to_owned()))
        );
        assert!(
            accept_capabilities(
                &names(&["x".repeat(MAX_CAPABILITY_BYTES).as_str()]),
                &["x".repeat(MAX_CAPABILITY_BYTES).as_str()]
            )
            .is_ok()
        );
    }
}
