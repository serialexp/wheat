//! Who may connect, and what they may do with each repository.
//!
//! The server asks an [`AccessProvider`] twice: once per session to turn the
//! `ClientHello` bearer token into a [`Principal`], and before every repository
//! operation to learn that principal's [`Access`] to the repository. Embedders
//! (for example Gothab) plug in their own provider; the standalone server uses
//! [`OpenLoopbackAccess`] when it has no auth configuration and
//! [`TokenFileAccess`] when it does.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;

use async_trait::async_trait;
use serde::Deserialize;

/// Largest bearer token the server will look at. The wire field allows 64 KiB;
/// the protocol design caps tokens at 8 KiB.
pub const MAX_AUTH_TOKEN_BYTES: usize = 8 * 1024;

/// The repository name that matches every repository in a token file.
pub const ANY_REPOSITORY: &str = "*";

/// What a principal may do with one repository. Each level includes the ones
/// below it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// Open the repository, read its state, and fetch objects and content.
    Read,
    /// Publish a new head that descends from the current one.
    Publish,
    /// Publish a head that does not descend from the current one.
    Force,
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Access::Read => "read",
            Access::Publish => "publish",
            Access::Force => "force",
        })
    }
}

/// An authenticated identity. Only the provider that issued it interprets the name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Principal {
    pub name: String,
}

/// A provider could not answer (for example its database is down). This is not
/// a "no": the session is refused, and the error is logged.
#[derive(Debug)]
pub struct AccessError(pub String);

impl fmt::Display for AccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AccessError {}

/// Authentication and per-repository authorization for the native protocol.
#[async_trait]
pub trait AccessProvider: Send + Sync {
    /// Check a `ClientHello` bearer token. `Ok(None)` rejects the session.
    async fn authenticate(&self, token: &[u8]) -> Result<Option<Principal>, AccessError>;

    /// The highest access `principal` has to `repository`, or `None` for none.
    /// Asked before every operation, so revocation takes effect mid-session.
    async fn access(
        &self,
        principal: &Principal,
        repository: &str,
    ) -> Result<Option<Access>, AccessError>;

    /// Whether this provider lets anyone in. The standalone server refuses to
    /// listen on a non-loopback address with such a provider.
    fn is_open(&self) -> bool {
        false
    }
}

/// No auth configured: every connection gets full access. Only safe when the
/// server listens on a loopback address, which `server::run` enforces.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenLoopbackAccess;

#[async_trait]
impl AccessProvider for OpenLoopbackAccess {
    async fn authenticate(&self, _token: &[u8]) -> Result<Option<Principal>, AccessError> {
        Ok(Some(Principal {
            name: "anonymous".to_owned(),
        }))
    }

    async fn access(
        &self,
        _principal: &Principal,
        _repository: &str,
    ) -> Result<Option<Access>, AccessError> {
        Ok(Some(Access::Force))
    }

    fn is_open(&self) -> bool {
        true
    }
}

/// The `[auth]` table of the server's TOML config file.
///
/// ```toml
/// [auth]
/// allowed_origins = ["https://example.com"]
///
/// [[auth.principals]]
/// name = "bart"
/// token_hashes = ["<64 hex digits: BLAKE3 of the token>"]
///
/// [auth.principals.repositories]
/// "*" = "read"
/// "serialexp/wheat" = "force"
/// ```
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Browser `Origin` values allowed to open a session. Connections without
    /// an `Origin` header (non-browser clients) are not affected.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default)]
    pub principals: Vec<PrincipalConfig>,
}

/// One principal in the token file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PrincipalConfig {
    pub name: String,
    /// Hex BLAKE3 hashes of the principal's tokens. The file never holds a token.
    #[serde(default)]
    pub token_hashes: Vec<String>,
    /// Access per repository name. An exact name wins over `"*"`, even when it
    /// grants less.
    #[serde(default)]
    pub repositories: HashMap<String, Access>,
}

/// A token file is invalid.
#[derive(Debug, Eq, PartialEq)]
pub struct AuthConfigError(pub String);

impl fmt::Display for AuthConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AuthConfigError {}

/// Tokens and access rules from the server config file.
#[derive(Debug)]
pub struct TokenFileAccess {
    /// BLAKE3 of each token → index into `principals`.
    tokens: HashMap<[u8; 32], usize>,
    principals: Vec<PrincipalRules>,
    by_name: HashMap<String, usize>,
}

#[derive(Debug)]
struct PrincipalRules {
    name: String,
    repositories: HashMap<String, Access>,
}

impl PrincipalRules {
    fn access(&self, repository: &str) -> Option<Access> {
        self.repositories
            .get(repository)
            .or_else(|| self.repositories.get(ANY_REPOSITORY))
            .copied()
    }
}

impl TokenFileAccess {
    /// Build the provider, rejecting duplicate names, duplicate tokens and
    /// malformed hashes.
    pub fn new(config: &AuthConfig) -> Result<Self, AuthConfigError> {
        let mut tokens = HashMap::new();
        let mut principals = Vec::with_capacity(config.principals.len());
        let mut by_name = HashMap::with_capacity(config.principals.len());
        for (index, principal) in config.principals.iter().enumerate() {
            if principal.name.trim().is_empty() {
                return Err(AuthConfigError(format!(
                    "auth.principals[{index}] has an empty name"
                )));
            }
            if by_name.insert(principal.name.clone(), index).is_some() {
                return Err(AuthConfigError(format!(
                    "principal '{}' is listed more than once",
                    principal.name
                )));
            }
            for hash in &principal.token_hashes {
                let digest = blake3::Hash::from_hex(hash).map_err(|_| {
                    AuthConfigError(format!(
                        "principal '{}' has a token hash that is not 64 hex digits",
                        principal.name
                    ))
                })?;
                match tokens.entry(*digest.as_bytes()) {
                    Entry::Occupied(existing) => {
                        let owner: &PrincipalConfig = &config.principals[*existing.get()];
                        return Err(AuthConfigError(format!(
                            "the same token hash is listed for '{}' and '{}'",
                            owner.name, principal.name
                        )));
                    }
                    Entry::Vacant(slot) => {
                        slot.insert(index);
                    }
                }
            }
            principals.push(PrincipalRules {
                name: principal.name.clone(),
                repositories: principal.repositories.clone(),
            });
        }
        Ok(Self {
            tokens,
            principals,
            by_name,
        })
    }
}

/// The hex BLAKE3 hash of a token, as written in `token_hashes`.
pub fn token_hash_hex(token: &[u8]) -> String {
    blake3::hash(token).to_hex().to_string()
}

#[async_trait]
impl AccessProvider for TokenFileAccess {
    async fn authenticate(&self, token: &[u8]) -> Result<Option<Principal>, AccessError> {
        if token.is_empty() || token.len() > MAX_AUTH_TOKEN_BYTES {
            return Ok(None);
        }
        let digest = blake3::hash(token);
        Ok(self.tokens.get(digest.as_bytes()).map(|&index| Principal {
            name: self.principals[index].name.clone(),
        }))
    }

    async fn access(
        &self,
        principal: &Principal,
        repository: &str,
    ) -> Result<Option<Access>, AccessError> {
        Ok(self
            .by_name
            .get(&principal.name)
            .and_then(|&index| self.principals[index].access(repository)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(name: &str, tokens: &[&str], repositories: &[(&str, Access)]) -> PrincipalConfig {
        PrincipalConfig {
            name: name.to_owned(),
            token_hashes: tokens
                .iter()
                .map(|t| token_hash_hex(t.as_bytes()))
                .collect(),
            repositories: repositories
                .iter()
                .map(|(repo, access)| ((*repo).to_owned(), *access))
                .collect(),
        }
    }

    fn named(name: &str) -> Principal {
        Principal {
            name: name.to_owned(),
        }
    }

    #[tokio::test]
    async fn tokens_map_to_their_principal() {
        let access = TokenFileAccess::new(&AuthConfig {
            allowed_origins: vec![],
            principals: vec![
                principal("alice", &["alice-1", "alice-2"], &[]),
                principal("bob", &["bob-1"], &[]),
            ],
        })
        .unwrap();
        assert_eq!(
            access.authenticate(b"alice-2").await.unwrap(),
            Some(named("alice"))
        );
        assert_eq!(
            access.authenticate(b"bob-1").await.unwrap(),
            Some(named("bob"))
        );
        assert_eq!(access.authenticate(b"carol").await.unwrap(), None);
        assert_eq!(access.authenticate(b"").await.unwrap(), None);
    }

    #[tokio::test]
    async fn oversized_tokens_are_rejected_without_matching() {
        let big = vec![b'x'; MAX_AUTH_TOKEN_BYTES + 1];
        let access = TokenFileAccess::new(&AuthConfig {
            allowed_origins: vec![],
            principals: vec![PrincipalConfig {
                name: "big".to_owned(),
                token_hashes: vec![token_hash_hex(&big)],
                repositories: HashMap::new(),
            }],
        })
        .unwrap();
        assert_eq!(access.authenticate(&big).await.unwrap(), None);
    }

    #[tokio::test]
    async fn exact_repository_rules_win_over_the_wildcard() {
        let access = TokenFileAccess::new(&AuthConfig {
            allowed_origins: vec![],
            principals: vec![principal(
                "alice",
                &["t"],
                &[
                    (ANY_REPOSITORY, Access::Force),
                    ("team/locked", Access::Read),
                ],
            )],
        })
        .unwrap();
        let alice = named("alice");
        assert_eq!(
            access.access(&alice, "team/other").await.unwrap(),
            Some(Access::Force)
        );
        assert_eq!(
            access.access(&alice, "team/locked").await.unwrap(),
            Some(Access::Read)
        );
    }

    #[tokio::test]
    async fn no_rule_means_no_access() {
        let access = TokenFileAccess::new(&AuthConfig {
            allowed_origins: vec![],
            principals: vec![principal("alice", &["t"], &[("team/a", Access::Publish)])],
        })
        .unwrap();
        assert_eq!(
            access.access(&named("alice"), "team/b").await.unwrap(),
            None
        );
        assert_eq!(
            access.access(&named("mallory"), "team/a").await.unwrap(),
            None
        );
    }

    #[test]
    fn access_levels_are_ordered() {
        assert!(Access::Read < Access::Publish);
        assert!(Access::Publish < Access::Force);
    }

    #[test]
    fn duplicate_names_and_tokens_are_rejected() {
        let error = TokenFileAccess::new(&AuthConfig {
            allowed_origins: vec![],
            principals: vec![principal("a", &["x"], &[]), principal("a", &["y"], &[])],
        })
        .unwrap_err();
        assert!(error.0.contains("more than once"), "{error}");

        let error = TokenFileAccess::new(&AuthConfig {
            allowed_origins: vec![],
            principals: vec![principal("a", &["x"], &[]), principal("b", &["x"], &[])],
        })
        .unwrap_err();
        assert!(error.0.contains("same token hash"), "{error}");
    }

    #[test]
    fn malformed_hashes_and_empty_names_are_rejected() {
        let error = TokenFileAccess::new(&AuthConfig {
            allowed_origins: vec![],
            principals: vec![PrincipalConfig {
                name: "a".to_owned(),
                token_hashes: vec!["not-hex".to_owned()],
                repositories: HashMap::new(),
            }],
        })
        .unwrap_err();
        assert!(error.0.contains("64 hex digits"), "{error}");

        let error = TokenFileAccess::new(&AuthConfig {
            allowed_origins: vec![],
            principals: vec![principal(" ", &[], &[])],
        })
        .unwrap_err();
        assert!(error.0.contains("empty name"), "{error}");
    }

    #[tokio::test]
    async fn open_access_lets_anyone_do_anything() {
        let open = OpenLoopbackAccess;
        assert!(open.is_open());
        let who = open.authenticate(b"").await.unwrap().unwrap();
        assert_eq!(
            open.access(&who, "any/repo").await.unwrap(),
            Some(Access::Force)
        );
    }
}
