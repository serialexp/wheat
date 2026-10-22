//! Native authoring identity, metadata, and typed configuration.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, FixedOffset, Local, SecondsFormat};
use serde::{Deserialize, Serialize};
use wheat_core::{Actor, ChangeMetadata, RevisionMetadata, Timestamp};

const CONFIG_FORMAT: &str = "wheat-config";
const CONFIG_VERSION: u32 = 2;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_ACTOR_NAME_BYTES: usize = 1_024;
const MAX_ACTOR_EMAIL_BYTES: usize = 3_200;
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_DESCRIPTION_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct Config {
    format: String,
    version: u32,
    #[serde(default)]
    actor: ActorConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    core: Option<CoreConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential: Option<CredentialConfig>,
}

/// How to get a token for a native remote.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
struct CredentialConfig {
    /// Shell command that prints a token; see `credential.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    helper: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
struct ActorConfig {
    name: Option<String>,
    email: Option<String>,
}

/// Extended configuration keys introduced in version 2.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CoreConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pager: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_format: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            format: CONFIG_FORMAT.to_owned(),
            version: CONFIG_VERSION,
            actor: ActorConfig::default(),
            core: None,
            credential: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigKey {
    ActorName,
    ActorEmail,
    CoreEditor,
    CorePager,
    CoreDefaultLabel,
    CoreDefaultFormat,
    CredentialHelper,
}

impl ConfigKey {
    /// The key as the user writes it.
    pub fn name(self) -> &'static str {
        match self {
            Self::ActorName => "actor.name",
            Self::ActorEmail => "actor.email",
            Self::CoreEditor => "core.editor",
            Self::CorePager => "core.pager",
            Self::CoreDefaultLabel => "core.default_label",
            Self::CoreDefaultFormat => "core.default_format",
            Self::CredentialHelper => "credential.helper",
        }
    }
}

impl std::str::FromStr for ConfigKey {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "actor.name" => Ok(Self::ActorName),
            "actor.email" => Ok(Self::ActorEmail),
            "core.editor" => Ok(Self::CoreEditor),
            "core.pager" => Ok(Self::CorePager),
            "core.default_label" => Ok(Self::CoreDefaultLabel),
            "core.default_format" => Ok(Self::CoreDefaultFormat),
            "credential.helper" => Ok(Self::CredentialHelper),
            _ => Err(format!(
                "unknown configuration key '{value}'; valid keys: \
                 actor.name, actor.email, core.editor, core.pager, \
                 core.default_label, core.default_format, credential.helper"
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub struct MetadataArgs {
    pub message: String,
    pub description: Option<String>,
    pub created_at: Option<String>,
    pub authored_at: Option<String>,
    pub committed_at: Option<String>,
}

pub fn set(store_root: Option<&Path>, global: bool, key: ConfigKey, value: &str) -> Result<()> {
    let path = if global {
        user_config_path()?
    } else {
        store_root
            .context("repository configuration requires a Wheat workspace or --store")?
            .join("config.json")
    };
    validate_value(key, value)?;
    let parent = path.parent().context("Wheat config path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create Wheat config directory {}", parent.display()))?;
    let lock_path = path.with_extension("lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    lock.lock()?;
    let mut config = read_optional(&path)?.unwrap_or_default();
    match key {
        ConfigKey::ActorName => config.actor.name = Some(value.to_owned()),
        ConfigKey::ActorEmail => config.actor.email = Some(value.to_owned()),
        ConfigKey::CoreEditor
        | ConfigKey::CorePager
        | ConfigKey::CoreDefaultLabel
        | ConfigKey::CoreDefaultFormat => {
            let core = config.core.get_or_insert_with(CoreConfig::default);
            match key {
                ConfigKey::CoreEditor => core.editor = Some(value.to_owned()),
                ConfigKey::CorePager => core.pager = Some(value.to_owned()),
                ConfigKey::CoreDefaultLabel => core.default_label = Some(value.to_owned()),
                ConfigKey::CoreDefaultFormat => core.default_format = Some(value.to_owned()),
                _ => unreachable!(),
            }
        }
        ConfigKey::CredentialHelper => {
            config
                .credential
                .get_or_insert_with(CredentialConfig::default)
                .helper = Some(value.to_owned());
        }
    }
    let result = write_config(&path, &config);
    let unlock = lock.unlock();
    result.and_then(|()| unlock.map_err(Into::into))
}

/// The configured `credential.helper`: the repository's if it sets one, else
/// the user's. `store_root` is `None` before a store exists (clone).
pub fn credential_helper(store_root: Option<&Path>) -> Result<Option<String>> {
    let repository = match store_root {
        Some(root) => read_optional(&root.join("config.json"))?,
        None => None,
    };
    let from = |config: Option<Config>| config.and_then(|c| c.credential).and_then(|c| c.helper);
    match from(repository) {
        Some(helper) => Ok(Some(helper)),
        None => Ok(from(read_optional(&user_config_path()?)?)),
    }
}

pub fn actor(store_root: &Path) -> Result<Actor> {
    let user = read_optional(&user_config_path()?)?.unwrap_or_default();
    let repository = read_optional(&store_root.join("config.json"))?.unwrap_or_default();
    let name =
        repository.actor.name.or(user.actor.name).context(
            "Wheat actor name is not configured; run `wheat config set actor.name <name>`",
        )?;
    let email = repository.actor.email.or(user.actor.email).context(
        "Wheat actor email is not configured; run `wheat config set actor.email <email>`",
    )?;
    validate_actor(&name, &email)?;
    Ok(Actor { name, email })
}

pub fn metadata(
    store_root: &Path,
    arguments: MetadataArgs,
    include_change: bool,
) -> Result<(Option<ChangeMetadata>, RevisionMetadata)> {
    validate_revision_message(&arguments.message, include_change)?;
    let actor = actor(store_root)?;
    let now = Local::now().fixed_offset();
    let authored_at = parse_timestamp(arguments.authored_at.as_deref(), now, "authored-at")?;
    let committed_at = parse_timestamp(arguments.committed_at.as_deref(), now, "committed-at")?;
    let revision = RevisionMetadata {
        message: arguments.message.clone(),
        author: actor.clone(),
        authored_at,
        committer: actor.clone(),
        committed_at,
        provenance: None,
    };
    let change = if include_change {
        let description = arguments.description.unwrap_or_default();
        ensure_bounded("change description", &description, MAX_DESCRIPTION_BYTES)?;
        Some(ChangeMetadata {
            title: message_title(&arguments.message)?.to_owned(),
            description,
            creator: actor,
            created_at: parse_timestamp(arguments.created_at.as_deref(), now, "created-at")?,
            provenance: None,
        })
    } else {
        ensure!(
            arguments.description.is_none() && arguments.created_at.is_none(),
            "--description and --created-at are valid only when creating a logical change"
        );
        None
    };
    Ok((change, revision))
}

/// The current local time, as revision metadata records it.
pub fn now() -> Result<Timestamp> {
    parse_timestamp(None, Local::now().fixed_offset(), "the local time")
}

fn message_title(message: &str) -> Result<&str> {
    let title = message.lines().next().unwrap_or_default().trim();
    ensure!(
        !title.is_empty(),
        "revision message must have a nonempty first line"
    );
    Ok(title)
}

/// Validates revision-message bounds using the canonical CLI limit.
///
/// Squash callers retain an existing message, so they pass `false` to permit an
/// empty message that was valid for a non-change revision.
pub fn validate_revision_message(message: &str, require_text: bool) -> Result<()> {
    if require_text {
        ensure_text("revision message", message, MAX_MESSAGE_BYTES)
    } else {
        ensure_bounded("revision message", message, MAX_MESSAGE_BYTES)
    }
}

fn parse_timestamp(
    value: Option<&str>,
    default: DateTime<FixedOffset>,
    field: &str,
) -> Result<Timestamp> {
    let value = match value {
        Some(value) => DateTime::parse_from_rfc3339(value)
            .with_context(|| format!("{field} must be an RFC3339 timestamp"))?,
        None => default,
    };
    let offset_seconds = value.offset().local_minus_utc();
    ensure!(
        offset_seconds % 60 == 0,
        "{field} offset must be an exact number of minutes"
    );
    let offset_minutes = i16::try_from(offset_seconds / 60)
        .with_context(|| format!("{field} offset is outside the supported range"))?;
    Ok(Timestamp {
        seconds: value.timestamp(),
        offset_minutes,
    })
}

fn validate_actor(name: &str, email: &str) -> Result<()> {
    validate_value(ConfigKey::ActorName, name)?;
    validate_value(ConfigKey::ActorEmail, email)
}

const MAX_CORE_VALUE_BYTES: usize = 4_096;

fn validate_value(key: ConfigKey, value: &str) -> Result<()> {
    let (field, limit) = match key {
        ConfigKey::ActorName => ("actor.name", MAX_ACTOR_NAME_BYTES),
        ConfigKey::ActorEmail => ("actor.email", MAX_ACTOR_EMAIL_BYTES),
        ConfigKey::CoreEditor => ("core.editor", MAX_CORE_VALUE_BYTES),
        ConfigKey::CorePager => ("core.pager", MAX_CORE_VALUE_BYTES),
        ConfigKey::CoreDefaultLabel => ("core.default_label", MAX_CORE_VALUE_BYTES),
        ConfigKey::CoreDefaultFormat => ("core.default_format", MAX_CORE_VALUE_BYTES),
        ConfigKey::CredentialHelper => ("credential.helper", MAX_CORE_VALUE_BYTES),
    };
    ensure_text(field, value, limit)?;
    ensure!(
        !value.chars().any(char::is_control),
        "{field} must not contain control characters"
    );
    if key == ConfigKey::ActorEmail {
        ensure!(
            value.contains('@') && !value.starts_with('@') && !value.ends_with('@'),
            "actor.email must contain a nonempty local part and domain"
        );
    }
    if key == ConfigKey::CoreDefaultFormat {
        ensure!(
            value == "human" || value == "json",
            "core.default_format must be 'human' or 'json'"
        );
    }
    Ok(())
}

fn ensure_text(field: &str, value: &str, limit: usize) -> Result<()> {
    ensure!(!value.trim().is_empty(), "{field} must not be empty");
    ensure_bounded(field, value, limit)
}

fn ensure_bounded(field: &str, value: &str, limit: usize) -> Result<()> {
    ensure!(
        value.len() <= limit,
        "{field} exceeds the {limit}-byte limit"
    );
    Ok(())
}

fn user_config_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("wheat/config.json"));
    }
    Ok(PathBuf::from(
        std::env::var_os("HOME").context("cannot locate Wheat user config: set XDG_CONFIG_HOME")?,
    )
    .join(".config/wheat/config.json"))
}

fn read_optional(path: &Path) -> Result<Option<Config>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    ensure!(
        metadata.is_file(),
        "Wheat config is not a regular file at {}",
        path.display()
    );
    ensure!(
        !metadata.file_type().is_symlink(),
        "Wheat config must not be a symlink at {}",
        path.display()
    );
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let length = file.metadata()?.len();
    ensure!(
        length <= MAX_CONFIG_BYTES,
        "Wheat config exceeds the {MAX_CONFIG_BYTES}-byte limit at {}",
        path.display()
    );
    let capacity = usize::try_from(length).context("Wheat config length exceeds address space")?;
    let mut bytes = Vec::with_capacity(capacity);
    use std::io::Read as _;
    file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_CONFIG_BYTES,
        "Wheat config grew beyond the {MAX_CONFIG_BYTES}-byte limit at {}",
        path.display()
    );
    let config: Config =
        serde_json::from_slice(&bytes).with_context(|| format!("decode {}", path.display()))?;
    ensure!(
        config.format == CONFIG_FORMAT && config.version >= 1 && config.version <= CONFIG_VERSION,
        "unsupported Wheat config at {} (format={:?}, version={})",
        path.display(),
        config.format,
        config.version
    );
    if let Some(name) = &config.actor.name {
        validate_value(ConfigKey::ActorName, name)?;
    }
    if let Some(email) = &config.actor.email {
        validate_value(ConfigKey::ActorEmail, email)?;
    }
    Ok(Some(config))
}

fn write_config(path: &Path, config: &Config) -> Result<()> {
    let parent = path.parent().context("Wheat config path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create Wheat config directory {}", parent.display()))?;
    let mut bytes = serde_json::to_vec_pretty(config)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() as u64 <= MAX_CONFIG_BYTES,
        "Wheat config is too large"
    );
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("publish Wheat config {}", path.display()))?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Builds a label annotation: the configured actor tags it, at `tagged_at`
/// (RFC3339) or now.
pub fn annotation(
    store_root: &Path,
    message: String,
    tagged_at: Option<&str>,
) -> Result<wheat_core::LabelAnnotation> {
    let annotation = wheat_core::LabelAnnotation {
        tagger: actor(store_root)?,
        tagged_at: parse_timestamp(tagged_at, Local::now().fixed_offset(), "tagged-at")?,
        message,
    };
    annotation
        .validate()
        .map_err(|error| crate::outcome::CliFailure::invalid(error.to_string()))?;
    Ok(annotation)
}

pub fn display_timestamp(value: Timestamp) -> Result<String> {
    let offset_seconds = i32::from(value.offset_minutes) * 60;
    let offset =
        FixedOffset::east_opt(offset_seconds).context("invalid stored timestamp offset")?;
    let datetime = DateTime::from_timestamp(value.seconds, 0)
        .context("invalid stored timestamp")?
        .with_timezone(&offset);
    Ok(datetime.to_rfc3339_opts(SecondsFormat::Secs, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_first_line_is_the_title() {
        assert_eq!(
            message_title("  concise title  \nbody").unwrap(),
            "concise title"
        );
        assert!(message_title("\nbody").is_err());
    }

    #[test]
    fn parses_and_formats_rfc3339_offsets_exactly() {
        let parsed = parse_timestamp(
            Some("2026-09-09T12:34:56+05:30"),
            Local::now().fixed_offset(),
            "test",
        )
        .unwrap();
        assert_eq!(parsed.offset_minutes, 330);
        assert_eq!(
            display_timestamp(parsed).unwrap(),
            "2026-09-09T12:34:56+05:30"
        );
    }
}
