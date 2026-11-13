//! The standalone server's TOML config file (`wheat-server --config <file>`).
//!
//! It is operator-written configuration, not a Wheat storage record, so it is
//! TOML rather than BinSchema. It holds the `[auth]` table, without which the
//! server runs open and loopback-only, and the `[limits]` table.

use std::num::{NonZeroU64, NonZeroUsize};

use serde::Deserialize;

use crate::access::AuthConfig;

/// Largest config file the server will read.
#[cfg(feature = "config-file")]
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// The whole config file.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// Authentication and per-repository access. Absent means open, loopback-only.
    pub auth: Option<AuthConfig>,
    /// Limits on connections.
    #[serde(default)]
    pub limits: LimitsConfig,
}

/// Limits on connections. One left out keeps its default (see
/// [`crate::server::ServerConfig`]); zero is refused, since it would turn every
/// client away.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    /// Seconds a connection has from being accepted until its caller is
    /// authenticated.
    pub session_setup_seconds: Option<NonZeroU64>,
    /// Most connections open at once.
    pub max_connections: Option<NonZeroUsize>,
}

/// Reading the file needs the `config-file` feature; an embedder without it builds
/// a [`FileConfig`] from its own configuration.
#[cfg(feature = "config-file")]
impl FileConfig {
    /// Parse config file text.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError(e.to_string()))
    }

    /// Read and parse a config file.
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        let length = std::fs::metadata(path)
            .map_err(|e| ConfigError(format!("read {}: {e}", path.display())))?
            .len();
        if length > MAX_CONFIG_BYTES {
            return Err(ConfigError(format!(
                "{} is larger than {MAX_CONFIG_BYTES} bytes",
                path.display()
            )));
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError(format!("read {}: {e}", path.display())))?;
        Self::parse(&text).map_err(|e| ConfigError(format!("{}: {}", path.display(), e.0)))
    }
}

/// The config file could not be read or is invalid.
#[derive(Debug, Eq, PartialEq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::Access;

    #[test]
    fn empty_file_has_no_auth() {
        assert_eq!(FileConfig::parse("").unwrap(), FileConfig::default());
    }

    #[test]
    fn parses_limits_and_refuses_zero() {
        let config =
            FileConfig::parse("[limits]\nsession_setup_seconds = 3\nmax_connections = 64\n")
                .unwrap();
        assert_eq!(config.limits.session_setup_seconds, NonZeroU64::new(3));
        assert_eq!(config.limits.max_connections, NonZeroUsize::new(64));
        assert!(FileConfig::parse("[limits]\nsession_setup_seconds = 0\n").is_err());
        assert!(FileConfig::parse("[limits]\nmax_connections = 0\n").is_err());
        assert!(FileConfig::parse("[limits]\nmax_sessions = 1\n").is_err());
    }

    #[test]
    fn parses_principals_and_origins() {
        let hash = "a".repeat(64);
        let config = FileConfig::parse(&format!(
            r#"
            [auth]
            allowed_origins = ["https://example.com"]

            [[auth.principals]]
            name = "bart"
            token_hashes = ["{hash}"]

            [auth.principals.repositories]
            "*" = "read"
            "serialexp/wheat" = "force"
            "#
        ))
        .unwrap();
        let auth = config.auth.unwrap();
        assert_eq!(auth.allowed_origins, ["https://example.com"]);
        let bart = &auth.principals[0];
        assert_eq!(bart.name, "bart");
        assert_eq!(bart.token_hashes, [hash]);
        assert_eq!(bart.repositories["*"], Access::Read);
        assert_eq!(bart.repositories["serialexp/wheat"], Access::Force);
        assert!(!bart.admin, "admin is off unless the file says so");
    }

    #[test]
    fn parses_the_admin_flag() {
        let hash = "a".repeat(64);
        let config = FileConfig::parse(&format!(
            "[[auth.principals]]\nname = \"ops\"\ntoken_hashes = [\"{hash}\"]\nadmin = true\n"
        ))
        .unwrap();
        assert!(config.auth.unwrap().principals[0].admin);
    }

    #[test]
    fn unknown_keys_and_levels_are_errors() {
        assert!(FileConfig::parse("[auth]\ntokens = []\n").is_err());
        assert!(FileConfig::parse("listen = \"0.0.0.0:1\"\n").is_err());
        assert!(
            FileConfig::parse(
                "[[auth.principals]]\nname = \"a\"\n[auth.principals.repositories]\n\"*\" = \"admin\"\n"
            )
            .is_err()
        );
    }
}
