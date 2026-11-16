//! Shared database snapshot, export, and import models.
//!
//! Snapshots contain database state only. They never include project files,
//! PHP settings, cache data, or mail. The daemon owns all filesystem and
//! engine work; the CLI, GUI, and MCP server exchange these values over the
//! normal protocol.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

use crate::{Database, LocalWebError, Result};

/// Version of the persisted snapshot metadata document.
pub const SNAPSHOT_SCHEMA: u32 = 1;
/// Largest accepted user-supplied snapshot name.
pub const SNAPSHOT_NAME_MAX: usize = 64;
/// Reserved selector name; it must never collide with a stored snapshot.
pub const SNAPSHOT_LATEST: &str = "latest";

/// Payload encoding used inside a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotFormat {
    /// Portable SQL produced by the engine's dump tool or the SQLite dumper.
    Sql,
    /// A consistent SQLite database image produced by the online backup API.
    Sqlite,
}

impl SnapshotFormat {
    /// Returns the stable on-disk label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sql => "sql",
            Self::Sqlite => "sqlite",
        }
    }
}

/// Compression applied to a snapshot or export payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompressionFormat {
    /// No compression.
    None,
    /// RFC 1952 gzip stream.
    Gzip,
    /// Zstandard stream.
    Zstd,
}

impl CompressionFormat {
    /// Returns the stable on-disk label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Gzip => "gzip",
            Self::Zstd => "zstd",
        }
    }

    /// Infers the compression from a destination path.
    ///
    /// Only the documented `.sql`, `.sql.gz`, and `.sql.zst` combinations are
    /// accepted. A non-SQL suffix is rejected instead of being guessed.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported extensions.
    pub fn from_path(path: &Path) -> Result<Self> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| LocalWebError::Config("export path has no valid file name".into()))?;
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".sql.zst") {
            Ok(Self::Zstd)
        } else if lower.ends_with(".sql.gz") {
            Ok(Self::Gzip)
        } else if Path::new(&lower)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("sql"))
        {
            Ok(Self::None)
        } else {
            Err(LocalWebError::Config(format!(
                "unsupported export extension for {name}; use .sql, .sql.gz, or .sql.zst"
            )))
        }
    }

    /// Returns the suffix expected by the compression, if any.
    #[must_use]
    pub const fn suffix(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Gzip => Some("gz"),
            Self::Zstd => Some("zst"),
        }
    }
}

/// Metadata persisted beside every snapshot payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseSnapshot {
    /// Metadata schema version.
    pub schema_version: u32,
    /// Stable identifier; never derived from the display name.
    pub id: String,
    /// User-visible unique name inside the project.
    pub name: String,
    /// Stable project identifier, unaffected by project renames.
    pub project_id: Uuid,
    /// Project name at creation time, kept for human context.
    pub project_name: String,
    /// Database engine at creation time.
    pub engine: Database,
    /// Engine version reported by the server.
    pub engine_version: String,
    /// Creation time in UTC.
    pub created_at: DateTime<Utc>,
    /// Payload size after compression.
    pub size_bytes: u64,
    /// Logical payload size before compression.
    pub uncompressed_bytes: u64,
    /// Payload encoding.
    pub format: SnapshotFormat,
    /// Payload compression.
    pub compression: CompressionFormat,
    /// SHA-256 of the stored payload, lowercase hexadecimal.
    pub checksum: String,
    /// Whether the snapshot was created automatically before a destructive
    /// operation. Safety snapshots are never selected by `--latest`.
    #[serde(default)]
    pub safety: bool,
    /// Optional operation note, for example `before-restore`.
    #[serde(default)]
    pub description: Option<String>,
}

impl DatabaseSnapshot {
    /// Returns whether this snapshot can be restored into `engine`.
    #[must_use]
    pub fn has_engine(&self, engine: Database) -> bool {
        self.engine == engine
    }
}

/// Selects a snapshot for restore or deletion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotSelector {
    /// Newest non-safety snapshot.
    Latest,
    /// Unique display name.
    Name(String),
    /// Stable snapshot identifier.
    Id(String),
}

impl SnapshotSelector {
    /// Parses a CLI selector, reserving `latest` for the newest non-safety
    /// snapshot and resolving everything else as a display name.
    #[must_use]
    pub fn from_cli(value: &str) -> Self {
        if value.eq_ignore_ascii_case(SNAPSHOT_LATEST) {
            Self::Latest
        } else {
            Self::Name(value.to_owned())
        }
    }
}

/// Result of a snapshot, export, import, restore, or deletion operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseOperationResult {
    /// Human-readable outcome.
    pub message: String,
    /// Newly created snapshot, when applicable.
    #[serde(default)]
    pub snapshot: Option<DatabaseSnapshot>,
    /// Safety snapshot created before a destructive operation.
    #[serde(default)]
    pub safety_snapshot: Option<DatabaseSnapshot>,
    /// Destination file for an export.
    #[serde(default)]
    pub path: Option<std::path::PathBuf>,
    /// Stored or exported size in bytes.
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// Logical size before compression.
    #[serde(default)]
    pub uncompressed_bytes: Option<u64>,
    /// Space freed by a delete or cleanup.
    #[serde(default)]
    pub reclaimed_bytes: Option<u64>,
    /// Number of snapshots removed by cleanup.
    #[serde(default)]
    pub removed: Option<usize>,
}

impl DatabaseOperationResult {
    /// Builds a message-only result.
    #[must_use]
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            snapshot: None,
            safety_snapshot: None,
            path: None,
            size_bytes: None,
            uncompressed_bytes: None,
            reclaimed_bytes: None,
            removed: None,
        }
    }
}

/// Validates a user-supplied snapshot name.
///
/// Names are display values, never paths. They are limited to ASCII letters,
/// digits, `.`, `_`, and `-`, must start with a letter or digit, and may not
/// end with a dot. `latest` is reserved for the selector.
///
/// # Errors
///
/// Returns an error for empty, oversized, path-like, or reserved names.
pub fn validate_snapshot_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > SNAPSHOT_NAME_MAX {
        return Err(LocalWebError::Config(format!(
            "snapshot names must contain between 1 and {SNAPSHOT_NAME_MAX} characters"
        )));
    }
    if name.eq_ignore_ascii_case(SNAPSHOT_LATEST) {
        return Err(LocalWebError::Config(
            "`latest` is reserved for selecting the newest snapshot".into(),
        ));
    }
    let bytes = name.as_bytes();
    if !bytes[0].is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        || name.ends_with('.')
        || name.contains("..")
    {
        return Err(LocalWebError::Config(format!(
            "invalid snapshot name `{name}`; use letters, digits, dots, underscores, or hyphens, starting with a letter or digit"
        )));
    }
    Ok(())
}

/// Returns the default timestamped snapshot name for `at`.
#[must_use]
pub fn default_snapshot_name(at: DateTime<Utc>) -> String {
    at.format("snapshot-%Y%m%d-%H%M%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_display_values_not_paths() {
        for valid in ["before-update", "v1.2", "Nightly_2026", "a"] {
            validate_snapshot_name(valid).unwrap();
        }
        for invalid in [
            "",
            "../escape",
            ".hidden",
            "name with spaces",
            "latest",
            "LATEST",
            "trailing.",
            "a/b",
            "a\\b",
            "a..b",
        ] {
            assert!(
                validate_snapshot_name(invalid).is_err(),
                "{invalid} should be rejected"
            );
        }
        assert_eq!(
            validate_snapshot_name(&"a".repeat(65)).unwrap_err().code(),
            "InvalidConfiguration"
        );
    }

    #[test]
    fn compression_is_strictly_inferred() {
        assert_eq!(
            CompressionFormat::from_path(Path::new("backup.sql")).unwrap(),
            CompressionFormat::None
        );
        assert_eq!(
            CompressionFormat::from_path(Path::new("backup.SQL.GZ")).unwrap(),
            CompressionFormat::Gzip
        );
        assert_eq!(
            CompressionFormat::from_path(Path::new("backup.sql.zst")).unwrap(),
            CompressionFormat::Zstd
        );
        CompressionFormat::from_path(Path::new("backup.sql.xz")).unwrap_err();
        CompressionFormat::from_path(Path::new("backup.zip")).unwrap_err();
    }

    #[test]
    fn cli_selector_reserves_latest() {
        assert_eq!(
            SnapshotSelector::from_cli("latest"),
            SnapshotSelector::Latest
        );
        assert_eq!(
            SnapshotSelector::from_cli("before-update"),
            SnapshotSelector::Name("before-update".into())
        );
    }
}
