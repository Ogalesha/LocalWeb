# Database snapshots, export and import

LocalWeb manages MariaDB, MySQL, PostgreSQL and SQLite databases for projects.
Every project with a database also gets database-only snapshots, restore, SQL
export and SQL import through the desktop app, the CLI and the MCP server.
All three clients send the same daemon requests, so the checks and safety
behaviour are identical.

Snapshots contain **database state only**. They never include project files,
PHP configuration, nginx or Apache configuration, Redis data, Mailpit
messages, certificates, environment files or other services. Use normal file
backups, version control and deployment tooling for source code.

## Snapshot model

A snapshot is a named, read-only copy of one project database:

- MariaDB, MySQL and PostgreSQL snapshots are logical SQL dumps with views,
  triggers, routines, events and ordinary indexes and constraints included
  where the engine and dump tool support them.
- SQLite snapshots use the SQLite online backup API. They are a consistent
  database image, not a live copy of the file next to a running writer.
- Every payload is compressed with Zstandard.
- A snapshot records a stable ID, a unique display name, the project ID and
  name, the engine, the engine version, the creation time in UTC, the stored
  and logical sizes, and a SHA-256 checksum.

Without `--name`, LocalWeb generates a timestamped name such as
`snapshot-20261116-153012` and adds a short suffix if two snapshots are
created in the same second. User-supplied names may contain letters, digits,
dots, underscores and hyphens, must start with a letter or digit, and are
limited to 64 characters. `latest` is reserved and cannot be used as a name.

Safety snapshots are created automatically before a restore or import unless
that step is disabled. Names begin with `safety-before-operation-`. They are
listed like other snapshots but are never selected by `--latest`; restore or
delete one by its explicit name.

### Storage and publication

Snapshots are stored in the platform data directory:

```text
# Linux (Ubuntu, Arch/Omarchy), including XDG_DATA_HOME overrides
${XDG_DATA_HOME:-~/.local/share}/localweb/databases/snapshots/<project-id>/
  <snapshot-id>/
    metadata.json
    database.sql.zst       # MariaDB, MySQL, PostgreSQL
    database.sqlite.zst    # SQLite

# macOS (same XDG fallback unless XDG_DATA_HOME is set)
~/.local/share/localweb/databases/snapshots/<project-id>/

# Windows
%LOCALAPPDATA%\LocalWeb\data\databases\snapshots\<project-id>\
```

On Unix, directories are private to the current account (`0700`) and files are
created with `0600`. Windows inherits the private ACL of the per-user
`%LOCALAPPDATA%` tree; LocalWeb does not weaken it. A snapshot is written to a
hidden temporary directory, flushed, and published with an atomic rename only
after its payload and metadata are complete. A crash or interruption therefore
leaves an incomplete directory that is never listed. Listing also checks that
the payload is a regular file and that its size matches the metadata. Restore
verifies the full SHA-256 checksum before it changes anything.

### Platform support

Snapshot, restore, export and import are available on every supported OS:

- **Ubuntu and Arch/Omarchy** use the tools bundled in the LocalWeb package
  (`mariadb-dump` or `mysqldump`, `pg_dump`, `pg_restore` and `psql`). SQLite is
  compiled into LocalWeb, so SQLite projects need no external client.
- **macOS** uses the same bundled tools when present and otherwise resolves
  MariaDB, MySQL and PostgreSQL from Homebrew installations. Versioned
  formulae such as `mariadb@11.8`, `mysql@8.4` and `postgresql@18` are
  recognized when the unversioned link is absent.
- **Windows** resolves the tools from the LocalWeb runtime, Scoop or
  Chocolatey installations. SQLite is compiled into LocalWeb.

The GUI accepts native absolute paths on each platform, and the CLI resolves
relative paths against the current directory before sending them to the
daemon. Snapshot storage, checksums, locking, cancellation and error handling
are identical on all platforms.

## Command-line reference

```text
localweb db snapshot create <project> [--name <name>] [--json]
localweb db snapshot list <project> [--json]
localweb db snapshot restore <project> <name|--latest>
    [--yes] [--no-safety] [--json]
localweb db snapshot delete <project> <name> [--yes] [--json]
localweb db snapshot cleanup <project> [--yes] [--json]
localweb db export <project> <path> [--force|--overwrite] [--json]
localweb db export <project> --stdout [--compression none|gzip|zstd] [--json]
localweb db import <project> <path> [--no-drop] [--no-safety] [--yes] [--json]
```

Examples:

```bash
# Take a named snapshot before a migration.
localweb db snapshot create acme --name before-migration

# Review snapshots, newest first.
localweb db snapshot list acme

# Restore the newest normal snapshot. A safety snapshot is created first.
localweb db snapshot restore acme --latest --yes

# Restore an exact snapshot without the automatic safety copy.
localweb db snapshot restore acme before-migration --no-safety --yes

# Delete one snapshot, or every snapshot for the project.
localweb db snapshot delete acme before-migration --yes
localweb db snapshot cleanup acme --yes

# Export using the compression implied by the extension.
localweb db export acme ~/Backups/acme.sql
localweb db export acme ~/Backups/acme.sql.gz
localweb db export acme ~/Backups/acme.sql.zst

# Stream a compressed export without a destination file.
localweb db export acme --stdout --compression zstd > acme.sql.zst

# Import replaces existing objects by default and takes a safety snapshot.
localweb db import acme ~/Backups/acme.sql.zst --yes

# Merge into the existing schema instead of dropping objects first.
localweb db import acme ~/Backups/partial.sql --no-drop --yes
```

Mutating commands prompt on a terminal and require `--yes` when standard input
is not a terminal. `--json` prints the daemon response as JSON. The GUI and
MCP server use their own confirmation step; MCP calls must pass
`"confirm": true`.

### Export rules

The destination must be absolute and end in `.sql`, `.sql.gz` or `.sql.zst`.
The extension selects compression; a mismatch between `--compression` and the
extension is rejected. An existing file is never replaced unless `--force`
(alias `--overwrite`) is passed explicitly. Exports are streamed through a
private temporary file and published only when complete, so a failed export
does not leave a truncated destination.

`--stdout` writes only dump data to standard output. Progress and diagnostics
go to standard error, which makes it safe to pipe into another program:

```bash
localweb db export acme --stdout --compression gzip | gzip -t
```

### Import rules

Import replaces the project's database objects by default:

- MariaDB and MySQL tables, views, routines and events are dropped first.
- PostgreSQL schemas other than `pg_*` and `information_schema` are dropped,
  then a clean `public` schema is created.
- SQLite tables, views, indexes and triggers are dropped first.

Use `--no-drop` to keep existing objects and apply the dump on top of them.
The engine's normal `CREATE` semantics then decide whether existing objects
are replaced, reused or rejected.

A safety snapshot is created by default before any object is dropped. The
snapshot is recorded only after the source has been read, decompressed and
validated, so a malformed file does not create noise. If the import still
fails, the error names the safety snapshot that can be restored explicitly.

## Confinement and import security

Imports are confined to the project database and use the project's generated
application credentials, never the shared administrator credentials.

Before any object is changed, LocalWeb reads the SQL and rejects statements
that would escape the project or run outside the database engine, including:

- cross-database commands such as `USE`, `CREATE DATABASE`, `DROP DATABASE`,
  `GRANT`, `REVOKE` and `ALTER USER`;
- qualified object names that target another database;
- MySQL/MariaDB `INTO OUTFILE`, `LOAD DATA LOCAL INFILE`, `SOURCE`, `SYSTEM`
  and client commands;
- PostgreSQL role, database, tablespace, `dblink`, `COPY ... FROM PROGRAM` and
  arbitrary `psql` meta-commands. The `\restrict` and `\unrestrict` markers
  emitted by current `pg_dump` releases are allowed;
- SQLite `ATTACH`, `DETACH`, `PRAGMA`, `VACUUM` and virtual-table creation.
  `PRAGMA foreign_keys=ON` and `PRAGMA foreign_keys=OFF` are the only
  exceptions, because LocalWeb's own exports use the `OFF` form so they can be
  re-imported without depending on table declaration order.

The scanner understands quoted strings, quoted identifiers, comments,
PostgreSQL dollar-quoted bodies and `COPY ... FROM stdin` data blocks so that
keywords inside data are not treated as commands. A file that cannot be
validated is rejected before the safety snapshot, drop or import step.

Compressed imports are limited to 64 GiB after decompression. Above a 64 MiB
floor, an expansion ratio over 500:1 is treated as a decompression bomb.
Unsupported extensions are rejected instead of guessed.

Passwords are passed through private temporary credential files (`0600` on
Unix; Windows inherits the private per-user profile ACL) or
environment-scoped password files and are never written to logs or command
arguments.

## Restore behaviour

Restore verifies:

1. the snapshot exists and belongs to this project;
2. the snapshot engine matches the project engine;
3. the snapshot engine major version matches the current engine major version;
4. the payload size and SHA-256 checksum match the metadata;
5. the decompressed SQL passes the same confinement scanner as an import.

It then creates the safety snapshot, replaces the current objects, and checks
that the database accepts a connection and returns a result. A successful
restore reports that verification. LocalWeb does not claim atomicity that the
engine cannot provide:

- PostgreSQL restore and import run under `psql --single-transaction`.
- SQLite snapshots are restored through the online backup API. SQLite SQL
  imports are not wrapped in a forced outer transaction; use a safety
  snapshot for recovery.
- MySQL and MariaDB DDL statements commit implicitly. A restore can therefore
  stop partway through and leave a partial schema; the safety snapshot is the
  recovery path.

## Concurrency, timeouts and interruption

Database operations take a per-project asynchronous lock. A long dump or
restore blocks only another database operation for the same project; the
daemon, the GUI, other projects and unrelated services keep working.
Blocking engine work runs on Tokio's blocking pool. Engine processes have a
four-hour operation budget; GUI and MCP database requests use a six-hour reply
budget. A timed-out child is killed.

Project update and removal take the same per-project database lock before the
daemon-wide operation lock, so a project cannot be reconfigured or removed
while its database is being dumped or restored. Interrupted work leaves hidden
temporary files that are cleaned by later operations; incomplete snapshots
are never listed.

Disk-full and permission errors are reported with the affected path. A failed
export removes its temporary file and leaves an existing destination intact.
A failed snapshot is not published. If an import or restore fails after data
was changed, the reported safety snapshot is the explicit recovery path.

## Desktop app

Open a project's **Details** view and use **Database Management**:

- the table lists snapshots newest first with name, creation time, size and
  engine, and marks safety snapshots;
- **Create snapshot** accepts an optional name;
- **Restore**, **Delete** and **Clean up** ask for confirmation and show the
  safety-snapshot choice for restore;
- **Export SQL** and **Import SQL** choose a destination or source file,
  infer compression from the extension, and expose replacement and
  safety-snapshot options;
- long operations show progress and keep the rest of the app responsive;
  errors are shown in the panel with the daemon's message.

The section is disabled for projects without a database. Only one database
operation runs per project at a time.

## MCP tools

The MCP server exposes database management with the same daemon behaviour:

```text
localweb_database_snapshots
localweb_database_snapshot_create
localweb_database_snapshot_restore
localweb_database_snapshot_delete
localweb_database_snapshot_cleanup
localweb_database_export
localweb_database_import
```

Restore, delete, cleanup and import require `"confirm": true`. Export never
overwrites an existing file unless `"overwrite": true` is passed. Paths must
be absolute. The tools use a six-hour reply budget so a large dump is not
mistaken for a hang.

## Requirements and troubleshooting

Snapshots, export, restore and import for network databases use the dump and
client tools bundled with the LocalWeb runtime:

- MariaDB: `mariadb-dump` (with `mysqldump` as a fallback) and `mariadb`
- MySQL: `mysqldump` and `mysql`
- PostgreSQL: `pg_dump`, `pg_restore` and `psql`

If a tool is missing, the operation fails with a runtime error before any
database changes. Reinstall the matching LocalWeb package or add the complete
engine runtime.

Common messages:

| Message | Meaning and fix |
| --- | --- |
| `a snapshot named ... already exists` | Choose another name or delete the old snapshot. |
| `failed its SHA-256 checksum` | The payload was modified or damaged. Delete it; restore another snapshot. |
| `major versions must match` | The snapshot was created by another engine major version. Use a matching runtime or export/import through SQL. |
| `refusing to import SQL at line ...` | The dump contains a statement outside the project database. Split or clean the dump before importing. |
| `suspicious compression ratio` | The archive expands too far to be a normal SQL dump. Verify its source. |
| `already exists; choose another path` | Export is protecting an existing file. Choose a new path or pass `--force`. |
