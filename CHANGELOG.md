# Changelog

All notable changes to LocalWeb are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions use
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.6] - 2026-11-18

### Added

- Project-aware shell integration installed by default: inside a registered
  project `php`, `node`, `npm`, `npx` and `composer` resolve to the project's
  runtimes, while shims fall back to system tools outside projects or when the
  daemon is stopped. Setup accepts `--no-shell-integration`, and Settings
  reports the integration state with install and remove controls.
- A UI-first README and quick start that describe day-to-day work without
  requiring terminal commands.
- Database-only snapshots for MariaDB, MySQL, PostgreSQL and SQLite projects
  on Ubuntu, Arch/Omarchy, macOS and Windows, with generated or named
  snapshots, newest-first listing, checksum and engine version verification,
  safety snapshots, restore, delete and cleanup.
- SQL export to plain, gzip or Zstandard files, with extension-based
  compression, atomic publication, no silent overwrite and `--stdout`
  streaming that keeps diagnostics on standard error.
- Confined SQL import with replacement by default, `--no-drop`, safety
  snapshots and preflight rejection of cross-database, administrator, shell,
  file-access and decompression-bomb input.
- Database Management in the project details view, with snapshot table,
  restore and import confirmations, progress and error reporting.
- `localweb db snapshot ...`, `localweb db export` and `localweb db import`
  commands with JSON output and non-interactive confirmations.
- MCP database tools for listing, creating, restoring, deleting and cleaning
  snapshots, plus export and import. Destructive calls require
  `"confirm": true`.
- Bundled runtime packaging and verification for `mariadb-dump`,
  `mysqldump`, `pg_dump` and `pg_restore`.
- Architecture and user documentation for snapshot storage, restore safety,
  import confinement, engine atomicity limits and troubleshooting.
- An optional daemon `phase` in status, so clients distinguish restoring,
  serving, stopping and stopped states.
- A recovery indicator in the project views, reporting which projects need an
  explicit cleanup stop before they can start again.

### Changed

- `localweb start` probes the daemon first and leaves an already-ready
  instance and its projects untouched; `--restart` explicitly replaces it.
- Restarting a running project performs an explicit stop followed by a
  guarded start, releasing project-owned services and shared configuration
  before replacement.
- Shared PHP-FPM, nginx, Apache and gateway units restart only when their
  rendered configuration changes, or when the unit is missing, inactive or
  unverifiable.
- Runtime resolution works while the daemon is restoring, so entering a
  project right after boot can use its PHP, Node and Composer runtimes;
  project mutations still wait for restoration.

### Fixed

- Database work is serialized with engine shutdown and project stops, so a
  long import or restore cannot race a stop.
- Startup waits for daemon progress instead of a fixed deadline; only a
  silent daemon expires the wait.
- Recovery projects require an explicit cleanup stop before Start or Restart,
  including from the GUI.
- Database and environment panels derive loading from pending requests and
  ignore stale project-scoped replies, so a superseded snapshot cannot leave
  a permanent spinner or flip another project's panel; the environment reveal
  button is disabled while its request is pending.
- A failed route write or gateway restart during service application is
  retried by maintenance with a forced gateway reload until routing catches
  up.
- Queued database operations re-read the project after acquiring their lock,
  so work queued behind a removal returns `ProjectNotFound` instead of using
  deleted configuration; successful removal also drops the project's lock
  entry.

### Security

- Database operations use project-scoped application credentials and private
  temporary password files; passwords are never passed on command lines or
  written to logs.
- Import validation rejects statements outside the project database before
  any object is dropped or changed.
- Snapshot directories and files are private to the current account, and
  incomplete or corrupt snapshots are never listed.
