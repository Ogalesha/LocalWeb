<p align="center">
  <img src="assets/brand/localweb-logo.png" alt="LocalWeb" width="420">
</p>

# LocalWeb

LocalWeb is a **native local development environment for PHP applications**.
It runs directly on your computer and starts the services a project needs:
PHP-FPM, nginx or Apache, MariaDB, MySQL, PostgreSQL or SQLite, Redis,
Mailpit, background workers and scheduled tasks. Each project gets stable
local ports and its own HTTPS address, such as `https://my-project.test`.

It does not use Docker or a virtual machine. Your application stays in the
folder where you already develop it. LocalWeb does not copy, scaffold or
rewrite it. Configuration, databases, certificates, logs and snapshots live
in your user data directory instead.

LocalWeb is **UI first**: setup, project management, certificates, database
management and shell integration are available in the desktop app. The
`localweb` command exposes the same operations for terminals, scripts and
IDEs, and the MCP server exposes them to AI assistants. The terminal is an
additional tool for developers, not a required control panel.

It is meant for WordPress, Laravel, Symfony, Drupal, another PHP framework or
CMS, or a custom PHP application. Presets are shortcuts, not a list of
supported applications. **Generic PHP** lets you choose the document root,
database, cache, worker command and scheduled tasks yourself. Your
application's requirements still decide which PHP and database versions it
can use.

## The words used in this README

- **Project** — one registered source folder plus the PHP version, web server
  and services LocalWeb starts for it. Adding a project does not move or
  modify its files.
- **Runtime** — the PHP version and supporting tools (Composer, Node.js,
  npm, database clients) used by a project. The Ubuntu and Arch packages
  bundle them; macOS and Windows use native installations described below.
- **Local domain** — a hostname such as `my-project.test` or
  `my-project.dev` that LocalWeb maps to your machine. Its HTTPS certificate
  is issued by the LocalWeb development CA.
- **Daemon** — the background process that owns projects and services. The
  desktop app, the `localweb` command and `localweb-mcp` all talk to the same
  daemon, so they see the same state and apply the same checks.
- **Shim** — a small `php`, `composer`, `node`, `npm` or `npx` entry that
  asks LocalWeb which project contains the current folder. Shims make a
  normal terminal use the project's versions instead of the system ones.
- **Shell integration** — the shims plus the single managed `PATH` block that
  LocalWeb writes to your shell startup file or user `PATH`. `localweb setup`
  installs it automatically.

### Reading the examples

Examples use `my-project` as a placeholder for the name you choose,
`~/Code/my-project` as its source folder, and `my-project.dev` or
`my-project.test` as its local domain. No project called "my-project" exists
by default: replace the name, folder and domain with your own values.

## Install

Download the latest verified build from the
[releases page](../../releases/tag/rolling):

- [Ubuntu `.deb`](../../releases/download/rolling/localweb-ubuntu-amd64.deb)
- [Arch / Omarchy `.pkg.tar.zst`](../../releases/download/rolling/localweb-arch-x86_64.pkg.tar.zst)
- [macOS `.pkg`](../../releases/download/rolling/LocalWeb-macos-universal.pkg)
- [Windows `.zip`](../../releases/download/rolling/localweb-windows-x86_64.zip)

These links always point to the latest successful `main` build for all four
platforms. The release notes list its version, source commit and checksums.
Builds are published after testing, so a push may take time to appear there.
[Stable releases](../../releases/latest) are separate snapshots.

```bash
sudo apt install ./localweb-ubuntu-amd64.deb           # Ubuntu
sudo pacman -U ./localweb-arch-x86_64.pkg.tar.zst       # Arch / Omarchy
```

On macOS, open the `.pkg`. On Windows, extract the `.zip` and run
`localweb-gui.exe` or `localweb.exe` from the extracted folder.

| Platform | Package | Native integration |
| --- | --- | --- |
| Ubuntu 26.04 amd64 | `.deb` | systemd, system certificate store and system authorization |
| Arch Linux amd64 | `.pkg.tar.zst` | systemd, Arch certificate store and system authorization |
| Omarchy amd64 | Arch package | Uses the same Arch base and integration |
| macOS 12+ arm64/x86_64 | `.pkg` (universal) | launchd agents and system keychain trust for the development CA |
| Windows 10/11 x86_64 | `.zip` (portable) | Named pipes, scheduled task and user certificate store |

Ubuntu and Arch packages include LocalWeb's private web stack. The macOS
installer and Windows ZIP contain the application binaries; install the
native tools separately through Homebrew or Scoop/Chocolatey. Check
`localweb doctor` before starting a project. A normal desktop session with the
platform service manager (systemd, launchd or Task Scheduler) is required.

## First run: from install to a working site

### 1. Set up LocalWeb

Open the desktop app and choose **Set up LocalWeb**, then **Trust
certificate**. The same workflow from a terminal is:

```bash
localweb setup
localweb ca install
localweb start
```

Running `localweb start` again leaves a healthy daemon and its projects
untouched. Use `localweb start --restart` after an upgrade when you
explicitly want to replace the running process.

Setup creates the user configuration, database, certificate and log
directories, generates the development CA, registers the paths with the
privileged helper and installs the user service units. It also installs the
**shell integration** described below, so a normal terminal will use each
project's PHP, Composer, Node.js and npm versions. The platform may ask for
authorization while setup registers paths with the privileged helper;
trusting the CA and changing `/etc/hosts` use the same normal authorization
prompt.

If you do not want setup to touch your shell startup file or user `PATH`:

```bash
localweb setup --no-shell-integration
```

You can install, repair or remove the integration later from
**Settings → Shell integration** in the desktop app, or with
`localweb integration install|status|remove`.

Restart any browser that was already open during the first certificate
installation so it reloads its trust store.

### 2. Add a project

In the desktop app choose **Add project**, select an existing application
folder, check the detected preset and document root, choose the PHP version,
web server and services, then confirm with **Add project**.

The folder must already contain the application: LocalWeb does not download
framework code or create a new application for you. If the folder is empty,
create the application first with Composer or your framework's installer.
The source files stay where they are; LocalWeb stores only its own
configuration under the user data directory.

From a terminal the equivalent is:

```bash
localweb project add ~/Code/my-project --php 8.4 --web nginx \
  --db mariadb --cache redis --document-root public --domain my-project.test
```

### 3. Connect the application

Open **Details → Show connection settings** in the desktop app and copy the
database, Redis and email values into the application's own configuration
(`.env`, `wp-config.php`, and so on). **Copy as .env** copies the same values.
LocalWeb does not edit your application's configuration file.

Set the application's base URL to the LocalWeb HTTPS address shown in
**Details**, for example `https://my-project.test`. From the CLI:

```bash
localweb env my-project --format dotenv
```

LocalWeb leaves source files and existing databases in place; it does not
import data from another development stack. The Ubuntu and Arch packages
bundle `phpredis` in every PHP runtime, so Laravel's default
`REDIS_CLIENT=phpredis` works there. On macOS and Windows, enable the PHP
`redis` extension provided with your PHP installation, or use
`predis/predis`. Copy `REDIS_HOST` and `REDIS_PORT` from
**Show connection settings** into `.env`; LocalWeb passes the same values to
CLI commands and workers.

### 4. Start and open

Select **Start** on the project row, then **Open**. The same commands are:

```bash
localweb project start my-project
```

Start prints the direct URL (`http://127.0.0.1:<web port>`). **Open** in the
desktop app uses the HTTPS local domain when the gateway is ready and the
direct URL otherwise. Stopping a project stops its services but keeps the
project and its data. Removing a project deletes LocalWeb's configuration for
it, not the source folder or the database.

## Use the project's tools in a terminal

This is the behaviour most developers expect: enter a project folder and run
the tools directly, without a `localweb` prefix.

After `localweb setup` (or the desktop setup), **open a new terminal** and run:

```bash
cd ~/Code/my-project

php -v            # the PHP version configured for my-project, not the system one
composer install  # Composer running with that same PHP
node -v           # the Node.js version configured for my-project
npm run build     # npm running against that Node.js
npx vite --version

cd ~
php -v            # the PHP installed by your operating system
```

For example, if the project is configured for PHP 8.4 and the machine also
has PHP 8.5, `php -v` inside the project reports **8.4**. The same applies to
`node`, `npm`, `npx`, `composer` and the tools they launch.

### Why it works

`localweb setup` installs five shims in the LocalWeb data directory
(`~/.local/share/localweb/bin` on Ubuntu, Arch/Omarchy and macOS,
`%LOCALAPPDATA%\LocalWeb\data\bin` on Windows). It then publishes that
directory on your login `PATH`:

- On Unix, it adds one delimited block to the startup file of your login shell
  (`~/.zshrc`, `~/.bashrc`, `~/.bash_profile`, `~/.profile` or
  `~/.config/fish/config.fish`). Nothing outside that block is edited.
- On Windows, it adds one entry to the user `PATH` and writes `.cmd` shims.
  No other registry value changes.

When you run `php`, the shim asks the daemon which registered project contains
the current folder. The deepest matching project wins, so a project nested
inside another one owns its own subtree. The daemon returns the same
`ExecutionSpec` used by `localweb php`: the selected PHP binary, its
`php.ini`, `LD_LIBRARY_PATH`, project environment variables and the project
shim directory. A shim keeps the working directory it was called from, so
relative paths and Composer lookups stay where you typed them.

Resolution order is deliberate:

1. `LOCALWEB_PROJECT`, which `localweb shell` exports.
2. The deepest registered project that contains the current folder.
3. The tool installed on your system.

Outside every project, or while the daemon is not running, the shim runs the
system tool instead of failing. When the daemon cannot answer, the fallback
prints a short notice on stderr, and only if stderr is a terminal, so scripts,
IDE tasks and pipelines stay quiet. An explicit `LOCALWEB_PROJECT`, which is
how `localweb shell` marks its sessions, stays strict instead: a stopped
daemon is reported rather than hidden.

### Check, install or remove it from the UI

Open **Settings → Shell integration** in the desktop app. The card shows:

- whether the five shims are installed;
- the shim folder;
- the shell startup file (or the user `PATH` on Windows);
- whether the app's environment already sees the shim folder on `PATH`;
- **Install**, **Reinstall** and **Remove integration** actions.

Installing from the app is safe and idempotent: the managed block is replaced
in place, not appended again. After installing, open a new terminal (or
`source` the startup file the card shows) so the current shell reloads its
`PATH`. On Windows, a new terminal is required after the user `PATH` changes.

The equivalent CLI commands are:

```bash
localweb integration status
localweb integration install
localweb integration remove
```

`localweb integration status --json` prints the same machine-readable state
used by the desktop card.

On Windows, a PHP installation listed in the **machine-wide** `PATH` takes
precedence over the per-user LocalWeb entry, so remove it from that list if
you want the LocalWeb shims to win.

### If you do not want shell integration

Use a project shell for the current session:

```bash
localweb shell my-project
npm run build
```

Or use the explicit forms, which work without shims and are useful in
scripts, IDE tasks and CI:

```bash
localweb php my-project -- -v
localweb composer my-project -- install
localweb node my-project -- --version
localweb npm my-project -- run build
localweb npx my-project -- --version
localweb exec my-project -- php artisan about
```

To bypass the shims for a single command in an already configured shell:

```bash
LOCALWEB_NO_SHIM=1 php -v
```

Setup accepts `--no-shell-integration` when it runs. Removing the integration
from **Settings → Shell integration** or with `localweb integration remove`
deletes the shims and the managed block; project runtimes remain available
through the explicit commands and the desktop app.

## What the desktop app can do

The desktop app is the primary interface for the daily workflow. The terminal
column shows the equivalent for scripting; the rows marked `—` are advanced
operations that currently remain terminal-only.

| Task | Desktop app | Terminal |
| --- | --- | --- |
| Initial setup | **Set up LocalWeb** | `localweb setup` |
| Trust the HTTPS certificate | **Trust certificate** | `localweb ca install` |
| Add a project | **Add project** | `localweb project add <path>` |
| Read project details | **Details** | `localweb project info <name>` |
| Edit runtime and services | **Edit** | `localweb project update <name>` |
| Start, stop or restart | project row actions | `localweb project start\|stop\|restart <name>` |
| Open the site or Mailpit | **Open** / **Mail** | use the URL from **Details** |
| Copy connection values | **Show connection settings** | `localweb env <name>` |
| Database snapshots, restore, export, import | **Details → Database management** | `localweb db ...` |
| Sync local domains | **Settings → Sync local domains** | `localweb hosts sync` |
| Repair setup | **Settings → Repair setup** | `localweb setup` |
| Shell integration status/install/remove | **Settings → Shell integration** | `localweb integration ...` |
| AI assistant configuration | **Settings → AI assistants** | `localweb-mcp` |
| Stop LocalWeb | **Stop LocalWeb** | `localweb shutdown` |
| Start a project when LocalWeb starts | project form checkbox | `localweb project update <name> --autostart` |
| Start LocalWeb at login | — | `localweb autostart enable\|disable` |
| Diagnostics | — | `localweb doctor` |

Closing the desktop window keeps LocalWeb running in the system tray on
Linux, macOS and Windows. Clicking the tray icon shows **Open** and **Quit
desktop app**: **Open** restores the window, while **Quit desktop app**
closes the desktop app only. The daemon and running projects keep working, so
**Stop LocalWeb** remains the way to stop the managed environment; **Stop**
on a project affects that project. On a Linux desktop without a
StatusNotifier watcher, closing the window exits normally instead of hiding
an unreachable app.

The displayed project state is the daemon's active-project state, not a
continuous health check of every service. When a project starts or restarts,
the daemon reads a bounded HTTP/1.x status line from its direct loopback port:
2xx/3xx and authentication challenges (401/403) count as reachable, while
404, 5xx, malformed responses and timeouts fail the start. An HTTP 500 means
the app returned an error: check its logs and the values from
`localweb env <name>`. Gateway state is checked through the native service
manager. Worker counts describe configured workers.

## Project management

Every project exposes the same four operations in the desktop app and in the
CLI:

| Operation | Desktop | CLI |
| --- | --- | --- |
| Create | **Add project** | `localweb project add <path>` |
| Read | **Details** on the project row | `localweb project info <name>` |
| Update | **Edit** on the project row | `localweb project update <name>` |
| Delete | **Remove** on the project row | `localweb project remove <name>` |

The row actions are always visible, including in the narrow stacked layout.
**Edit** opens the full settings form; **Remove** asks for confirmation.
Deleting an active project stops it first, then removes its LocalWeb
configuration, local domain and project-owned runtime artifacts. The source
folder and database data are always retained.

`localweb project update` changes only the flags you pass. Name, path,
hostname, project identifier, environment variables, resource limits and
creation time are preserved. The same applies to the desktop edit form.
For example:

```bash
localweb project update my-project --php 8.4 --web apache
localweb project update my-project --db postgres --cache redis --mail
localweb project update my-project --no-mail --no-autostart
localweb project update my-project --clear-cron
localweb project update my-project --cron '*/10 * * * * @php artisan schedule:run'
```

`--supervisor` enables the profile worker preset, `--no-supervisor` disables
the worker, and `--supervisor-command` replaces it. Without one of these flags
the current worker configuration is kept.

## Runtimes and local domains

| Tool | What LocalWeb provides |
| --- | --- |
| **PHP 8.0–8.5** | Per-project CLI and PHP-FPM selection. PHP 8.0 and 8.1 are end-of-life compatibility versions and receive no upstream security fixes; 8.2 is in security-only support until 31 Dec 2026. Use a supported branch for current work. The desktop indicates which versions are included. PHP 8.6 will be added after its stable release. Bundled Linux runtimes include the `redis` (phpredis) extension, and Laravel projects may use `predis/predis` instead. |
| **nginx** | Per-project web configuration for a LEMP stack. |
| **Apache 2.4** | Per-project web configuration for a LAMP stack; can run alongside nginx. |
| **MariaDB** | Shared local server with a database and credentials for each project; the reference payload uses MariaDB 11.8 LTS. |
| **MySQL** | Shared local server with a database and credentials for each project; the reference payload uses MySQL 8.4 LTS. |
| **PostgreSQL** | Private local cluster with a database and login for each project. |
| **SQLite** | A LocalWeb-owned database file for each project, accessed through PHP. |
| **Redis** | A separate local cache process for each project. |
| **Composer 2** | Runs with the project's selected PHP. |
| **Node.js 24, npm and npx** | Bundled LTS build tools. Node.js 22 and 20 can also be selected when matching system installations are available; Node.js 20 is EOL. |
| **Mailpit** | Per-project SMTP server and browser inbox for development email. |
| **Background workers** | Supervisor-style process management using the native service manager; Laravel queue and Symfony Messenger presets, or a custom executable and arguments. |
| **Scheduled tasks** | Five-field cron schedules, framework presets, logs and overlap protection while the project is active. |

Every project gets a fixed loopback HTTP port (18000–18999) and, for network
databases, a fixed loopback database port (19000–19999) forwarded to the
shared engine. Pin them with `--web-port` and `--db-port`, or let LocalWeb
allocate them from those ranges. `localweb project info <name>` shows both
endpoints, and the environment values expose the database proxy port as
`DB_PORT`.

The HTTPS gateway runs locally as part of LocalWeb and is managed by the
daemon. It prefers port 443 on its own loopback address; when that is not
available it uses a high port and shows the resulting URL, so it never stops
or rewrites an unrelated service. `localweb status` shows the addresses and
ports in use. `localweb ca install` adds the public development CA to the
trust stores used by the current platform. Restart browsers that were open
during the first installation.

Local domains are exact project entries in `/etc/hosts`, with `.dev` as the
default suffix. **Settings → Sync local domains** repairs them from the
desktop app; `localweb hosts sync` is the CLI equivalent. LocalWeb only
changes the entries it owns.

The Linux build pipeline assembles all six PHP CLI/FPM pairs from the pinned
sources in `packaging/php-sources.txt`. A smaller development package may
include only PHP 8.5. Selecting another version does not download it: its
matching CLI and FPM must already be in the runtime or on the system.
PostgreSQL likewise needs its complete runtime payload and PHP's
`pdo_pgsql` extension. **Memcached is not implemented.**

See [architecture](ARCHITECTURE.md) for the runtime layout.

## Background work

| Preset | Worker | Scheduled task |
| --- | --- | --- |
| Laravel | `artisan queue:work` | `artisan schedule:run` every minute |
| Symfony | `bin/console messenger:consume async` | Supply a schedule if your app needs one |
| WordPress | Custom command if needed | `wp-cron.php` every five minutes; disable WordPress's page-triggered cron in your app configuration |
| Drupal | Custom command if needed | `vendor/bin/drush cron` every fifteen minutes; install Drush in the project |
| Other apps | Custom executable and arguments | Custom schedule and command |

Enable a preset with `--supervisor` or `--cron-preset`. A custom worker uses
`--supervisor-command` and one `--supervisor-arg` per literal argument.
`@php` uses the project's selected PHP and private PHP configuration. The
desktop exposes the same presets and custom-command fields.

## Database snapshots and SQL files

Every project with a database has database-only snapshots, restore, SQL
export and SQL import. Open **Details → Database management** in the desktop
app, or use the same operations from the CLI:

```bash
localweb db snapshot create my-project --name before-migration
localweb db snapshot list my-project
localweb db snapshot restore my-project --latest --yes
localweb db snapshot restore my-project before-migration --no-safety --yes
localweb db snapshot delete my-project before-migration --yes
localweb db snapshot cleanup my-project --yes

localweb db export my-project ~/Backups/my-project.sql.zst
localweb db export my-project --stdout --compression gzip > my-project.sql.gz
localweb db import my-project ~/Backups/my-project.sql.zst --yes
localweb db import my-project ~/Backups/partial.sql --no-drop --yes
```

Snapshots contain database state only. They never include project files, PHP
or web-server configuration, Redis data, Mailpit messages or certificates.
MariaDB, MySQL and PostgreSQL snapshots are logical SQL dumps; SQLite
snapshots use the online backup API. All payloads are Zstandard-compressed,
checksummed and published atomically under the platform data directory
(`~/.local/share/localweb/databases/snapshots/` on Ubuntu, Arch/Omarchy and
macOS, `%LOCALAPPDATA%\LocalWeb\data\databases\snapshots\` on Windows).

Restore verifies the checksum, engine and major version before changing data,
creates a safety snapshot first by default, and verifies the database after
the restore. `--latest` selects the newest normal snapshot; safety snapshots
must be named explicitly. Export supports `.sql`, `.sql.gz` and `.sql.zst`,
never replaces an existing file unless `--force` is passed, and `--stdout`
writes only data to standard output. Import replaces existing objects by
default, supports `--no-drop` and a safety snapshot, and validates the whole
dump before modifying anything.

Imports are confined to the project database with project-scoped
credentials. Cross-database and administrator commands, client shell
commands, path traversal and decompression bombs are rejected before any
change. MySQL and MariaDB DDL is not atomic, so keep the default safety
snapshot where rollback matters. PostgreSQL restore and import run inside a
single transaction. See
[database management](docs/database-management.md) for the full rules,
storage layout, engine behaviour and troubleshooting.

## Common commands

```bash
localweb status
localweb doctor
localweb runtime list
localweb project list
localweb project info my-project
localweb project update my-project --php 8.4
localweb project restart my-project
localweb project stop my-project
localweb project remove my-project
localweb node my-project -- --version
localweb npm my-project -- run build
localweb npx my-project -- --version
localweb exec my-project -- php artisan about
localweb shell my-project
localweb db snapshot create my-project --name before-migration
localweb db export my-project ~/Backups/my-project.sql.zst
localweb autostart enable
localweb integration status
localweb integration install
localweb integration remove
localweb shutdown
```

## AI assistants (MCP)

The package installs `localweb-mcp`, a Model Context Protocol server that
exposes the LocalWeb daemon over stdio. Point any MCP-capable client at the
binary to let an assistant create a project, start it, run `composer`,
framework commands or tests inside the project runtime, read logs and open
the environment values. The tool set covers status and diagnostics, daemon
start/stop, project add/remove/start/stop/restart, `@php`/`@composer`/`@node`/
`@npm`/`@npx` command execution, environment, runtime list, certificate
status/installation, hosts synchronization, login autostart, and database
snapshots, restore, export and import. Destructive database tools require an
explicit `"confirm": true`.

The desktop **Settings → AI assistants** view shows the resolved
`localweb-mcp` path and copies the configuration below. The
**Environment** card next to it shows the domain defaults and configuration
folder.

```json
{
  "mcpServers": {
    "localweb": {
      "command": "localweb-mcp"
    }
  }
}
```

The assistant connects to the same user daemon as the desktop app; start
LocalWeb once (`localweb start`) or use the `localweb_daemon_start` tool.
Privileged steps (CA installation, `/etc/hosts` changes) still require the
normal polkit authentication.

## Troubleshooting

Run `localweb doctor` for a checklist of setup, certificate, gateway, local
domain and project-file problems. Common checks:

- **`php -v` shows the system version inside a project.** Open
  **Settings → Shell integration** and check that the status is **Active**,
  then open a new terminal. `which php` must point to the LocalWeb shim
  folder shown in the card. On Windows, remove any machine-wide PHP entry
  that appears before the user `PATH`.
- **The daemon is not reachable.** Start it from the desktop app or with
  `localweb start`. Outside projects, the shims fall back to system tools
  while it is stopped.
- **HTTPS is not trusted.** Run **Trust certificate** in the desktop app or
  `localweb ca install`, then restart the browser.
- **A project start reports HTTP 500.** The web server is reachable but the
  application failed. Check its logs and the values copied from **Details →
  Show connection settings**.
- **A local domain does not resolve.** Use **Settings → Sync local domains**
  or `localweb hosts sync`. The system asks for authorization to edit
  `/etc/hosts`.

## Validation status

The earlier Ubuntu/PHP 8.5 package has passed live stack and removal tests.
The expanded stack and platform ports are checked by
[Native packages and acceptance](../../actions/workflows/native-builds.yml).
For the source revision you are installing, check that its platform job
passed. Arch CI covers the base used by Omarchy; an Omarchy desktop session
still needs its own final acceptance check. macOS jobs build and smoke the
universal binaries on a macOS runner, and the Windows binaries are
cross-built and exercised under Wine; a final acceptance run on real macOS
and Windows hosts is still pending.

The Linux jobs build and install the package, test PHP 8.0–8.5 with both web
servers and all four database choices, check Redis, Mailpit and the PHP CLI,
then exercise stop, removal and source-folder preservation. Packages are
uploaded as workflow artifacts only after the job passes. macOS and Windows
also run the workspace regression suite on native runners; Windows runs core
and daemon regressions under Wine too. These checks do not replace native
web-stack acceptance on macOS and Windows. Tagged versions and the rolling
pre-release publish the same files as public release assets.

## Remove LocalWeb

```bash
sudo apt remove localweb     # Ubuntu
sudo pacman -R localweb      # Arch / Omarchy
```

Package removal stops LocalWeb's services and removes its system integration,
binaries, logs and host mappings. **Per-user data is preserved:** projects,
databases, settings, generated certificates and the private CA keys are never
deleted by `apt remove` or `pacman -R`, and upgrades keep them as well.

Setup installs the shell integration by default. Before removing the package,
remove it from **Settings → Shell integration** in the desktop app, or run:

```bash
localweb integration remove
```

This deletes the shims and the managed block from your shell startup file (or
the user `PATH` entry on Windows). To erase this account's LocalWeb data
explicitly:

```bash
localweb ca remove              # remove this account's CA from system trust
localweb data purge --dry-run   # list directories and sizes
localweb data purge             # requires typing "purge" to confirm
```

`localweb data purge` only removes directories owned by the calling account
and refuses symlinked paths. Source project folders outside LocalWeb data
directories are never touched.

## Build

Use stable Rust. The runtime staging tools need Python 3 on the **build
host**; the installed application does not use Python, Electron or a browser
shell.

```bash
cargo build --release --workspace
cargo test --workspace --all-targets
```

Packaging scripts are under `packaging/` and `scripts/`. See
[architecture](ARCHITECTURE.md) and [security](SECURITY.md) for the runtime
layout and security model.
