# Changelog

All notable changes to LocalWeb are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions use
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### Added

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

### Security

- Database operations use project-scoped application credentials and private
  temporary password files; passwords are never passed on command lines or
  written to logs.
- Import validation rejects statements outside the project database before
  any object is dropped or changed.
- Snapshot directories and files are private to the current account, and
  incomplete or corrupt snapshots are never listed.
