# LocalWeb architecture

LocalWeb uses native Rust processes on Ubuntu, Arch Linux, Omarchy and macOS. On Linux, managed services use transient systemd user units with a private bundled runtime; on macOS they are daemon-supervised child processes that use Homebrew-installed runtimes, and the gateway takes its port from launchd socket activation. See `docs/macos-porting.md` for the port status.

```text
 CLI ───────┐
            ├── Unix socket (0600) ── localwebd ── systemd --user
 egui GUI ──┘                              │
 Browser ── HTTP 127.0.0.1:18000-18999 ────┤
                                           ├── localweb-gateway (HTTPS, Linux default)
 Browser ── HTTPS ─────────────────────────┤        │
                                           │        ├── nginx 127.0.0.1:<project port>
                                           │        └── Apache 127.0.0.1:<project port>
                                           │                    │
                                           ├── PHP-FPM 8.x ── project Unix sockets
                                           ├── MariaDB/MySQL/PostgreSQL loopback
                                           ├── per-project DB proxies 127.0.0.1:19000-19999
                                           ├── per-project SQLite files
                                           ├── per-project Redis/Mailpit loopback
                                           └── per-project supervised workers
```

The daemon owns configuration and the desired active-project set, not continuous health monitoring of every child service. Gateway state is checked through the native service manager (systemd on Linux, `launchctl` on macOS). Clients share a user-only Unix-socket protocol. Closing the GUI disconnects only that client; **Stop LocalWeb** sends the same request as `localweb shutdown`. Setup, CA installation and host synchronization invoke the CLI/helper. On Linux only the gateway receives the narrow capability required to bind its local HTTPS port; on macOS launchd creates the gateway socket, so no root process and no capability are needed.

## Process ownership

nginx, Apache, PHP-FPM and database servers are shared. Redis, Mailpit and workers are per-project processes. They run as the desktop user through the native service backend (`crates/localweb-daemon/src/backend.rs`): transient systemd units on Linux, tracked child processes with pidfiles and log files on macOS. Workers restart after exit and stop with the project. PHP, Composer, FPM, workers and `@php` schedules use the selected private PHP configuration when present.

```text
/usr/lib/localweb/
  localwebd, localweb, localweb-gateway, localweb-helper, localweb-gui, localweb-mcp
  runtime/
    nginx/
    apache/
    php/8.5/
    composer/2/
    node/
    mariadb/
    mysql/
    postgres/
    redis/
    mailpit/

~/.config/localweb/       human-readable configuration
~/.local/share/localweb/  CA, certificates, owned persistent data
~/.local/state/localweb/  logs and operational state
$XDG_RUNTIME_DIR/localweb sockets, PIDs, generated runtime state
```

`localweb-mcp` is a Model Context Protocol server on stdio. It translates tool calls into the same daemon requests, privileged helper runs (`ca install`, `hosts sync`) and project command executions the GUI and CLI use, so an AI assistant drives the identical stack.

Inside `runtime/mariadb`, the directory layout mirrors a `/usr` basedir (`bin/`, `sbin/mariadbd`, `share/mariadb/`, `lib/plugin`) so the stock MariaDB tools work unmodified against `--basedir`.

## Startup transaction

Startup validates the selected runtime, generates the leaf certificate, checks hosts, provisions MariaDB/MySQL/PostgreSQL/SQLite, starts requested Redis/Mailpit, renders PHP-FPM and the selected web server, updates routes, starts the worker and checks HTTP startup. Failure attempts to restore the previous active configuration and stop new services.

Stop applies the remaining active configuration, then stops project services and unused shared databases. Global shutdown inventories owned project services independently of readable project configuration. A failed shared-service rewrite marks every affected project as needing recovery. Cron runs at most once per matching minute with overlap protection; project stop cancels and joins its running jobs and owned descendants. Editing cron preserves the minute history of unchanged entries, including reordered entries.

CLI and desktop edits use an optional conditional update that rejects a stale project snapshot before side effects. The original unconditional update remains supported. MCP keeps reading control messages while one tool runs; cancellation and EOF release its owned commands. A daemon request already accepted may still complete after cancellation or timeout, so clients must refresh before retrying a mutation.

Privileged integration changes share a system-wide lock. Failed Linux trust-store refreshes retain the CA anchor for a retry; trust checks require both the anchor and its compiled bundle entry. Database initialization records an in-progress marker before running the native initializer; interrupted or nonempty incomplete directories require inspection and are never deleted automatically.

Rolling downloads are published by a separate, serialized workflow after native acceptance succeeds. The publisher stages and verifies all assets, retains the previous bundle, and records recovery metadata before promotion. GitHub does not offer an atomic multi-asset swap: a brief mixed-version window remains possible during promotion; interrupted promotions are recovered from the journal before the next publication.

## Database management

Database snapshots, SQL export and SQL import are daemon requests. They take a per-project asynchronous lock instead of the daemon-wide operation lock, so a long dump or restore does not block the GUI, the daemon or another project. Blocking engine and filesystem work runs on Tokio's blocking pool, and engine child processes are killed after the four-hour operation budget.

MariaDB, MySQL and PostgreSQL snapshots are plain SQL dumps produced by `mariadb-dump`/`mysqldump` or `pg_dump`; SQLite snapshots use the SQLite online backup API. Every snapshot payload is Zstandard-compressed. Snapshots live under the platform data directory at `databases/snapshots/<project-id>/<snapshot-id>/` with `metadata.json` and one payload: `~/.local/share/localweb/` on Ubuntu, Arch/Omarchy and macOS, and `%LOCALAPPDATA%\LocalWeb\data\` on Windows. On Unix directories are `0700` and files are `0600`; on Windows the per-user `%LOCALAPPDATA%` ACL applies. A completed snapshot is published with an atomic rename, so interrupted work is never listed. Listing checks that the payload is a regular file of the recorded size; restore verifies the SHA-256 checksum.

Export and import use the same runtime engine tools and the project's generated application credentials, never the shared administrator credentials. Temporary credential files are `0600` and are removed on drop. Export streams through a private temporary file and publishes it without overwriting an existing destination unless explicitly allowed. Import reads, decompresses and validates the SQL before dropping objects or creating a safety snapshot. The SQL scanner understands quoting, comments, dollar-quoted bodies and PostgreSQL `COPY ... FROM stdin` data, and rejects cross-database, administrator, shell and file-access statements. Compressed inputs are capped at 64 GiB and at a 500:1 expansion ratio above a 64 MiB floor.

Restore additionally checks the snapshot engine and major version, creates a safety snapshot by default, and verifies the database after replacement. PostgreSQL imports use `psql --single-transaction`; MySQL and MariaDB DDL is not atomic, and SQLite SQL imports are not wrapped in a forced transaction, so the reported safety snapshot is the recovery path. Project update and removal take the same per-project database lock before the daemon-wide operation lock.

## Tool shims

`localweb setup` installs the shell integration by default: it writes `php`, `composer`, `node`, `npm` and `npx` entries into a shim directory inside the LocalWeb data directory, then publishes that directory first on the login shell `PATH` through one delimited block in the shell startup file. `localweb setup --no-shell-integration` skips this step. Windows writes `.cmd` shims that call the hidden `localweb shim <tool> --` entry point and adds the directory to the user `PATH`; no other registry value changes. The desktop Settings view reads the same state through `localweb integration status --json` and can reinstall or remove the integration. A failure to modify the shell file does not fail setup: the setup command reports it and the Settings card can retry.

`localweb` selects a project for an invocation in this order: `LOCALWEB_PROJECT` (exported by `localweb shell`), the deepest registered project whose root contains the working directory, then the system binary. Path resolution is a daemon request (`ResolveExecutionAt`), so the CLI never reads project configuration: the daemon canonicalizes both sides through their deepest existing ancestor and compares path components, which keeps symlinked working directories correct, resolves folders that do not exist yet, and never matches a sibling such as `my-project-copy` for `my-project`. The resulting `ExecutionSpec` is the same one `localweb php` uses, so `php.ini`, `LD_LIBRARY_PATH`, project environment variables and the project shim directory are applied identically. A shim keeps the working directory it was called from, so relative paths and Composer lookups stay where the command was typed, while the explicit `localweb php NAME -- ...` keeps starting at the project root.

On Unix the client replaces itself with the selected process, preserving signals, exit codes and interactive programs. The system fallback searches `PATH` while skipping the shim directory and any entry that resolves to the `localweb` binary itself, so a stale shim cannot recurse. The fallback warns on stderr only when stderr is a terminal; scripts keep their output clean.

## Packaging

Native staging records the target OS and architecture and bundles private shared libraries. CI verifies, installs, exercises and removes each native package before uploading. macOS support is implemented but still unvalidated on hardware; the implementation map, remaining risks and removal-parity checklist live in [macOS porting](docs/macos-porting.md), with the package builder in `packaging/macos/build-pkg.sh` and the uninstaller in `packaging/macos/uninstall.sh`.
