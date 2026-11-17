//! Database-only snapshots, SQL export, and SQL import.
//!
//! Every operation is scoped to one project and runs under a per-project
//! asynchronous lock. Blocking engine work executes on Tokio's blocking pool,
//! so one long dump or restore never stalls the daemon or another project.
//!
//! Snapshots are stored under the `LocalWeb` data directory as
//! `<project-id>/<snapshot-id>/metadata.json` plus a compressed payload. The
//! directory is published with an atomic rename after the payload and its
//! checksum are complete, so interrupted work never appears in listings.

use crate::State;
use crate::find_project;
use crate::services::Services;
use localweb_core::{
    Database, LocalWebError, Paths, Project, Request, Response, Result, atomic_write,
    database::{
        CompressionFormat, DatabaseOperationResult, DatabaseSnapshot, SnapshotFormat,
        SnapshotSelector, default_snapshot_name, validate_snapshot_name,
    },
    runtime::RuntimeLayout,
};
use rusqlite::{Connection, OpenFlags, backup::Backup, ffi, types::ValueRef};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Maximum time one blocking engine operation may run before its child is
/// killed and the operation is reported as failed.
const OPERATION_TIMEOUT: Duration = Duration::from_hours(4);
/// Hard cap for a decompressed import or restore payload.
const MAX_UNCOMPRESSED_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// Maximum accepted expansion ratio for compressed imports. SQL text
/// normally expands by a few times; an archive that expands hundreds of
/// times is treated as a decompression bomb.
const MAX_EXPANSION_RATIO: u64 = 500;
/// Floor below which the expansion ratio is not enforced.
const EXPANSION_RATIO_FLOOR: u64 = 64 * 1024 * 1024;
/// Zstandard level used for snapshots and `.sql.zst` exports.
const ZSTD_LEVEL: i32 = 10;

/// Executes one database-management request.
///
/// # Errors
///
/// Returns validation, runtime, storage, and engine failures.
pub(crate) async fn execute(request: Request, state: &Arc<State>) -> Result<Response> {
    match request {
        Request::DatabaseSnapshotCreate { project, name } => {
            let result = with_project(state, &project, move |paths, project| {
                create_snapshot(paths, project, name, false, None)
            })
            .await?;
            Ok(Response::DatabaseOperation(Box::new(result)))
        }
        Request::DatabaseSnapshotList { project } => {
            let result = with_project(state, &project, |paths, project| {
                let store = SnapshotStore::new(paths, project)?;
                Ok(Response::Snapshots(store.list()?))
            })
            .await?;
            Ok(result)
        }
        Request::DatabaseSnapshotRestore {
            project,
            snapshot,
            safety_snapshot,
            confirm,
        } => {
            require_confirmation(
                confirm,
                "restoring a snapshot replaces the current database",
            )?;
            let result = with_project(state, &project, move |paths, project| {
                restore_snapshot(paths, project, &snapshot, safety_snapshot)
            })
            .await?;
            Ok(Response::DatabaseOperation(Box::new(result)))
        }
        Request::DatabaseSnapshotDelete {
            project,
            snapshot,
            confirm,
        } => {
            require_confirmation(confirm, "deleting a snapshot is irreversible")?;
            let result = with_project(state, &project, move |paths, project| {
                let store = SnapshotStore::new(paths, project)?;
                store.delete(&snapshot)
            })
            .await?;
            Ok(Response::DatabaseOperation(Box::new(result)))
        }
        Request::DatabaseSnapshotCleanup { project, confirm } => {
            require_confirmation(confirm, "deleting every snapshot is irreversible")?;
            let result = with_project(state, &project, |paths, project| {
                let store = SnapshotStore::new(paths, project)?;
                store.cleanup()
            })
            .await?;
            Ok(Response::DatabaseOperation(Box::new(result)))
        }
        Request::DatabaseExport {
            project,
            destination,
            compression,
            overwrite,
        } => {
            let result = with_project(state, &project, move |paths, project| {
                export_database(paths, project, &destination, compression, overwrite)
            })
            .await?;
            Ok(Response::DatabaseOperation(Box::new(result)))
        }
        Request::DatabaseImport {
            project,
            source,
            no_drop,
            safety_snapshot,
            confirm,
        } => {
            require_confirmation(
                confirm,
                "importing replaces the current database unless --no-drop is used",
            )?;
            let result = with_project(state, &project, move |paths, project| {
                import_database(paths, project, &source, no_drop, safety_snapshot)
            })
            .await?;
            Ok(Response::DatabaseOperation(Box::new(result)))
        }
        _ => Err(LocalWebError::Protocol(
            "not a database-management request".into(),
        )),
    }
}

/// Serializes database work for one project without taking the daemon-wide
/// project mutation lock.
pub(crate) async fn lock_project(state: &State, name: &str) -> Arc<Mutex<()>> {
    let mut locks = state.database_locks.lock().await;
    Arc::clone(
        locks
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

async fn with_project<T, F>(state: &Arc<State>, name: &str, operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Paths, &Project) -> Result<T> + Send + 'static,
{
    let paths = state.paths.clone();
    let queued = find_project(&paths, name)?;
    if queued.database == Database::None {
        return Err(LocalWebError::Config(format!(
            "{} has no database; database management is unavailable",
            queued.name
        )));
    }
    let lock = lock_project(state, name).await;
    let _guard = lock.lock().await;
    // A removal or an earlier database task may have changed the project
    // while this request waited. Re-read its definition under the project lock
    // so queued work cannot run against a deleted project.
    let project = find_project(&paths, name)?;
    if project.database == Database::None {
        return Err(LocalWebError::Config(format!(
            "{} has no database; database management is unavailable",
            project.name
        )));
    }
    // Database work reads and writes the same shared engines that project
    // start/stop and shutdown manage. Take the daemon phase lock after the
    // project lock, matching ProjectUpdate/ProjectRemove, so a long import or
    // restore can never race an engine stop.
    let _operations = state.begin_operation().await?;
    let project = find_project(&paths, name)?;
    tokio::task::spawn_blocking(move || operation(&paths, &project))
        .await
        .map_err(|error| {
            LocalWebError::Service(format!("database operation task failed: {error}"))
        })?
}

fn require_confirmation(confirm: bool, action: &str) -> Result<()> {
    if confirm {
        Ok(())
    } else {
        Err(LocalWebError::Config(format!(
            "{action}; pass confirm=true after the user acknowledges the destructive action"
        )))
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

struct SnapshotStore {
    root: PathBuf,
    project_id: Uuid,
}

impl SnapshotStore {
    fn new(paths: &Paths, project: &Project) -> Result<Self> {
        let root = paths
            .data
            .join("databases/snapshots")
            .join(project.id.simple().to_string());
        fs::create_dir_all(&root)?;
        restrict_directory(&root)?;
        Ok(Self {
            root,
            project_id: project.id,
        })
    }

    fn list(&self) -> Result<Vec<DatabaseSnapshot>> {
        let mut snapshots = Vec::new();
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Ok(snapshots);
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let metadata = entry.metadata()?;
            if !metadata.is_dir() {
                continue;
            }
            if fs::symlink_metadata(entry.path())?.file_type().is_symlink() {
                continue;
            }
            let file = entry.path().join("metadata.json");
            let Ok(bytes) = fs::read(&file) else {
                continue;
            };
            let Ok(snapshot) = serde_json::from_slice::<DatabaseSnapshot>(&bytes) else {
                continue;
            };
            if snapshot.project_id != self.project_id
                || snapshot.schema_version != localweb_core::database::SNAPSHOT_SCHEMA
                || snapshot.checksum.len() != 64
                || snapshot.id != name
            {
                continue;
            }
            let payload = entry.path().join(payload_name(snapshot.format));
            let Ok(payload_metadata) = fs::metadata(&payload) else {
                continue;
            };
            if !payload_metadata.is_file() || payload_metadata.len() != snapshot.size_bytes {
                continue;
            }
            snapshots.push(snapshot);
        }
        snapshots.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(snapshots)
    }

    fn find(&self, selector: &SnapshotSelector) -> Result<(DatabaseSnapshot, PathBuf)> {
        let snapshots = self.list()?;
        let selected = match selector {
            SnapshotSelector::Latest => snapshots.iter().find(|snapshot| !snapshot.safety),
            SnapshotSelector::Name(name) => snapshots
                .iter()
                .find(|snapshot| snapshot.name == *name || snapshot.id == *name),
            SnapshotSelector::Id(id) => snapshots.iter().find(|snapshot| snapshot.id == *id),
        };
        let snapshot = selected.cloned().ok_or_else(|| {
            let detail = match selector {
                SnapshotSelector::Latest => {
                    "no restorable snapshot exists; safety snapshots must be selected by name"
                        .to_owned()
                }
                SnapshotSelector::Name(name) => format!("snapshot `{name}` was not found"),
                SnapshotSelector::Id(id) => format!("snapshot id `{id}` was not found"),
            };
            LocalWebError::Config(detail)
        })?;
        let directory = self.root.join(&snapshot.id);
        ensure_contained(&self.root, &directory)?;
        Ok((snapshot, directory))
    }

    fn create<F>(
        &self,
        project: &Project,
        metadata: NewSnapshot,
        write: F,
    ) -> Result<DatabaseSnapshot>
    where
        F: FnOnce(&Path, CompressionFormat) -> Result<(u64, u64, String)>,
    {
        fs::create_dir_all(&self.root)?;
        restrict_directory(&self.root)?;
        validate_snapshot_name(&metadata.name)?;
        let existing = self.list()?;
        if existing
            .iter()
            .any(|snapshot| snapshot.name == metadata.name)
        {
            return Err(LocalWebError::Config(format!(
                "a snapshot named `{}` already exists",
                metadata.name
            )));
        }
        let id = loop {
            let candidate = format!(
                "{}-{}",
                metadata.created_at.format("%Y%m%d-%H%M%S"),
                &Uuid::new_v4().simple().to_string()[..8]
            );
            if !self.root.join(&candidate).exists() {
                break candidate;
            }
        };
        let temp = self.root.join(format!(".tmp-{}", Uuid::new_v4()));
        fs::create_dir_all(&temp)?;
        restrict_directory(&temp)?;
        let payload = temp.join(payload_name(metadata.format));
        let outcome = (|| {
            let (size_bytes, uncompressed_bytes, checksum) = write(&payload, metadata.compression)?;
            let snapshot = DatabaseSnapshot {
                schema_version: localweb_core::database::SNAPSHOT_SCHEMA,
                id: id.clone(),
                name: metadata.name,
                project_id: project.id,
                project_name: project.name.clone(),
                engine: project.database,
                engine_version: metadata.engine_version,
                created_at: metadata.created_at,
                size_bytes,
                uncompressed_bytes,
                format: metadata.format,
                compression: metadata.compression,
                checksum,
                safety: metadata.safety,
                description: metadata.description,
            };
            let encoded = serde_json::to_vec_pretty(&snapshot)
                .map_err(|error| LocalWebError::Config(error.to_string()))?;
            atomic_write(&temp.join("metadata.json"), &encoded, 0o600)?;
            sync_directory(&temp)?;
            let destination = self.root.join(&id);
            fs::rename(&temp, &destination)?;
            sync_directory(&self.root)?;
            Ok(snapshot)
        })();
        if outcome.is_err() {
            let _ = fs::remove_dir_all(&temp);
        }
        outcome
    }

    fn delete(&self, selector: &SnapshotSelector) -> Result<DatabaseOperationResult> {
        let (snapshot, directory) = self.find(selector)?;
        ensure_contained(&self.root, &directory)?;
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(LocalWebError::Config(
                "snapshot directory is not a regular directory".into(),
            ));
        }
        fs::remove_dir_all(&directory)?;
        Ok(DatabaseOperationResult {
            message: format!("deleted snapshot `{}`", snapshot.name),
            reclaimed_bytes: Some(snapshot.size_bytes),
            ..DatabaseOperationResult::message("")
        })
    }

    fn cleanup(&self) -> Result<DatabaseOperationResult> {
        let snapshots = self.list()?;
        let mut reclaimed = 0_u64;
        let mut removed = 0_usize;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                fs::remove_file(&path)?;
                continue;
            }
            if metadata.is_dir() {
                let name = entry.file_name();
                if name.to_str().is_some_and(|name| name.starts_with('.')) {
                    let _ = fs::remove_dir_all(&path);
                    continue;
                }
                if let Some(id) = name.to_str()
                    && let Some(snapshot) = snapshots.iter().find(|snapshot| snapshot.id == id)
                {
                    reclaimed = reclaimed.saturating_add(snapshot.size_bytes);
                    removed += 1;
                }
                fs::remove_dir_all(&path)?;
            }
        }
        Ok(DatabaseOperationResult {
            message: if removed == 0 {
                "no snapshots to remove".to_owned()
            } else {
                format!("deleted {removed} snapshot(s)")
            },
            reclaimed_bytes: Some(reclaimed),
            removed: Some(removed),
            ..DatabaseOperationResult::message("")
        })
    }
}

struct NewSnapshot {
    name: String,
    created_at: chrono::DateTime<chrono::Utc>,
    format: SnapshotFormat,
    compression: CompressionFormat,
    engine_version: String,
    safety: bool,
    description: Option<String>,
}

fn payload_name(format: SnapshotFormat) -> &'static str {
    match format {
        SnapshotFormat::Sql => "database.sql.zst",
        SnapshotFormat::Sqlite => "database.sqlite.zst",
    }
}

fn ensure_contained(root: &Path, path: &Path) -> Result<()> {
    let root = root.canonicalize()?;
    let path = path.canonicalize()?;
    if !path.starts_with(&root) {
        return Err(LocalWebError::Config(
            "refusing a snapshot path outside its project directory".into(),
        ));
    }
    Ok(())
}

#[cfg_attr(
    windows,
    expect(
        clippy::unnecessary_wraps,
        reason = "Windows inherits the private profile ACL; callers stay platform neutral"
    )
)]
fn restrict_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg_attr(
    windows,
    expect(
        clippy::unnecessary_wraps,
        reason = "Windows does not expose a directory fsync equivalent"
    )
)]
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    match File::open(path).and_then(|directory| directory.sync_all()) {
        Ok(()) => {}
        // Some Unix filesystems (and macOS directory descriptors) do not
        // implement directory fsync. The rename is still atomic; only the
        // extra durability barrier is unavailable there.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
            ) => {}
        Err(error) => return Err(error.into()),
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn unique_name(existing: &[DatabaseSnapshot], base: String) -> String {
    if !existing.iter().any(|snapshot| snapshot.name == base) {
        return base;
    }
    for suffix in 2..=9999 {
        let candidate = format!("{base}-{suffix}");
        if !existing.iter().any(|snapshot| snapshot.name == candidate) {
            return candidate;
        }
    }
    format!("{base}-{}", Uuid::new_v4().simple())
}

fn generated_name(store: &SnapshotStore, safety: bool) -> Result<String> {
    let existing = store.list()?;
    let timestamp = chrono::Utc::now();
    let base = if safety {
        format!(
            "safety-before-operation-{}",
            timestamp.format("%Y%m%d-%H%M%S")
        )
    } else {
        default_snapshot_name(timestamp)
    };
    let candidate = unique_name(&existing, base);
    validate_snapshot_name(&candidate)?;
    Ok(candidate)
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

fn create_snapshot(
    paths: &Paths,
    project: &Project,
    requested_name: Option<String>,
    safety: bool,
    description: Option<String>,
) -> Result<DatabaseOperationResult> {
    let engine = prepare_engine(paths, project)?;
    let store = SnapshotStore::new(paths, project)?;
    let name = match requested_name {
        Some(name) => {
            validate_snapshot_name(&name)?;
            let existing = store.list()?;
            if existing.iter().any(|snapshot| snapshot.name == name) {
                return Err(LocalWebError::Config(format!(
                    "a snapshot named `{name}` already exists"
                )));
            }
            name
        }
        None => generated_name(&store, safety)?,
    };
    let format = if project.database == Database::Sqlite {
        SnapshotFormat::Sqlite
    } else {
        SnapshotFormat::Sql
    };
    let created_at = chrono::Utc::now();
    let snapshot = store.create(
        project,
        NewSnapshot {
            name,
            created_at,
            format,
            compression: CompressionFormat::Zstd,
            engine_version: engine.version.clone(),
            safety,
            description,
        },
        |payload, compression| {
            write_snapshot_payload(paths, project, &engine, payload, compression)
        },
    )?;
    Ok(DatabaseOperationResult {
        message: format!(
            "created snapshot `{}` ({} bytes)",
            snapshot.name, snapshot.size_bytes
        ),
        snapshot: Some(snapshot),
        ..DatabaseOperationResult::message("")
    })
}

fn write_snapshot_payload(
    paths: &Paths,
    project: &Project,
    engine: &EngineContext,
    payload: &Path,
    compression: CompressionFormat,
) -> Result<(u64, u64, String)> {
    if engine.engine == Database::Sqlite {
        let directory = payload.parent().ok_or_else(|| {
            LocalWebError::Config("snapshot payload has no parent directory".into())
        })?;
        let backup = directory.join(format!(".sqlite-backup-{}", Uuid::new_v4()));
        let result = (|| {
            sqlite_backup(&engine.database_path, &backup)?;
            let file = create_private(payload)?;
            copy_file_compressed(&backup, file, compression)
        })();
        let _ = fs::remove_file(&backup);
        result
    } else {
        let file = create_private(payload)?;
        stream_dump(paths, project, engine, file, compression)
    }
}

fn restore_snapshot(
    paths: &Paths,
    project: &Project,
    selector: &SnapshotSelector,
    safety: bool,
) -> Result<DatabaseOperationResult> {
    let engine = prepare_engine(paths, project)?;
    let store = SnapshotStore::new(paths, project)?;
    let (snapshot, directory) = store.find(selector)?;
    if !snapshot.has_engine(project.database) {
        return Err(LocalWebError::Config(format!(
            "snapshot `{}` was created for {:?}, but {} uses {:?}",
            snapshot.name, snapshot.engine, project.name, project.database
        )));
    }
    ensure_version_compatible(&snapshot.engine_version, &engine.version, &snapshot.name)?;
    let payload = directory.join(payload_name(snapshot.format));
    verify_payload(&payload, &snapshot)?;
    let staging = staging_directory(paths)?;
    let sql_file = staging.join(format!("restore-{}.sql", Uuid::new_v4()));
    let outcome = (|| {
        if snapshot.format == SnapshotFormat::Sql {
            decompress_to_file(&payload, &sql_file, snapshot.compression)?;
            scan_sql(project.database, &sql_file, &engine.database)?;
        }
        let safety_snapshot = if safety {
            Some(
                create_snapshot(
                    paths,
                    project,
                    None,
                    true,
                    Some(format!("before restoring `{}`", snapshot.name)),
                )?
                .snapshot
                .ok_or_else(|| LocalWebError::Service("safety snapshot was not recorded".into()))?,
            )
        } else {
            None
        };
        let restore = match project.database {
            Database::Sqlite => {
                let backup = staging.join(format!("restore-{}.sqlite", Uuid::new_v4()));
                decompress_to_file(&payload, &backup, snapshot.compression)?;
                verify_sqlite_file(&backup)?;
                let result = sqlite_restore(&backup, &engine.database_path);
                let _ = fs::remove_file(&backup);
                result
            }
            Database::Mariadb | Database::Mysql => {
                drop_mysql_objects(paths, project, &engine)?;
                import_mysql_file(paths, project, &engine, &sql_file)
            }
            Database::Postgres => {
                drop_postgres_schema(paths, project, &engine)?;
                import_postgres_file(paths, project, &engine, &sql_file)
            }
            Database::None => unreachable!("a project without a database was rejected"),
        };
        match restore {
            Ok(()) => {
                verify_current_database(paths, project, &engine)?;
                Ok(DatabaseOperationResult {
                    message: format!(
                        "restored snapshot `{}` into {}",
                        snapshot.name, project.name
                    ),
                    safety_snapshot,
                    ..DatabaseOperationResult::message("")
                })
            }
            Err(error) => {
                let recovery = safety_snapshot
                    .as_ref()
                    .map_or_else(String::new, |snapshot| {
                        format!(
                            " Safety snapshot `{}` is available; restore it explicitly to recover.",
                            snapshot.name
                        )
                    });
                Err(LocalWebError::Service(format!(
                    "restore of `{}` failed: {error}.{recovery}",
                    snapshot.name
                )))
            }
        }
    })();
    let _ = fs::remove_dir_all(&staging);
    outcome
}

fn verify_payload(path: &Path, snapshot: &DatabaseSnapshot) -> Result<()> {
    let metadata = fs::metadata(path)?;
    if metadata.len() != snapshot.size_bytes {
        return Err(LocalWebError::Config(format!(
            "snapshot `{}` is incomplete: expected {} bytes, found {}",
            snapshot.name,
            snapshot.size_bytes,
            metadata.len()
        )));
    }
    let checksum = sha256_file(path)?;
    if checksum != snapshot.checksum {
        return Err(LocalWebError::Config(format!(
            "snapshot `{}` failed its SHA-256 checksum",
            snapshot.name
        )));
    }
    Ok(())
}

fn ensure_version_compatible(snapshot: &str, current: &str, name: &str) -> Result<()> {
    let snapshot_major = version_major(snapshot);
    let current_major = version_major(current);
    match (snapshot_major, current_major) {
        (Some(left), Some(right)) if left != right => Err(LocalWebError::Config(format!(
            "snapshot `{name}` was created with {snapshot}, but the current engine is {current}; major versions must match"
        ))),
        _ => Ok(()),
    }
}

fn version_major(version: &str) -> Option<&str> {
    version
        .split(|character: char| !character.is_ascii_digit() && character != '.')
        .find(|part| !part.is_empty())
        .and_then(|part| part.split('.').next())
        .filter(|part| !part.is_empty())
}

// ---------------------------------------------------------------------------
// Export and import
// ---------------------------------------------------------------------------

fn export_database(
    paths: &Paths,
    project: &Project,
    destination: &Path,
    compression: Option<CompressionFormat>,
    overwrite: bool,
) -> Result<DatabaseOperationResult> {
    if !destination.is_absolute() {
        return Err(LocalWebError::Config(
            "the export destination must be an absolute path".into(),
        ));
    }
    let parent = destination.parent().ok_or_else(|| {
        LocalWebError::Config("the export destination has no parent directory".into())
    })?;
    if !parent.is_dir() {
        return Err(LocalWebError::Config(format!(
            "export directory {} does not exist",
            parent.display()
        )));
    }
    if destination.exists() && !overwrite {
        return Err(LocalWebError::Config(format!(
            "{} already exists; choose another path or allow replacement",
            destination.display()
        )));
    }
    let compression = match compression {
        Some(compression) => {
            let inferred = CompressionFormat::from_path(destination)?;
            if inferred != compression {
                return Err(LocalWebError::Config(format!(
                    "export path {} does not match {} compression",
                    destination.display(),
                    compression.as_str()
                )));
            }
            compression
        }
        None => CompressionFormat::from_path(destination)?,
    };
    let engine = prepare_engine(paths, project)?;
    let temp = parent.join(format!(".localweb-export-{}.tmp", Uuid::new_v4()));
    let file = create_private(&temp)?;
    let outcome: Result<(u64, u64)> = (|| {
        let (size, uncompressed, _checksum) = if project.database == Database::Sqlite {
            stream_sqlite_dump(&engine.database_path, file, compression)?
        } else {
            stream_dump(paths, project, &engine, file, compression)?
        };
        publish_export(&temp, destination, overwrite)?;
        Ok((size, uncompressed))
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(&temp);
    }
    let (size_bytes, uncompressed_bytes) = outcome?;
    Ok(DatabaseOperationResult {
        message: format!(
            "exported {} to {} ({} bytes)",
            project.name,
            destination.display(),
            size_bytes
        ),
        path: Some(destination.to_path_buf()),
        size_bytes: Some(size_bytes),
        uncompressed_bytes: Some(uncompressed_bytes),
        ..DatabaseOperationResult::message("")
    })
}

fn import_database(
    paths: &Paths,
    project: &Project,
    source: &Path,
    no_drop: bool,
    safety: bool,
) -> Result<DatabaseOperationResult> {
    if !source.is_absolute() {
        return Err(LocalWebError::Config(
            "the import source must be an absolute path".into(),
        ));
    }
    let metadata = fs::metadata(source)?;
    if !metadata.is_file() {
        return Err(LocalWebError::Config(format!(
            "import source {} is not a regular file",
            source.display()
        )));
    }
    let compression = input_compression(source)?;
    let engine = prepare_engine(paths, project)?;
    let staging = staging_directory(paths)?;
    let sql_file = staging.join(format!("import-{}.sql", Uuid::new_v4()));
    let outcome = (|| {
        if compression == CompressionFormat::None {
            check_expansion(source, metadata.len(), metadata.len())?;
            scan_sql(project.database, source, &engine.database)?;
        } else {
            decompress_to_file(source, &sql_file, compression)?;
            check_expansion(source, metadata.len(), fs::metadata(&sql_file)?.len())?;
            scan_sql(project.database, &sql_file, &engine.database)?;
        }
        let input = if compression == CompressionFormat::None {
            source
        } else {
            &sql_file
        };
        let safety_snapshot = if safety {
            Some(
                create_snapshot(
                    paths,
                    project,
                    None,
                    true,
                    Some(format!("before import from {}", source.display())),
                )?
                .snapshot
                .ok_or_else(|| LocalWebError::Service("safety snapshot was not recorded".into()))?,
            )
        } else {
            None
        };
        let import = (|| {
            if !no_drop {
                match project.database {
                    Database::Mariadb | Database::Mysql => {
                        drop_mysql_objects(paths, project, &engine)?;
                    }
                    Database::Postgres => drop_postgres_schema(paths, project, &engine)?,
                    Database::Sqlite => drop_sqlite_objects(&engine.database_path)?,
                    Database::None => unreachable!("a project without a database was rejected"),
                }
            }
            match project.database {
                Database::Mariadb | Database::Mysql => {
                    import_mysql_file(paths, project, &engine, input)
                }
                Database::Postgres => import_postgres_file(paths, project, &engine, input),
                Database::Sqlite => import_sqlite_file(&engine.database_path, input),
                Database::None => unreachable!("a project without a database was rejected"),
            }
        })();
        match import {
            Ok(()) => {
                verify_current_database(paths, project, &engine)?;
                Ok(DatabaseOperationResult {
                    message: format!("imported {} into {}", source.display(), project.name),
                    safety_snapshot,
                    size_bytes: Some(metadata.len()),
                    ..DatabaseOperationResult::message("")
                })
            }
            Err(error) => {
                let recovery = safety_snapshot
                    .as_ref()
                    .map_or_else(String::new, |snapshot| {
                        format!(
                            " Safety snapshot `{}` is available; restore it explicitly to recover.",
                            snapshot.name
                        )
                    });
                Err(LocalWebError::Service(format!(
                    "import failed: {error}.{recovery}"
                )))
            }
        }
    })();
    let _ = fs::remove_dir_all(&staging);
    outcome
}

fn input_compression(path: &Path) -> Result<CompressionFormat> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| LocalWebError::Config("import path has no valid file name".into()))?
        .to_ascii_lowercase();
    if Path::new(&name)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("sql"))
    {
        Ok(CompressionFormat::None)
    } else if name.ends_with(".sql.gz") {
        Ok(CompressionFormat::Gzip)
    } else if name.ends_with(".sql.zst") {
        Ok(CompressionFormat::Zstd)
    } else {
        Err(LocalWebError::Config(format!(
            "unsupported import format for {name}; use .sql, .sql.gz, or .sql.zst"
        )))
    }
}

fn check_expansion(source: &Path, compressed: u64, expanded: u64) -> Result<()> {
    if expanded > MAX_UNCOMPRESSED_BYTES {
        return Err(LocalWebError::Config(format!(
            "refusing to process {}: decompressed content exceeds {} GiB",
            source.display(),
            MAX_UNCOMPRESSED_BYTES / 1024 / 1024 / 1024
        )));
    }
    if compressed > 0
        && expanded > EXPANSION_RATIO_FLOOR
        && expanded / compressed > MAX_EXPANSION_RATIO
    {
        return Err(LocalWebError::Config(format!(
            "refusing to process {}: suspicious compression ratio",
            source.display()
        )));
    }
    Ok(())
}

fn publish_export(temp: &Path, destination: &Path, overwrite: bool) -> Result<()> {
    if !overwrite {
        match fs::hard_link(temp, destination) {
            Ok(()) => {
                let _ = fs::remove_file(temp);
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(LocalWebError::Config(format!(
                    "{} already exists; choose another path",
                    destination.display()
                )));
            }
            Err(_) => {}
        }
        return fs::rename(temp, destination).map_err(|error| {
            if destination.exists() {
                LocalWebError::Config(format!(
                    "{} already exists; choose another path",
                    destination.display()
                ))
            } else {
                LocalWebError::from(error)
            }
        });
    }
    // Unix replaces atomically. Windows refuses to rename over a file, so the
    // old destination is removed only after that first attempt fails.
    match fs::rename(temp, destination) {
        Ok(()) => Ok(()),
        Err(_error) if destination.exists() => {
            fs::remove_file(destination)?;
            fs::rename(temp, destination).map_err(LocalWebError::from)
        }
        Err(error) => Err(error.into()),
    }
}

// ---------------------------------------------------------------------------
// Engine preparation and credentials
// ---------------------------------------------------------------------------

struct EngineContext {
    engine: Database,
    version: String,
    database: String,
    user: String,
    password: String,
    port: u16,
    database_path: PathBuf,
}

fn prepare_engine(paths: &Paths, project: &Project) -> Result<EngineContext> {
    let services = Services { paths };
    let mut context = match project.database {
        Database::Mariadb => {
            services.ensure_mariadb()?;
            services.ensure_project_database(project)?;
            let secrets = services.project_secrets(project)?;
            EngineContext {
                engine: Database::Mariadb,
                version: String::new(),
                database: secrets.database_name,
                user: secrets.database_user,
                password: secrets.database_password,
                port: 33_071,
                database_path: PathBuf::new(),
            }
        }
        Database::Mysql => {
            services.ensure_mysql()?;
            services.ensure_project_mysql_database(project)?;
            let secrets = services.project_secrets(project)?;
            EngineContext {
                engine: Database::Mysql,
                version: String::new(),
                database: secrets.database_name,
                user: secrets.database_user,
                password: secrets.database_password,
                port: 33_072,
                database_path: PathBuf::new(),
            }
        }
        Database::Postgres => {
            services.ensure_postgres()?;
            services.ensure_project_postgres_database(project)?;
            let secrets = services.project_secrets(project)?;
            EngineContext {
                engine: Database::Postgres,
                version: String::new(),
                database: secrets.database_name,
                user: secrets.database_user,
                password: secrets.database_password,
                port: 33_073,
                database_path: PathBuf::new(),
            }
        }
        Database::Sqlite => {
            let database_path = services.ensure_sqlite(project)?;
            EngineContext {
                engine: Database::Sqlite,
                version: String::new(),
                database: project.name.clone(),
                user: String::new(),
                password: String::new(),
                port: 0,
                database_path,
            }
        }
        Database::None => {
            return Err(LocalWebError::Config(format!(
                "{} has no database",
                project.name
            )));
        }
    };
    context.version = match context.engine {
        Database::Sqlite => sqlite_version(&context.database_path)?,
        Database::Postgres => postgres_version(paths, &context)?,
        Database::Mariadb | Database::Mysql => mysql_version(paths, &context)?,
        Database::None => unreachable!(),
    };
    Ok(context)
}

struct TempFile {
    path: PathBuf,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn operation_directory(paths: &Paths) -> Result<PathBuf> {
    let directory = paths.state.join("tmp/database");
    fs::create_dir_all(&directory)?;
    restrict_directory(&directory)?;
    Ok(directory)
}

fn staging_directory(paths: &Paths) -> Result<PathBuf> {
    let directory = operation_directory(paths)?;
    let staging = directory.join(format!("operation-{}", Uuid::new_v4()));
    fs::create_dir_all(&staging)?;
    restrict_directory(&staging)?;
    Ok(staging)
}

fn mysql_credentials(paths: &Paths, engine: &EngineContext) -> Result<TempFile> {
    let directory = operation_directory(paths)?;
    let path = directory.join(format!("mysql-{}.cnf", Uuid::new_v4()));
    let content = format!(
        "[client]\nuser={}\npassword={}\nhost=127.0.0.1\nport={}\nprotocol=TCP\ndefault-character-set=utf8mb4\n",
        engine.user, engine.password, engine.port
    );
    atomic_write(&path, content.as_bytes(), 0o600)?;
    Ok(TempFile { path })
}

fn postgres_credentials(paths: &Paths, engine: &EngineContext) -> Result<TempFile> {
    let directory = operation_directory(paths)?;
    let path = directory.join(format!("pgpass-{}", Uuid::new_v4()));
    let content = format!(
        "127.0.0.1:{}:{}:{}:{}\n",
        engine.port, engine.database, engine.user, engine.password
    );
    atomic_write(&path, content.as_bytes(), 0o600)?;
    Ok(TempFile { path })
}

fn runtime_environment(command: &mut Command) {
    let layout = RuntimeLayout::discover();
    let library_path = layout.library_path();
    if !library_path.is_empty() {
        command.env("LD_LIBRARY_PATH", library_path);
    }
}

fn mysql_client(engine: Database) -> Result<PathBuf> {
    let layout = RuntimeLayout::discover();
    if engine == Database::Mariadb {
        Ok(layout.mariadb()?.2)
    } else {
        Ok(layout.mysql()?.1)
    }
}

fn mysql_defaults_argument(path: &Path) -> OsString {
    OsString::from(format!("--defaults-extra-file={}", path.display()))
}

fn mysql_version(paths: &Paths, engine: &EngineContext) -> Result<String> {
    let client = mysql_client(engine.engine)?;
    let credentials = mysql_credentials(paths, engine)?;
    let mut command = Command::new(client);
    command
        .arg(mysql_defaults_argument(&credentials.path))
        .args([
            "--batch",
            "--skip-column-names",
            "--execute=SELECT VERSION()",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    runtime_environment(&mut command);
    let output = command.output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(LocalWebError::Service(format!(
            "cannot read the database version: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn postgres_version(paths: &Paths, engine: &EngineContext) -> Result<String> {
    let layout = RuntimeLayout::discover();
    let (_, _, psql) = layout.postgres()?;
    let credentials = postgres_credentials(paths, engine)?;
    let mut command = Command::new(psql);
    command
        .args([
            "--no-psqlrc",
            "--set=ON_ERROR_STOP=1",
            "--tuples-only",
            "--no-align",
            "--command=SHOW server_version",
        ])
        .env("PGPASSFILE", &credentials.path)
        .env("PGHOST", "127.0.0.1")
        .env("PGPORT", engine.port.to_string())
        .env("PGUSER", &engine.user)
        .env("PGDATABASE", &engine.database)
        .env("PGCONNECT_TIMEOUT", "10")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    runtime_environment(&mut command);
    let output = command.output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(LocalWebError::Service(format!(
            "cannot read the PostgreSQL version: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn sqlite_version(path: &Path) -> Result<String> {
    let connection = sq(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ))?;
    let version: String =
        sq(connection.query_row("SELECT sqlite_version()", [], |row| row.get(0)))?;
    Ok(version)
}

// ---------------------------------------------------------------------------
// Dump and import engines
// ---------------------------------------------------------------------------

fn stream_dump(
    paths: &Paths,
    project: &Project,
    engine: &EngineContext,
    file: File,
    compression: CompressionFormat,
) -> Result<(u64, u64, String)> {
    match engine.engine {
        Database::Mariadb | Database::Mysql => {
            let defaults = mysql_credentials(paths, engine)?;
            let tool = if engine.engine == Database::Mariadb {
                RuntimeLayout::discover().mariadb_dump()?
            } else {
                RuntimeLayout::discover().mysql_dump()?
            };
            let mut command = Command::new(tool);
            command.arg(mysql_defaults_argument(&defaults.path));
            command.args([
                "--single-transaction",
                "--quick",
                "--routines",
                "--events",
                "--triggers",
                "--hex-blob",
                "--no-tablespaces",
                "--default-character-set=utf8mb4",
                "--skip-add-locks",
            ]);
            if engine.engine == Database::Mysql {
                command.arg("--set-gtid-purged=OFF");
            }
            command
                .arg(&engine.database)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            runtime_environment(&mut command);
            stream_child_stdout(command, file, compression, "database dump")
        }
        Database::Postgres => {
            let layout = RuntimeLayout::discover();
            let (dump, _) = layout.postgres_dump_tools()?;
            let credentials = postgres_credentials(paths, engine)?;
            let mut command = Command::new(dump);
            command
                .args([
                    "--format=plain",
                    "--no-owner",
                    "--no-privileges",
                    "--clean",
                    "--if-exists",
                    "--encoding=UTF8",
                ])
                .arg(format!("--dbname={}", engine.database))
                .env("PGPASSFILE", &credentials.path)
                .env("PGHOST", "127.0.0.1")
                .env("PGPORT", engine.port.to_string())
                .env("PGUSER", &engine.user)
                .env("PGDATABASE", &engine.database)
                .env("PGCONNECT_TIMEOUT", "10")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            runtime_environment(&mut command);
            stream_child_stdout(command, file, compression, "PostgreSQL dump")
        }
        Database::Sqlite => {
            let _ = project;
            stream_sqlite_dump(&engine.database_path, file, compression)
        }
        Database::None => unreachable!("a project without a database was rejected"),
    }
}

fn stream_sqlite_dump(
    database: &Path,
    file: File,
    compression: CompressionFormat,
) -> Result<(u64, u64, String)> {
    let mut writer = PayloadWriter::new(HashingWriter::new(file), compression)?;
    let uncompressed = dump_sqlite(database, &mut writer)?;
    finish_payload(writer, uncompressed)
}

fn stream_child_stdout(
    mut command: Command,
    file: File,
    compression: CompressionFormat,
    label: &str,
) -> Result<(u64, u64, String)> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        LocalWebError::RuntimeNotInstalled(format!("cannot start the {label}: {error}"))
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| LocalWebError::Service(format!("{label} stdout is unavailable")))?;
    let stderr = child.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let mut reader = stderr;
            let _ = reader.read_to_end(&mut buffer);
            buffer
        })
    });
    let mut writer = PayloadWriter::new(HashingWriter::new(file), compression)?;
    let copied = match io::copy(&mut BufReader::new(stdout), &mut writer) {
        Ok(copied) => copied,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            if let Some(reader) = stderr {
                let _ = reader.join();
            }
            return Err(error.into());
        }
    };
    let wait = wait_for_child(&mut child, label);
    let errors = stderr
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    wait?;
    if !errors.is_empty() {
        tracing::debug!(
            "{} wrote diagnostics: {}",
            label,
            String::from_utf8_lossy(&errors).trim()
        );
    }
    finish_payload(writer, copied)
}

fn finish_payload<W: SyncWrite>(
    writer: PayloadWriter<HashingWriter<W>>,
    logical: u64,
) -> Result<(u64, u64, String)> {
    let hashing = writer.finish()?;
    let (file, size, checksum) = hashing.finish();
    file.sync()?;
    Ok((size, logical, checksum))
}

trait SyncWrite: Write {
    fn sync(&self) -> io::Result<()>;
}

impl SyncWrite for File {
    fn sync(&self) -> io::Result<()> {
        self.sync_all()
    }
}

impl SyncWrite for &mut File {
    fn sync(&self) -> io::Result<()> {
        self.sync_all()
    }
}

fn import_mysql_file(
    paths: &Paths,
    project: &Project,
    engine: &EngineContext,
    source: &Path,
) -> Result<()> {
    let defaults = mysql_credentials(paths, engine)?;
    let client = mysql_client(project.database)?;
    let mut command = Command::new(client);
    command
        .arg(mysql_defaults_argument(&defaults.path))
        .args([
            "--binary-mode",
            "--default-character-set=utf8mb4",
            "--database",
            &engine.database,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    runtime_environment(&mut command);
    run_child_with_input(command, source, "MySQL import")
}

fn import_postgres_file(
    paths: &Paths,
    project: &Project,
    engine: &EngineContext,
    source: &Path,
) -> Result<()> {
    let layout = RuntimeLayout::discover();
    let (_, _, psql) = layout.postgres()?;
    let credentials = postgres_credentials(paths, engine)?;
    let mut command = Command::new(psql);
    command
        .args([
            "--no-psqlrc",
            "--set=ON_ERROR_STOP=1",
            "--single-transaction",
        ])
        .env("PGPASSFILE", &credentials.path)
        .env("PGHOST", "127.0.0.1")
        .env("PGPORT", engine.port.to_string())
        .env("PGUSER", &engine.user)
        .env("PGDATABASE", &engine.database)
        .env("PGCONNECT_TIMEOUT", "10")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    runtime_environment(&mut command);
    let _ = project;
    run_child_with_input(command, source, "PostgreSQL import")
}

fn drop_mysql_objects(paths: &Paths, project: &Project, engine: &EngineContext) -> Result<()> {
    use std::fmt::Write as _;
    let defaults = mysql_credentials(paths, engine)?;
    let client = mysql_client(project.database)?;
    let mut command = Command::new(client);
    command
        .arg(mysql_defaults_argument(&defaults.path))
        .args([
            "--batch",
            "--skip-column-names",
            "--execute=SELECT table_name, table_type FROM information_schema.tables WHERE table_schema = DATABASE(); SELECT routine_type, routine_name FROM information_schema.routines WHERE routine_schema = DATABASE(); SELECT 'EVENT', event_name FROM information_schema.events WHERE event_schema = DATABASE();",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    runtime_environment(&mut command);
    let output = command.output()?;
    if !output.status.success() {
        return Err(LocalWebError::Service(format!(
            "cannot inspect MySQL objects: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut sql = String::from("SET FOREIGN_KEY_CHECKS=0;\n");
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split('\t');
        let first = fields.next().unwrap_or_default();
        let second = fields.next().unwrap_or_default();
        if second.is_empty() {
            continue;
        }
        if first.eq_ignore_ascii_case("VIEW") {
            let _ = writeln!(sql, "DROP VIEW IF EXISTS `{}`;", escape_mysql(second));
        } else if first.eq_ignore_ascii_case("BASE TABLE") {
            let _ = writeln!(sql, "DROP TABLE IF EXISTS `{}`;", escape_mysql(second));
        } else if first.eq_ignore_ascii_case("PROCEDURE") {
            let _ = writeln!(sql, "DROP PROCEDURE IF EXISTS `{}`;", escape_mysql(second));
        } else if first.eq_ignore_ascii_case("FUNCTION") {
            let _ = writeln!(sql, "DROP FUNCTION IF EXISTS `{}`;", escape_mysql(second));
        } else if first.eq_ignore_ascii_case("EVENT") {
            let _ = writeln!(sql, "DROP EVENT IF EXISTS `{}`;", escape_mysql(second));
        } else if first.eq_ignore_ascii_case("SEQUENCE") {
            let _ = writeln!(sql, "DROP SEQUENCE IF EXISTS `{}`;", escape_mysql(second));
        }
    }
    sql.push_str("SET FOREIGN_KEY_CHECKS=1;\n");
    run_mysql_sql(
        paths,
        project,
        engine,
        sql.as_bytes(),
        "MySQL object cleanup",
    )
}

fn drop_postgres_schema(paths: &Paths, project: &Project, engine: &EngineContext) -> Result<()> {
    let sql = "DO $localweb$ DECLARE schema_name text; BEGIN FOR schema_name IN SELECT nspname FROM pg_namespace WHERE nspname NOT LIKE 'pg_%' AND nspname <> 'information_schema' LOOP EXECUTE format('DROP SCHEMA %I CASCADE', schema_name); END LOOP; END $localweb$;\nCREATE SCHEMA public AUTHORIZATION current_user;\n";
    run_postgres_sql(
        paths,
        project,
        engine,
        sql.as_bytes(),
        "PostgreSQL schema cleanup",
    )
}

fn run_mysql_sql(
    paths: &Paths,
    project: &Project,
    engine: &EngineContext,
    sql: &[u8],
    label: &str,
) -> Result<()> {
    let defaults = mysql_credentials(paths, engine)?;
    let client = mysql_client(project.database)?;
    let mut command = Command::new(client);
    command
        .arg(mysql_defaults_argument(&defaults.path))
        .args(["--binary-mode", "--database", &engine.database])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    runtime_environment(&mut command);
    let mut child = command.spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| LocalWebError::Service(format!("{label} stdin is unavailable")))?;
    if let Err(error) = stdin.write_all(sql) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error.into());
    }
    drop(stdin);
    let stderr = child.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let mut reader = stderr;
            let _ = reader.read_to_end(&mut buffer);
            buffer
        })
    });
    wait_for_child(&mut child, label)?;
    if let Some(reader) = stderr
        && let Ok(errors) = reader.join()
        && !errors.is_empty()
    {
        tracing::debug!(
            "{} wrote diagnostics: {}",
            label,
            String::from_utf8_lossy(&errors).trim()
        );
    }
    Ok(())
}

fn run_postgres_sql(
    paths: &Paths,
    project: &Project,
    engine: &EngineContext,
    sql: &[u8],
    label: &str,
) -> Result<()> {
    let layout = RuntimeLayout::discover();
    let (_, _, psql) = layout.postgres()?;
    let credentials = postgres_credentials(paths, engine)?;
    let mut command = Command::new(psql);
    command
        .args([
            "--no-psqlrc",
            "--set=ON_ERROR_STOP=1",
            "--single-transaction",
        ])
        .env("PGPASSFILE", &credentials.path)
        .env("PGHOST", "127.0.0.1")
        .env("PGPORT", engine.port.to_string())
        .env("PGUSER", &engine.user)
        .env("PGDATABASE", &engine.database)
        .env("PGCONNECT_TIMEOUT", "10")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    runtime_environment(&mut command);
    let _ = project;
    let mut child = command.spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| LocalWebError::Service(format!("{label} stdin is unavailable")))?;
    if let Err(error) = stdin.write_all(sql) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error.into());
    }
    drop(stdin);
    let stderr = child.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let mut reader = stderr;
            let _ = reader.read_to_end(&mut buffer);
            buffer
        })
    });
    wait_for_child(&mut child, label)?;
    if let Some(reader) = stderr
        && let Ok(errors) = reader.join()
        && !errors.is_empty()
    {
        tracing::debug!(
            "{} wrote diagnostics: {}",
            label,
            String::from_utf8_lossy(&errors).trim()
        );
    }
    Ok(())
}

fn run_child_with_input(mut command: Command, source: &Path, label: &str) -> Result<()> {
    let mut child = command.spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| LocalWebError::Service(format!("{label} stdin is unavailable")))?;
    let source = source.to_path_buf();
    let writer = thread::spawn(move || -> io::Result<()> {
        let mut input = BufReader::new(File::open(&source)?);
        io::copy(&mut input, &mut stdin)?;
        stdin.flush()?;
        Ok(())
    });
    let stderr = child.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let mut reader = stderr;
            let _ = reader.read_to_end(&mut buffer);
            buffer
        })
    });
    let wait = wait_for_child(&mut child, label);
    let write = writer
        .join()
        .map_err(|_error| LocalWebError::Service(format!("{label} input task failed")))?;
    let errors = stderr
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    wait?;
    write?;
    if !errors.is_empty() {
        tracing::debug!(
            "{} wrote diagnostics: {}",
            label,
            String::from_utf8_lossy(&errors).trim()
        );
    }
    Ok(())
}

fn verify_current_database(paths: &Paths, project: &Project, engine: &EngineContext) -> Result<()> {
    match project.database {
        Database::Sqlite => {
            verify_sqlite_file(&engine.database_path)?;
            Ok(())
        }
        Database::Mariadb | Database::Mysql => {
            let defaults = mysql_credentials(paths, engine)?;
            let client = mysql_client(project.database)?;
            let mut command = Command::new(client);
            command
                .arg(mysql_defaults_argument(&defaults.path))
                .args(["--batch", "--skip-column-names", "--execute=SELECT 1"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            runtime_environment(&mut command);
            let output = command.output()?;
            if output.status.success() {
                Ok(())
            } else {
                Err(LocalWebError::Service(format!(
                    "database verification failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )))
            }
        }
        Database::Postgres => {
            let layout = RuntimeLayout::discover();
            let (_, _, psql) = layout.postgres()?;
            let credentials = postgres_credentials(paths, engine)?;
            let mut command = Command::new(psql);
            command
                .args(["--no-psqlrc", "--set=ON_ERROR_STOP=1", "--command=SELECT 1"])
                .env("PGPASSFILE", &credentials.path)
                .env("PGHOST", "127.0.0.1")
                .env("PGPORT", engine.port.to_string())
                .env("PGUSER", &engine.user)
                .env("PGDATABASE", &engine.database)
                .env("PGCONNECT_TIMEOUT", "10")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            runtime_environment(&mut command);
            let output = command.output()?;
            if output.status.success() {
                Ok(())
            } else {
                Err(LocalWebError::Service(format!(
                    "database verification failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )))
            }
        }
        Database::None => unreachable!("a project without a database was rejected"),
    }
}

fn wait_for_child(child: &mut Child, label: &str) -> Result<()> {
    let deadline = Instant::now() + OPERATION_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            return Err(LocalWebError::Service(format!(
                "{label} exited with {status}"
            )));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(LocalWebError::Service(format!(
                "{label} timed out after {} seconds",
                OPERATION_TIMEOUT.as_secs()
            )));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn escape_mysql(value: &str) -> String {
    value.replace('`', "``")
}

// ---------------------------------------------------------------------------
// SQLite
// ---------------------------------------------------------------------------

fn sq<T>(result: rusqlite::Result<T>) -> Result<T> {
    result.map_err(|error| LocalWebError::Service(format!("SQLite error: {error}")))
}

fn sqlite_backup(source: &Path, destination: &Path) -> Result<()> {
    let source = sq(Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ))?;
    let mut destination = sq(Connection::open(destination))?;
    let backup = sq(Backup::new(&source, &mut destination))?;
    sq(backup.run_to_completion(64, Duration::from_millis(20), None))?;
    drop(backup);
    sq(destination.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);"))?;
    Ok(())
}

fn sqlite_restore(source: &Path, destination: &Path) -> Result<()> {
    let source = sq(Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ))?;
    let mut destination = sq(Connection::open(destination))?;
    sq(destination.busy_timeout(Duration::from_secs(30)))?;
    let backup = sq(Backup::new(&source, &mut destination))?;
    sq(backup.run_to_completion(64, Duration::from_millis(20), None))?;
    Ok(())
}

fn verify_sqlite_file(path: &Path) -> Result<()> {
    let connection = sq(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ))?;
    let integrity: String =
        sq(connection.query_row("PRAGMA integrity_check", [], |row| row.get(0)))?;
    if integrity != "ok" {
        return Err(LocalWebError::Config(format!(
            "SQLite integrity check failed: {integrity}"
        )));
    }
    let _: i64 =
        sq(connection.query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0)))?;
    Ok(())
}

fn drop_sqlite_objects(path: &Path) -> Result<()> {
    let connection = sq(Connection::open(path))?;
    sq(connection.busy_timeout(Duration::from_secs(30)))?;
    sq(connection.execute_batch("PRAGMA foreign_keys=OFF;"))?;
    let objects = {
        let mut statement = sq(connection.prepare(
            "SELECT type, name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY CASE type WHEN 'trigger' THEN 0 WHEN 'view' THEN 1 WHEN 'index' THEN 2 WHEN 'table' THEN 3 ELSE 4 END",
        ))?;
        let rows = sq(statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }))?;
        sq(rows.collect::<rusqlite::Result<Vec<_>>>())?
    };
    for (kind, name) in objects {
        let keyword = match kind.as_str() {
            "trigger" => "TRIGGER",
            "view" => "VIEW",
            "index" => "INDEX",
            "table" => "TABLE",
            _ => continue,
        };
        let quoted = quote_sqlite_identifier(&name);
        sq(connection.execute_batch(&format!("DROP {keyword} IF EXISTS {quoted};")))?;
    }
    Ok(())
}

fn import_sqlite_file(database: &Path, source: &Path) -> Result<()> {
    let connection = sq(Connection::open(database))?;
    sq(connection.busy_timeout(Duration::from_secs(30)))?;
    sq(connection.execute_batch("PRAGMA foreign_keys=OFF;"))?;
    let mut reader = BufReader::new(File::open(source)?);
    let mut statement = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        statement.extend_from_slice(&line);
        let complete = {
            let text = std::str::from_utf8(&statement).map_err(|error| {
                LocalWebError::Config(format!("SQLite import contains invalid UTF-8 SQL: {error}"))
            })?;
            let c_string = std::ffi::CString::new(text).map_err(|_nul| {
                LocalWebError::Config("SQLite import contains a NUL byte".into())
            })?;
            // SAFETY: the C string is valid and NUL-terminated for the
            // duration of the call; SQLite only reads it.
            unsafe { ffi::sqlite3_complete(c_string.as_ptr()) != 0 }
        };
        if complete {
            let text = std::str::from_utf8(&statement).map_err(|error| {
                LocalWebError::Config(format!("SQLite import contains invalid UTF-8 SQL: {error}"))
            })?;
            sq(connection.execute_batch(text))?;
            statement.clear();
        }
    }
    if statement.iter().any(|byte| !byte.is_ascii_whitespace()) {
        return Err(LocalWebError::Config(
            "SQLite import ended with an incomplete SQL statement".into(),
        ));
    }
    Ok(())
}

struct CountingWriter<W: Write> {
    inner: W,
    bytes: u64,
}

impl<W: Write> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, bytes: 0 }
    }

    fn finish(self) -> (W, u64) {
        (self.inner, self.bytes)
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.bytes = self.bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn dump_sqlite(database: &Path, writer: &mut impl Write) -> Result<u64> {
    let connection = sq(Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ))?;
    let transaction = sq(connection.unchecked_transaction())?;
    let mut counting = CountingWriter::new(writer);
    writeln!(counting, "PRAGMA foreign_keys=OFF;")?;
    writeln!(counting, "BEGIN TRANSACTION;")?;
    let objects = {
        let mut statement = sq(connection.prepare(
            "SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' AND type IN ('table','view','index','trigger') ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 WHEN 'view' THEN 2 WHEN 'trigger' THEN 3 ELSE 4 END, name",
        ))?;
        let rows = sq(statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        }))?;
        sq(rows.collect::<rusqlite::Result<Vec<_>>>())?
    };
    let tables: Vec<_> = objects
        .iter()
        .filter(|(kind, _, _)| kind == "table")
        .cloned()
        .collect();
    for (_, name, sql) in &tables {
        writeln!(counting, "{sql};")?;
        dump_sqlite_rows(&connection, name, &mut counting)?;
    }
    for (_, _, sql) in objects.iter().filter(|(kind, _, _)| kind != "table") {
        writeln!(counting, "{sql};")?;
    }
    writeln!(counting, "COMMIT;")?;
    sq(transaction.commit())?;
    let (_, bytes) = counting.finish();
    Ok(bytes)
}

fn dump_sqlite_rows(connection: &Connection, table: &str, writer: &mut impl Write) -> Result<()> {
    let quoted = quote_sqlite_identifier(table);
    let columns = {
        let mut statement = sq(connection.prepare(&format!("PRAGMA table_xinfo({quoted})")))?;
        let rows = sq(statement.query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(6)?))
        }))?;
        sq(rows.collect::<rusqlite::Result<Vec<_>>>())?
            .into_iter()
            .filter(|(_, hidden)| *hidden == 0)
            .map(|(name, _)| name)
            .collect::<Vec<_>>()
    };
    if columns.is_empty() {
        return Ok(());
    }
    let column_list = columns
        .iter()
        .map(|name| quote_sqlite_identifier(name))
        .collect::<Vec<_>>()
        .join(", ");
    let mut statement = sq(connection.prepare(&format!("SELECT {column_list} FROM {quoted}")))?;
    let mut rows = sq(statement.query([]))?;
    while let Some(row) = sq(rows.next())? {
        write!(writer, "INSERT INTO {quoted} ({column_list}) VALUES (")?;
        for index in 0..columns.len() {
            if index > 0 {
                write!(writer, ", ")?;
            }
            write_sqlite_value(writer, sq(row.get_ref(index))?)?;
        }
        writeln!(writer, ");")?;
    }
    Ok(())
}

fn write_sqlite_value(writer: &mut impl Write, value: ValueRef<'_>) -> Result<()> {
    match value {
        ValueRef::Null => write!(writer, "NULL")?,
        ValueRef::Integer(value) => write!(writer, "{value}")?,
        ValueRef::Real(value) => {
            if value.is_finite() {
                write!(writer, "{value:?}")?;
            } else {
                write!(writer, "NULL")?;
            }
        }
        ValueRef::Text(value) => match std::str::from_utf8(value) {
            Ok(text) => write!(writer, "'{}'", text.replace('\'', "''"))?,
            Err(_) => write!(writer, "CAST(X'{}' AS TEXT)", hex(value))?,
        },
        ValueRef::Blob(value) => write!(writer, "X'{}'", hex(value))?,
    }
    Ok(())
}

fn quote_sqlite_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02X}");
    }
    output
}

// ---------------------------------------------------------------------------
// Compression and checksums
// ---------------------------------------------------------------------------

struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(self) -> (W, u64, String) {
        (self.inner, self.bytes, hex_digest(&self.hasher.finalize()))
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.bytes = self.bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

enum PayloadWriter<W: Write> {
    Raw(W),
    Gzip(flate2::write::GzEncoder<W>),
    Zstd(zstd::stream::write::Encoder<'static, W>),
}

impl<W: Write> PayloadWriter<W> {
    fn new(writer: W, compression: CompressionFormat) -> Result<Self> {
        match compression {
            CompressionFormat::None => Ok(Self::Raw(writer)),
            CompressionFormat::Gzip => Ok(Self::Gzip(flate2::write::GzEncoder::new(
                writer,
                flate2::Compression::default(),
            ))),
            CompressionFormat::Zstd => Ok(Self::Zstd(
                zstd::stream::write::Encoder::new(writer, ZSTD_LEVEL)
                    .map_err(|error| LocalWebError::Service(error.to_string()))?,
            )),
        }
    }

    fn finish(self) -> Result<W> {
        match self {
            Self::Raw(writer) => Ok(writer),
            Self::Gzip(writer) => writer
                .finish()
                .map_err(|error| LocalWebError::Service(error.to_string())),
            Self::Zstd(writer) => writer
                .finish()
                .map_err(|error| LocalWebError::Service(error.to_string())),
        }
    }
}

impl<W: Write> Write for PayloadWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Raw(writer) => writer.write(buf),
            Self::Gzip(writer) => writer.write(buf),
            Self::Zstd(writer) => writer.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Raw(writer) => writer.flush(),
            Self::Gzip(writer) => writer.flush(),
            Self::Zstd(writer) => writer.flush(),
        }
    }
}

fn create_private(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    Ok(options.open(path)?)
}

fn copy_file_compressed(
    source: &Path,
    file: File,
    compression: CompressionFormat,
) -> Result<(u64, u64, String)> {
    let input = File::open(source)?;
    let size = input.metadata()?.len();
    let mut writer = PayloadWriter::new(HashingWriter::new(file), compression)?;
    io::copy(&mut BufReader::new(input), &mut writer)?;
    finish_payload(writer, size)
}

fn decompress_to_file(
    source: &Path,
    destination: &Path,
    compression: CompressionFormat,
) -> Result<u64> {
    let input = BufReader::new(File::open(source)?);
    let reader: Box<dyn Read> = match compression {
        CompressionFormat::None => Box::new(input),
        CompressionFormat::Gzip => Box::new(flate2::read::MultiGzDecoder::new(input)),
        CompressionFormat::Zstd => Box::new(
            zstd::stream::read::Decoder::new(input)
                .map_err(|error| LocalWebError::Service(error.to_string()))?,
        ),
    };
    let file = create_private(destination)?;
    let mut writer = BufWriter::new(file);
    let written = io::copy(&mut reader.take(MAX_UNCOMPRESSED_BYTES + 1), &mut writer)?;
    if written > MAX_UNCOMPRESSED_BYTES {
        let _ = fs::remove_file(destination);
        return Err(LocalWebError::Config(
            "decompressed payload exceeds the configured safety limit".into(),
        ));
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(written)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(&hasher.finalize()))
}

fn hex_digest(digest: &[u8]) -> String {
    hex(digest).to_ascii_lowercase()
}

// ---------------------------------------------------------------------------
// SQL safety scanner
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanMode {
    Normal,
    SingleQuote,
    DoubleQuote,
    Backtick,
    LineComment,
    BlockComment,
    DollarQuote,
    CopyData,
}

struct SqlScanner {
    engine: Database,
    mode: ScanMode,
    dollar_tag: Vec<u8>,
    executable_comment: bool,
    copy_data: bool,
}

impl SqlScanner {
    fn new(engine: Database) -> Self {
        Self {
            engine,
            mode: ScanMode::Normal,
            dollar_tag: Vec::new(),
            executable_comment: false,
            copy_data: false,
        }
    }

    fn scan_file(&mut self, path: &Path, database: &str) -> Result<()> {
        let mut reader = BufReader::new(File::open(path)?);
        let mut line = Vec::new();
        let mut number = 0_u64;
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            number += 1;
            if self.copy_data {
                if line.trim_ascii() == b"\\." {
                    self.copy_data = false;
                    self.mode = ScanMode::Normal;
                }
                continue;
            }
            let visible = self.visible(&line);
            let text = String::from_utf8_lossy(&visible);
            check_sql_line(self.engine, &text, database, number)?;
            let upper = text.trim().to_ascii_uppercase();
            if upper.starts_with("COPY ") && upper.contains(" FROM STDIN") {
                self.copy_data = true;
                self.mode = ScanMode::CopyData;
            }
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one state machine keeps SQL literals, comments, and data blocks aligned"
    )]
    fn visible(&mut self, line: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(line.len());
        let mut index = 0;
        while index < line.len() {
            let byte = line[index];
            match self.mode {
                ScanMode::Normal => {
                    if self.copy_data {
                        output.push(b' ');
                        index += 1;
                        continue;
                    }
                    if byte == b'-' && line.get(index + 1) == Some(&b'-') {
                        self.mode = ScanMode::LineComment;
                        output.extend_from_slice(b"  ");
                        index += 2;
                    } else if byte == b'#' && self.engine != Database::Postgres {
                        self.mode = ScanMode::LineComment;
                        output.push(b' ');
                        index += 1;
                    } else if byte == b'/' && line.get(index + 1) == Some(&b'*') {
                        if line.get(index + 2) == Some(&b'!') {
                            self.executable_comment = true;
                            output.push(b' ');
                            index += 2;
                        } else {
                            self.mode = ScanMode::BlockComment;
                            output.extend_from_slice(b"  ");
                            index += 2;
                        }
                    } else if byte == b'\'' {
                        self.mode = ScanMode::SingleQuote;
                        output.push(b' ');
                        index += 1;
                    } else if byte == b'"' {
                        self.mode = ScanMode::DoubleQuote;
                        output.push(b' ');
                        index += 1;
                    } else if byte == b'`' && self.engine != Database::Postgres {
                        self.mode = ScanMode::Backtick;
                        output.push(byte);
                        index += 1;
                    } else if byte == b'$'
                        && self.engine == Database::Postgres
                        && let Some((tag, length)) = dollar_tag(&line[index..])
                    {
                        self.mode = ScanMode::DollarQuote;
                        self.dollar_tag = tag;
                        output.extend(std::iter::repeat_n(b' ', length));
                        index += length;
                    } else if byte == b'*'
                        && line.get(index + 1) == Some(&b'/')
                        && self.executable_comment
                    {
                        self.executable_comment = false;
                        output.extend_from_slice(b"  ");
                        index += 2;
                    } else {
                        output.push(byte);
                        index += 1;
                    }
                }
                ScanMode::SingleQuote => {
                    if byte == b'\\' {
                        output.extend_from_slice(b"  ");
                        index += 2;
                    } else if byte == b'\'' {
                        if line.get(index + 1) == Some(&b'\'') {
                            output.extend_from_slice(b"  ");
                            index += 2;
                        } else {
                            self.mode = ScanMode::Normal;
                            output.push(b' ');
                            index += 1;
                        }
                    } else {
                        output.push(if byte == b'\n' { b'\n' } else { b' ' });
                        index += 1;
                    }
                }
                ScanMode::DoubleQuote => {
                    if byte == b'\\' {
                        output.extend_from_slice(b"  ");
                        index += 2;
                    } else if byte == b'"' {
                        if line.get(index + 1) == Some(&b'"') {
                            output.extend_from_slice(b"  ");
                            index += 2;
                        } else {
                            self.mode = ScanMode::Normal;
                            output.push(b' ');
                            index += 1;
                        }
                    } else {
                        output.push(if byte == b'\n' { b'\n' } else { b' ' });
                        index += 1;
                    }
                }
                ScanMode::Backtick => {
                    if byte == b'`' {
                        if line.get(index + 1) == Some(&b'`') {
                            output.extend_from_slice(b"``");
                            index += 2;
                        } else {
                            self.mode = ScanMode::Normal;
                            output.push(b'`');
                            index += 1;
                        }
                    } else {
                        output.push(byte);
                        index += 1;
                    }
                }
                ScanMode::LineComment => {
                    output.push(if byte == b'\n' { b'\n' } else { b' ' });
                    index += 1;
                    if byte == b'\n' {
                        self.mode = ScanMode::Normal;
                    }
                }
                ScanMode::BlockComment => {
                    if byte == b'*' && line.get(index + 1) == Some(&b'/') {
                        self.mode = ScanMode::Normal;
                        output.extend_from_slice(b"  ");
                        index += 2;
                    } else {
                        output.push(if byte == b'\n' { b'\n' } else { b' ' });
                        index += 1;
                    }
                }
                ScanMode::DollarQuote => {
                    if line[index..].starts_with(&self.dollar_tag) {
                        let length = self.dollar_tag.len();
                        output.extend(std::iter::repeat_n(b' ', length));
                        index += length;
                        self.mode = ScanMode::Normal;
                        self.dollar_tag.clear();
                    } else {
                        output.push(if byte == b'\n' { b'\n' } else { b' ' });
                        index += 1;
                    }
                }
                ScanMode::CopyData => {
                    output.push(b' ');
                    index += 1;
                }
            }
        }
        if self.mode == ScanMode::LineComment {
            self.mode = ScanMode::Normal;
        }
        output
    }
}

fn dollar_tag(bytes: &[u8]) -> Option<(Vec<u8>, usize)> {
    if bytes.first() != Some(&b'$') {
        return None;
    }
    let mut index = 1;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'$' {
            return Some((bytes[..=index].to_vec(), index + 1));
        }
        if !(byte.is_ascii_alphanumeric() || byte == b'_') {
            return None;
        }
        index += 1;
    }
    None
}

#[expect(
    clippy::too_many_lines,
    reason = "each engine's forbidden statements stay visible in one scanner"
)]
fn check_sql_line(engine: Database, visible: &str, database: &str, line: u64) -> Result<()> {
    let keyword_text = if matches!(engine, Database::Mariadb | Database::Mysql) {
        blank_backticks(visible)
    } else {
        visible.to_owned()
    };
    let upper = keyword_text.to_ascii_uppercase();
    let reject = |reason: &str| -> Result<()> {
        Err(LocalWebError::Config(format!(
            "refusing to import SQL at line {line}: {reason}"
        )))
    };
    match engine {
        Database::Mariadb | Database::Mysql => {
            if let Some(command) = mysql_client_command(&upper) {
                return reject(&format!("client command `{command}` is not allowed"));
            }
            for phrase in [
                "USE",
                "CREATE DATABASE",
                "DROP DATABASE",
                "ALTER DATABASE",
                "CREATE USER",
                "ALTER USER",
                "DROP USER",
                "RENAME USER",
                "GRANT",
                "REVOKE",
                "SET GLOBAL",
                "SET PERSIST",
                "SET @@GLOBAL",
                "INSTALL PLUGIN",
                "INSTALL SONAME",
                "UNINSTALL PLUGIN",
                "UNINSTALL SONAME",
                "SHUTDOWN",
                "KILL",
                "RESET MASTER",
                "RESET REPLICA",
                "RESET SLAVE",
                "PURGE BINARY LOGS",
                "CHANGE MASTER",
                "CHANGE REPLICATION",
                "START REPLICA",
                "START SLAVE",
                "STOP REPLICA",
                "STOP SLAVE",
                "CLONE",
                "LOAD DATA",
                "LOAD XML",
                "INTO OUTFILE",
                "INTO DUMPFILE",
            ] {
                if contains_phrase(&upper, phrase) {
                    return reject(&format!("`{phrase}` is not allowed"));
                }
            }
            if let Some(cross) = mysql_cross_database_reference(visible, database) {
                return reject(&format!("it references database `{cross}`"));
            }
        }
        Database::Postgres => {
            for phrase in [
                "CREATE DATABASE",
                "DROP DATABASE",
                "ALTER DATABASE",
                "CREATE ROLE",
                "ALTER ROLE",
                "DROP ROLE",
                "CREATE USER",
                "ALTER USER",
                "DROP USER",
                "GRANT",
                "REVOKE",
                "SET ROLE",
                "SET SESSION AUTHORIZATION",
                "ALTER SYSTEM",
                "CREATE TABLESPACE",
                "DROP TABLESPACE",
                "COPY PROGRAM",
                "FROM PROGRAM",
                "TO PROGRAM",
                "LO_IMPORT",
                "LO_EXPORT",
                "DBLINK",
                "PG_READ_FILE",
                "PG_WRITE_FILE",
                "PG_EXECUTE_SERVER_PROGRAM",
            ] {
                if contains_phrase(&upper, phrase) {
                    return reject(&format!("`{phrase}` is not allowed"));
                }
            }
            if let Some(command) = postgres_meta_command(visible) {
                return reject(&format!("psql meta-command `{command}` is not allowed"));
            }
        }
        Database::Sqlite => {
            // The SQLite dumper emits `PRAGMA foreign_keys=OFF` so an export
            // can be re-imported without deferring to declaration order. Allow
            // that one harmless switch and reject every other pragma.
            if upper.split(';').any(|statement| {
                contains_phrase(statement, "PRAGMA") && !sqlite_pragma_is_allowed(statement)
            }) {
                return reject("only `PRAGMA foreign_keys=ON/OFF` is allowed");
            }
            for phrase in [
                "ATTACH",
                "DETACH",
                "VACUUM",
                "LOAD_EXTENSION",
                "CREATE VIRTUAL TABLE",
            ] {
                if contains_phrase(&upper, phrase) {
                    return reject(&format!("`{phrase}` is not allowed"));
                }
            }
        }
        Database::None => {}
    }
    Ok(())
}

fn blank_backticks(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut inside = false;
    for character in text.chars() {
        if character == '`' {
            inside = !inside;
            output.push(' ');
        } else if inside || character.is_ascii() {
            output.push(if inside { ' ' } else { character });
        } else {
            output.push(' ');
        }
    }
    output
}

fn mysql_client_command(upper: &str) -> Option<&'static str> {
    let trimmed = upper.trim_start();
    ["SOURCE", "SYSTEM", "\\!", "\\.", "\\C", "\\R"]
        .into_iter()
        .find(|command| trimmed.starts_with(command))
}

fn postgres_meta_command(visible: &str) -> Option<&str> {
    let trimmed = visible.trim_start();
    if trimmed.starts_with("\\.") {
        return None;
    }
    if let Some(line) = trimmed.lines().next()
        && let Some(argument) = line
            .strip_prefix("\\restrict")
            .or_else(|| line.strip_prefix("\\unrestrict"))
        && argument
            .trim_start()
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        // pg_dump 17.6+ wraps dumps in these psql restriction markers. They
        // do not grant database access; every other meta-command stays barred.
        return None;
    }
    if trimmed.starts_with('\\') {
        return Some(trimmed.lines().next().unwrap_or("\\"));
    }
    None
}

fn contains_phrase(haystack: &str, phrase: &str) -> bool {
    let mut offset = 0;
    while let Some(found) = haystack[offset..].find(phrase) {
        let start = offset + found;
        let end = start + phrase.len();
        let before = haystack[..start].chars().next_back();
        let after = haystack[end..].chars().next();
        if !before.is_some_and(is_word_character) && !after.is_some_and(is_word_character) {
            return true;
        }
        offset = end;
    }
    false
}

fn is_word_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

fn sqlite_pragma_is_allowed(statement: &str) -> bool {
    let normalized = statement
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase();
    matches!(
        normalized.as_str(),
        "PRAGMAFOREIGN_KEYS=OFF" | "PRAGMAFOREIGN_KEYS=ON"
    )
}

fn mysql_cross_database_reference(visible: &str, database: &str) -> Option<String> {
    const INTRODUCERS: &[&str] = &[
        "FROM",
        "JOIN",
        "INTO",
        "UPDATE",
        "TABLE",
        "REFERENCES",
        "TABLES",
        "TRUNCATE",
        "RENAME",
        "CALL",
        "VIEW",
        "PROCEDURE",
        "FUNCTION",
        "TRIGGER",
        "EVENT",
        "LIKE",
        "TO",
    ];
    const MODIFIERS: &[&str] = &[
        "IF",
        "NOT",
        "EXISTS",
        "LOW_PRIORITY",
        "HIGH_PRIORITY",
        "DELAYED",
        "IGNORE",
        "QUICK",
    ];
    let bytes = visible.as_bytes();
    let mut index = 0;
    let mut expect_table = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if byte == b'`' {
            let start = index + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end] != b'`' {
                end += 1;
            }
            if end >= bytes.len() {
                break;
            }
            let name = &visible[start..end];
            if expect_table
                && follows_with_dot(bytes, end + 1)
                && !name.eq_ignore_ascii_case(database)
            {
                return Some(name.to_owned());
            }
            expect_table = false;
            index = end + 1;
            continue;
        }
        if byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$' {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'_' | b'$'))
            {
                index += 1;
            }
            let word = &visible[start..index];
            let upper = word.to_ascii_uppercase();
            if expect_table {
                if MODIFIERS.contains(&upper.as_str()) {
                    continue;
                }
                if follows_with_dot(bytes, index) && !word.eq_ignore_ascii_case(database) {
                    return Some(word.to_owned());
                }
                expect_table = false;
                continue;
            }
            if INTRODUCERS.contains(&upper.as_str())
                || (upper == "ON" && mysql_index_target(&visible[..start]))
            {
                expect_table = true;
            }
            continue;
        }
        expect_table = false;
        index += 1;
    }
    None
}

fn mysql_index_target(prefix: &str) -> bool {
    let prefix = prefix.to_ascii_uppercase();
    contains_phrase(&prefix, "INDEX")
        && (contains_phrase(&prefix, "CREATE") || contains_phrase(&prefix, "DROP"))
}

fn follows_with_dot(bytes: &[u8], mut index: usize) -> bool {
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    bytes.get(index) == Some(&b'.')
}

fn scan_sql(engine: Database, path: &Path, database: &str) -> Result<()> {
    let mut scanner = SqlScanner::new(engine);
    scanner.scan_file(path, database)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn scanner_rejects_cross_database_and_admin_statements() {
        for sql in [
            "CREATE DATABASE other;",
            "USE other;",
            "DROP DATABASE other;",
            "GRANT ALL ON *.* TO 'x'@'%';",
            "SELECT * INTO OUTFILE '/tmp/x' FROM users;",
            "CREATE TABLE `other`.`users` (id int);",
        ] {
            let mut scanner = SqlScanner::new(Database::Mariadb);
            let mut visible = Vec::new();
            for line in sql.as_bytes().split_inclusive(|byte| *byte == b'\n') {
                visible.extend(scanner.visible(line));
            }
            let error = check_sql_line(
                Database::Mariadb,
                &String::from_utf8_lossy(&visible),
                "mine",
                1,
            )
            .unwrap_err();
            assert!(error.to_string().contains("refusing"), "{error}");
        }
    }

    #[test]
    fn scanner_allows_qualified_target_and_strings() {
        for sql in [
            b"INSERT INTO `mine`.`users` VALUES ('USE other;');\n".as_slice(),
            b"CREATE VIEW `names` AS SELECT `users`.`id` FROM `users`;\n".as_slice(),
            b"CREATE TABLE IF NOT EXISTS `mine`.`users` (id int);\n".as_slice(),
        ] {
            let mut scanner = SqlScanner::new(Database::Mariadb);
            let visible = scanner.visible(sql);
            check_sql_line(
                Database::Mariadb,
                &String::from_utf8_lossy(&visible),
                "mine",
                1,
            )
            .unwrap();
        }
    }

    #[test]
    fn scanner_rejects_unquoted_cross_database_qualifiers() {
        for sql in [
            "SELECT * FROM other.users;",
            "INSERT INTO other.users (id) VALUES (1);",
            "CREATE TABLE IF NOT EXISTS other.users (id int);",
            "UPDATE other.users SET id = 1;",
            "RENAME TABLE users TO other.users;",
            "CREATE INDEX users_id ON other.users (id);",
        ] {
            let mut scanner = SqlScanner::new(Database::Mariadb);
            let visible = scanner.visible(sql.as_bytes());
            let error = check_sql_line(
                Database::Mariadb,
                &String::from_utf8_lossy(&visible),
                "mine",
                1,
            )
            .unwrap_err();
            assert!(error.to_string().contains("database `other`"), "{error}");
        }
    }

    #[test]
    fn postgres_dollar_quotes_and_copy_data_are_not_mistaken_for_commands() {
        let mut scanner = SqlScanner::new(Database::Postgres);
        let visible = scanner.visible(
            b"CREATE FUNCTION f() RETURNS void AS $body$ GRANT ALL; $body$ LANGUAGE sql;\n",
        );
        check_sql_line(
            Database::Postgres,
            &String::from_utf8_lossy(&visible),
            "mine",
            1,
        )
        .unwrap();
        let visible = scanner.visible(b"COPY public.users FROM stdin;\n");
        assert!(
            String::from_utf8_lossy(&visible)
                .to_ascii_uppercase()
                .contains("FROM STDIN")
        );
        scanner.copy_data = true;
        scanner.mode = ScanMode::CopyData;
        let visible = scanner.visible(b"GRANT ALL;\n");
        assert_eq!(String::from_utf8_lossy(&visible).trim(), "");
        check_sql_line(
            Database::Postgres,
            &String::from_utf8_lossy(&visible),
            "mine",
            2,
        )
        .unwrap();
    }

    #[test]
    fn copy_data_blocks_hide_statements_until_the_terminator() {
        let base = std::env::temp_dir().join(format!("lw-scan-{}", Uuid::new_v4()));
        fs::create_dir_all(&base).unwrap();
        let path = base.join("dump.sql");
        fs::write(
            &path,
            b"COPY public.users FROM stdin;\nGRANT ALL;\nDROP DATABASE other;\n\\.\nSELECT 1;\n",
        )
        .unwrap();
        scan_sql(Database::Postgres, &path, "mine").unwrap();
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn snapshot_names_and_versions_are_checked() {
        validate_snapshot_name("before-update").unwrap();
        assert!(validate_snapshot_name("../x").is_err());
        assert_eq!(version_major("11.8.6-MariaDB"), Some("11"));
        assert_eq!(version_major("PostgreSQL 18.2"), Some("18"));
        ensure_version_compatible("11.4.2", "11.8.6", "s").unwrap();
        ensure_version_compatible("10.11", "11.8", "s").unwrap_err();
    }

    #[test]
    fn sqlite_dump_round_trips_unicode_and_blobs() {
        let base = std::env::temp_dir().join(format!("lw-sqlite-{}", Uuid::new_v4()));
        fs::create_dir_all(&base).unwrap();
        let source = base.join("source.sqlite");
        let connection = Connection::open(&source).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, data BLOB, value REAL);\
                 CREATE INDEX items_name ON items(name);\
                 INSERT INTO items(name, data, value) VALUES ('caffè ☕', X'00FF10', 1.5);",
            )
            .unwrap();
        drop(connection);
        let mut dump = Vec::new();
        dump_sqlite(&source, &mut dump).unwrap();
        let target = base.join("target.sqlite");
        let dump_path = base.join("dump.sql");
        fs::write(&dump_path, &dump).unwrap();
        scan_sql(Database::Sqlite, &dump_path, "mine").unwrap();
        import_sqlite_file(&target, &dump_path).unwrap();
        let connection = Connection::open(&target).unwrap();
        let (name, data, value): (String, Vec<u8>, f64) = connection
            .query_row("SELECT name, data, value FROM items", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(name, "caffè ☕");
        assert_eq!(data, [0x00, 0xFF, 0x10]);
        assert!((value - 1.5).abs() < f64::EPSILON);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn sqlite_dump_preserves_views_and_triggers() {
        let base = std::env::temp_dir().join(format!("lw-sqlite-view-{}", Uuid::new_v4()));
        fs::create_dir_all(&base).unwrap();
        let source = base.join("source.sqlite");
        let connection = Connection::open(&source).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT);\
                 CREATE TABLE audit (value TEXT);\
                 CREATE VIEW item_names AS SELECT name FROM items;\
                 CREATE TRIGGER items_audit AFTER INSERT ON items BEGIN INSERT INTO audit(value) VALUES (NEW.name); END;\
                 INSERT INTO items(name) VALUES ('first');",
            )
            .unwrap();
        drop(connection);
        let mut dump = Vec::new();
        dump_sqlite(&source, &mut dump).unwrap();
        let target = base.join("target.sqlite");
        let dump_path = base.join("dump.sql");
        fs::write(&dump_path, &dump).unwrap();
        scan_sql(Database::Sqlite, &dump_path, "mine").unwrap();
        import_sqlite_file(&target, &dump_path).unwrap();
        let connection = Connection::open(&target).unwrap();
        let view: String = connection
            .query_row(
                "SELECT name FROM items WHERE name = (SELECT name FROM item_names)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(view, "first");
        connection
            .execute("INSERT INTO items(name) VALUES ('second')", [])
            .unwrap();
        let audited: i64 = connection
            .query_row("SELECT count(*) FROM audit", [], |row| row.get(0))
            .unwrap();
        assert_eq!(audited, 2);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn sqlite_snapshot_payload_round_trips_through_the_store() {
        let base = std::env::temp_dir().join(format!("lw-sqlite-snapshot-{}", Uuid::new_v4()));
        let paths = test_paths(&base);
        let source = paths.data.join("source.sqlite");
        let connection = Connection::open(&source).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT);\
                 INSERT INTO items(name) VALUES ('original');",
            )
            .unwrap();
        drop(connection);
        let mut project = test_project(&paths);
        project.database = Database::Sqlite;
        let engine = EngineContext {
            engine: Database::Sqlite,
            version: sqlite_version(&source).unwrap(),
            database: project.name.clone(),
            user: String::new(),
            password: String::new(),
            port: 0,
            database_path: source.clone(),
        };
        let store = SnapshotStore::new(&paths, &project).unwrap();
        let snapshot = store
            .create(
                &project,
                NewSnapshot {
                    name: "sqlite-baseline".into(),
                    created_at: chrono::Utc::now(),
                    format: SnapshotFormat::Sqlite,
                    compression: CompressionFormat::Zstd,
                    engine_version: engine.version.clone(),
                    safety: false,
                    description: None,
                },
                |payload, compression| {
                    write_snapshot_payload(&paths, &project, &engine, payload, compression)
                },
            )
            .unwrap();
        assert_eq!(snapshot.format, SnapshotFormat::Sqlite);
        assert_eq!(snapshot.engine, Database::Sqlite);
        let (found, directory) = store
            .find(&SnapshotSelector::Name("sqlite-baseline".into()))
            .unwrap();
        let payload = directory.join(payload_name(found.format));
        verify_payload(&payload, &found).unwrap();
        assert_eq!(&fs::read(&payload).unwrap()[..4], &[0x28, 0xB5, 0x2F, 0xFD]);
        verify_sqlite_file(&source).unwrap();

        let connection = Connection::open(&source).unwrap();
        connection
            .execute("INSERT INTO items(name) VALUES ('changed')", [])
            .unwrap();
        drop(connection);
        let restored = base.join("restored.sqlite");
        decompress_to_file(&payload, &restored, found.compression).unwrap();
        verify_sqlite_file(&restored).unwrap();
        sqlite_restore(&restored, &source).unwrap();
        let connection = Connection::open(&source).unwrap();
        let names: Vec<String> = connection
            .prepare("SELECT name FROM items ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(names, ["original"]);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn sqlite_complete_splits_trigger_bodies() {
        let sql = b"CREATE TABLE t(x);\nCREATE TRIGGER tr AFTER INSERT ON t BEGIN\nSELECT 1;\nSELECT 2;\nEND;\n";
        let mut statement = Vec::new();
        let mut count = 0;
        for line in sql.split_inclusive(|byte| *byte == b'\n') {
            statement.extend_from_slice(line);
            let text = std::str::from_utf8(&statement).unwrap();
            let c = std::ffi::CString::new(text).unwrap();
            // SAFETY: the C string is valid and NUL-terminated.
            if unsafe { ffi::sqlite3_complete(c.as_ptr()) } != 0 {
                count += 1;
                statement.clear();
            }
        }
        assert_eq!(count, 2);
    }

    #[test]
    fn postgres_restriction_markers_are_allowed_and_other_meta_commands_are_not() {
        let mut scanner = SqlScanner::new(Database::Postgres);
        for marker in [
            b"\\restrict abc123\n".as_slice(),
            b"\\unrestrict abc123\n".as_slice(),
        ] {
            let visible = scanner.visible(marker);
            check_sql_line(
                Database::Postgres,
                &String::from_utf8_lossy(&visible),
                "mine",
                1,
            )
            .unwrap();
        }
        for sql in [
            "\\connect other\n",
            "\\copy users FROM stdin\n",
            "\\i /etc/passwd\n",
            "\\! id\n",
        ] {
            let mut scanner = SqlScanner::new(Database::Postgres);
            let visible = scanner.visible(sql.as_bytes());
            let error = check_sql_line(
                Database::Postgres,
                &String::from_utf8_lossy(&visible),
                "mine",
                1,
            )
            .unwrap_err();
            assert!(error.to_string().contains("meta-command"), "{error}");
        }
    }

    #[test]
    fn sqlite_import_scanner_blocks_privileged_statements() {
        for sql in [
            "ATTACH DATABASE '/tmp/other.sqlite' AS other;",
            "PRAGMA writable_schema=ON;",
            "VACUUM INTO '/tmp/copy.sqlite';",
            "CREATE VIRTUAL TABLE docs USING fts5(body);",
        ] {
            let mut scanner = SqlScanner::new(Database::Sqlite);
            let visible = scanner.visible(sql.as_bytes());
            let error = check_sql_line(
                Database::Sqlite,
                &String::from_utf8_lossy(&visible),
                "mine",
                1,
            )
            .unwrap_err();
            assert!(error.to_string().contains("refusing"), "{error}");
        }
        let mut scanner = SqlScanner::new(Database::Sqlite);
        let visible = scanner.visible(b"INSERT INTO items(name) VALUES ('ATTACH DATABASE x');\n");
        check_sql_line(
            Database::Sqlite,
            &String::from_utf8_lossy(&visible),
            "mine",
            1,
        )
        .unwrap();
        for allowed in [
            "PRAGMA foreign_keys=OFF;\n",
            "PRAGMA foreign_keys = ON;\n",
            "PRAGMA FOREIGN_KEYS=off;\n",
        ] {
            let mut scanner = SqlScanner::new(Database::Sqlite);
            let visible = scanner.visible(allowed.as_bytes());
            check_sql_line(
                Database::Sqlite,
                &String::from_utf8_lossy(&visible),
                "mine",
                1,
            )
            .unwrap();
        }
        for rejected in [
            "PRAGMA journal_mode=WAL;\n",
            "PRAGMA foreign_keys=OFF; VACUUM;\n",
        ] {
            let mut scanner = SqlScanner::new(Database::Sqlite);
            let visible = scanner.visible(rejected.as_bytes());
            check_sql_line(
                Database::Sqlite,
                &String::from_utf8_lossy(&visible),
                "mine",
                1,
            )
            .unwrap_err();
        }
    }

    #[test]
    fn expansion_limits_reject_bombs() {
        let path = Path::new("archive.sql.gz");
        check_expansion(path, 1_000, 1_000).unwrap();
        assert!(
            check_expansion(path, 1, 100 * 1024 * 1024).is_err(),
            "high ratio must be rejected"
        );
        assert!(
            check_expansion(path, 1024, MAX_UNCOMPRESSED_BYTES + 1).is_err(),
            "oversized payload must be rejected"
        );
    }

    fn test_paths(base: &Path) -> Paths {
        let paths = Paths {
            config: base.join("config"),
            data: base.join("data"),
            state: base.join("state"),
            runtime: base.join("runtime"),
        };
        paths.create().unwrap();
        paths
    }

    fn test_project(paths: &Paths) -> Project {
        Project {
            schema_version: 1,
            id: Uuid::new_v4(),
            name: "store-test".into(),
            path: paths.data.join("source"),
            hostname: "store-test.test".into(),
            web_port: Some(18_000),
            database_port: Some(19_000),
            document_root: PathBuf::from("."),
            profile: localweb_core::Profile::GenericPhp,
            web_server: localweb_core::WebServer::Nginx,
            php: "8.5".into(),
            composer: "2".into(),
            node: "24".into(),
            database: Database::Mariadb,
            cache: localweb_core::Cache::None,
            supervisor: localweb_core::SupervisorConfig::default(),
            cron: Vec::new(),
            mail: false,
            autostart: false,
            enabled: true,
            environment: std::collections::BTreeMap::new(),
            resources: localweb_core::ResourceSettings::default(),
            created_at: chrono::Utc::now(),
        }
    }

    fn fake_payload(path: &Path, contents: &[u8]) -> Result<(u64, u64, String)> {
        let mut file = create_private(path)?;
        file.write_all(contents)?;
        file.sync_all()?;
        let checksum = sha256_file(path)?;
        Ok((contents.len() as u64, contents.len() as u64, checksum))
    }

    #[test]
    fn snapshot_store_publishes_private_ordered_snapshots() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt as _;
        let base = std::env::temp_dir().join(format!("lw-store-{}", Uuid::new_v4()));
        let paths = test_paths(&base);
        let project = test_project(&paths);
        let store = SnapshotStore::new(&paths, &project).unwrap();
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&store.root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let first = store
            .create(
                &project,
                NewSnapshot {
                    name: "first".into(),
                    created_at: chrono::Utc::now(),
                    format: SnapshotFormat::Sql,
                    compression: CompressionFormat::Zstd,
                    engine_version: "11.8".into(),
                    safety: false,
                    description: None,
                },
                |payload, _| fake_payload(payload, b"first"),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let second = store
            .create(
                &project,
                NewSnapshot {
                    name: "second".into(),
                    created_at: chrono::Utc::now(),
                    format: SnapshotFormat::Sql,
                    compression: CompressionFormat::Zstd,
                    engine_version: "11.8".into(),
                    safety: true,
                    description: None,
                },
                |payload, _| fake_payload(payload, b"second"),
            )
            .unwrap();
        let listed = store.list().unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|snapshot| snapshot.name.as_str())
                .collect::<Vec<_>>(),
            ["second", "first"]
        );
        let (_, directory) = store.find(&SnapshotSelector::Latest).unwrap();
        assert_eq!(directory.file_name().unwrap(), first.id.as_str());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(directory.join("metadata.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(directory.join("database.sql.zst"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        store
            .find(&SnapshotSelector::Name(second.id.clone()))
            .unwrap();
        let error = store
            .create(
                &project,
                NewSnapshot {
                    name: "first".into(),
                    created_at: chrono::Utc::now(),
                    format: SnapshotFormat::Sql,
                    compression: CompressionFormat::Zstd,
                    engine_version: "11.8".into(),
                    safety: false,
                    description: None,
                },
                |payload, _| fake_payload(payload, b"duplicate"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        let deleted = store
            .delete(&SnapshotSelector::Name("first".into()))
            .unwrap();
        assert_eq!(deleted.reclaimed_bytes, Some(5));
        let cleaned = store.cleanup().unwrap();
        assert_eq!(cleaned.removed, Some(1));
        assert_eq!(store.list().unwrap().len(), 0);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn incomplete_and_corrupt_snapshots_are_not_listed() {
        let base = std::env::temp_dir().join(format!("lw-corrupt-{}", Uuid::new_v4()));
        let paths = test_paths(&base);
        let project = test_project(&paths);
        let store = SnapshotStore::new(&paths, &project).unwrap();
        let snapshot = store
            .create(
                &project,
                NewSnapshot {
                    name: "complete".into(),
                    created_at: chrono::Utc::now(),
                    format: SnapshotFormat::Sql,
                    compression: CompressionFormat::Zstd,
                    engine_version: "11.8".into(),
                    safety: false,
                    description: None,
                },
                |payload, _| fake_payload(payload, b"payload"),
            )
            .unwrap();
        let payload = store.root.join(&snapshot.id).join("database.sql.zst");
        let original = fs::read(&payload).unwrap();
        fs::write(&payload, b"short").unwrap();
        assert_eq!(store.list().unwrap().len(), 0);
        fs::write(&payload, &original).unwrap();
        fs::write(&payload, b"corrupt").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        let (found, _) = store
            .find(&SnapshotSelector::Name("complete".into()))
            .unwrap();
        verify_payload(&payload, &found).unwrap_err();
        fs::write(&payload, &original).unwrap();
        verify_payload(&payload, &found).unwrap();

        let temp = store.root.join(".tmp-interrupted");
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("metadata.json"), b"partial").unwrap();
        let missing = store.root.join("20200101-000000-deadbeef");
        fs::create_dir_all(&missing).unwrap();
        let mut metadata = snapshot.clone();
        metadata.id = "20200101-000000-deadbeef".into();
        metadata.name = "unfinished".into();
        fs::write(
            missing.join("metadata.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn compression_round_trip() {
        let base = std::env::temp_dir().join(format!("lw-compress-{}", Uuid::new_v4()));
        fs::create_dir_all(&base).unwrap();
        for compression in [
            CompressionFormat::None,
            CompressionFormat::Gzip,
            CompressionFormat::Zstd,
        ] {
            let payload = base.join(format!("payload-{}", compression.as_str()));
            let file = create_private(&payload).unwrap();
            let mut writer = PayloadWriter::new(HashingWriter::new(file), compression).unwrap();
            writer.write_all(b"hello database\n").unwrap();
            let (size, logical, checksum) = finish_payload(writer, 15).unwrap();
            assert!(size > 0);
            assert_eq!(logical, 15);
            assert_eq!(checksum.len(), 64);
            let restored = base.join(format!("restored-{}", compression.as_str()));
            decompress_to_file(&payload, &restored, compression).unwrap();
            assert_eq!(fs::read(restored).unwrap(), b"hello database\n");
        }
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn hashing_writer_matches_sha256_file() {
        let base = std::env::temp_dir().join(format!("lw-hash-{}", Uuid::new_v4()));
        fs::create_dir_all(&base).unwrap();
        let path = base.join("file");
        let mut file = create_private(&path).unwrap();
        let mut hasher = HashingWriter::new(&mut file);
        hasher.write_all(b"abc").unwrap();
        let (_, _, checksum) = hasher.finish();
        assert_eq!(
            checksum,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn io_cursor_compression_is_deterministic() {
        let mut output = Vec::new();
        let mut writer = PayloadWriter::new(&mut output, CompressionFormat::Gzip).unwrap();
        writer.write_all(b"line\n").unwrap();
        writer.finish().unwrap();
        assert!(output.starts_with(&[0x1f, 0x8b]));
        let _ = Cursor::new(output);
    }
}
