use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use localweb_core::{
    Cache, Database, ExecutionSpec, NewProject, Paths, Profile, Project, Request, Response,
    SupervisorConfig, WebServer,
    certificate::CaPaths,
    copy,
    hosts::{HostsSync, HostsSyncLock},
    send_request,
};
use std::{
    ffi::OsString,
    io::{BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// How long `localweb start` waits for project restoration before reporting an error.
const STARTUP_READY_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Parser)]
#[command(name = "localweb", version, about = "Run local web projects")]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[expect(
    clippy::large_enum_variant,
    reason = "clap subcommands carry every project option in one place"
)]
#[derive(Subcommand)]
enum CommandKind {
    #[command(about = "Set up LocalWeb")]
    Setup,
    #[command(about = "Start LocalWeb")]
    Start,
    #[command(about = "Stop LocalWeb and running projects")]
    Shutdown,
    #[command(about = "Show LocalWeb status")]
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Check setup and suggest fixes.
    Doctor,
    /// Run the project's PHP.
    Php {
        name: String,
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Run Composer with the project's PHP.
    Composer {
        name: String,
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Run the project's Node.js.
    Node {
        name: String,
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Run the project's npm.
    Npm {
        name: String,
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Run the project's npx.
    Npx {
        name: String,
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Run a command in a project.
    Exec {
        name: String,
        #[arg(last = true, required = true)]
        command: Vec<OsString>,
    },
    /// Open a project shell.
    Shell { name: String },
    /// Show project connection settings.
    Env {
        name: String,
        #[arg(long, value_enum, default_value_t = EnvFormat::Human)]
        format: EnvFormat,
    },
    /// Manage HTTPS certificates.
    Ca {
        #[command(subcommand)]
        command: CaCommand,
    },
    /// Sync local domains.
    Hosts {
        #[command(subcommand)]
        command: HostsCommand,
    },
    /// Add and manage projects.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    #[command(about = "Start LocalWeb at login")]
    Autostart {
        #[command(subcommand)]
        command: Toggle,
    },
    /// List available runtimes.
    Runtime {
        #[command(subcommand)]
        command: RuntimeCommand,
    },
    #[command(about = "Manage this account's LocalWeb data")]
    Data {
        #[command(subcommand)]
        command: DataCommand,
    },
    /// Route php, composer, node, npm and npx to the current project.
    Integration {
        #[command(subcommand)]
        command: IntegrationCommand,
    },
    /// Internal entry point used by the generated Windows shims.
    #[command(hide = true)]
    Shim {
        tool: String,
        #[arg(last = true)]
        args: Vec<OsString>,
    },
}

#[derive(Subcommand, Clone, Copy)]
enum IntegrationCommand {
    #[command(about = "Put the LocalWeb tool shims on this account's PATH")]
    Install,
    #[command(about = "Remove the shims and the managed shell block")]
    Remove,
    #[command(about = "Report whether the shims are active")]
    Status,
}

#[derive(Subcommand)]
enum DataCommand {
    #[command(about = "Permanently delete this account's LocalWeb data")]
    Purge {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum Toggle {
    Enable,
    Disable,
}

#[derive(Subcommand)]
enum CaCommand {
    /// Check HTTPS certificate trust.
    Status,
    #[command(about = "Trust the LocalWeb HTTPS certificate")]
    Install,
    /// Remove HTTPS certificate trust.
    Remove,
}

#[derive(Subcommand)]
enum HostsCommand {
    Sync,
}

#[derive(Subcommand)]
enum RuntimeCommand {
    List,
}

#[derive(Subcommand)]
enum ProjectCommand {
    /// List projects and their status.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Add an existing source folder.
    #[command(visible_alias = "create")]
    Add {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long = "domain")]
        hostname: Option<String>,
        #[arg(long = "document-root")]
        document_root: Option<PathBuf>,
        #[arg(long = "web")]
        web_server: Option<WebArg>,
        /// PHP version for both CLI and FPM.
        #[arg(long, value_parser = localweb_core::PHP_VERSIONS)]
        php: Option<String>,
        /// Node.js version. Includes 24; 20/22 need a matching installation.
        #[arg(long, value_parser = ["20", "22", "24"])]
        node: Option<String>,
        #[arg(long = "db")]
        database: Option<DbArg>,
        #[arg(long)]
        cache: Option<CacheArg>,
        /// Composer version (2 only).
        #[arg(long, value_parser = ["2"])]
        composer: Option<String>,
        /// Run the Laravel/Symfony worker preset. Other apps need a custom command.
        #[arg(long)]
        supervisor: bool,
        /// Worker executable. Use @php for project PHP.
        #[arg(long = "supervisor-command")]
        supervisor_command: Option<String>,
        /// One worker argument. Repeat for more.
        #[arg(
            long = "supervisor-arg",
            allow_hyphen_values = true,
            requires = "supervisor_command"
        )]
        supervisor_args: Vec<String>,
        /// One scheduled task (`"*/5 * * * * @php artisan schedule:run"`);
        /// repeat for multiple tasks. Runs while the project is active.
        #[arg(long = "cron")]
        cron: Vec<String>,
        /// Use the app preset when --cron is omitted.
        #[arg(long = "cron-preset")]
        cron_preset: bool,
        /// Capture test email with Mailpit.
        #[arg(long)]
        mail: bool,
        /// Local HTTP port. Assigned automatically if omitted.
        #[arg(long = "web-port", value_parser = clap::value_parser!(u16).range(1024..))]
        web_port: Option<u16>,
        /// Local database port. Assigned automatically if omitted.
        #[arg(long = "db-port", value_parser = clap::value_parser!(u16).range(1024..))]
        database_port: Option<u16>,
        #[arg(long, help = "Start this project when LocalWeb starts")]
        autostart: bool,
    },
    /// Show project settings and status.
    #[command(visible_alias = "show")]
    Info {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Change project settings.
    #[command(visible_alias = "edit")]
    Update {
        name: String,
        #[arg(long = "document-root")]
        document_root: Option<PathBuf>,
        #[arg(long)]
        profile: Option<ProfileArg>,
        #[arg(long = "web")]
        web_server: Option<WebArg>,
        /// PHP version for both CLI and FPM.
        #[arg(long, value_parser = localweb_core::PHP_VERSIONS)]
        php: Option<String>,
        /// Node.js version. Includes 24; 20/22 need a matching installation.
        #[arg(long, value_parser = ["20", "22", "24"])]
        node: Option<String>,
        #[arg(long = "db")]
        database: Option<DbArg>,
        #[arg(long)]
        cache: Option<CacheArg>,
        /// Composer version (2 only).
        #[arg(long, value_parser = ["2"])]
        composer: Option<String>,
        /// Enable the detected Laravel or Symfony worker preset.
        #[arg(long, conflicts_with = "no_supervisor")]
        supervisor: bool,
        /// Disable the project worker.
        #[arg(long = "no-supervisor", conflicts_with = "supervisor")]
        no_supervisor: bool,
        /// Replace the worker executable; implies --supervisor.
        #[arg(long = "supervisor-command", conflicts_with = "no_supervisor")]
        supervisor_command: Option<String>,
        /// One worker argument. Repeat for more.
        #[arg(
            long = "supervisor-arg",
            allow_hyphen_values = true,
            requires = "supervisor_command"
        )]
        supervisor_args: Vec<String>,
        /// Replace the enabled scheduled tasks; repeat for multiple tasks.
        #[arg(long = "cron", conflicts_with = "clear_cron")]
        cron: Vec<String>,
        /// Use the app preset when --cron is omitted.
        #[arg(long = "cron-preset", conflicts_with = "clear_cron")]
        cron_preset: bool,
        /// Remove every scheduled task, including disabled ones.
        #[arg(long = "clear-cron", conflicts_with_all = ["cron", "cron_preset"])]
        clear_cron: bool,
        /// Capture test email with Mailpit.
        #[arg(long, conflicts_with = "no_mail")]
        mail: bool,
        /// Disable Mailpit for this project.
        #[arg(long = "no-mail", conflicts_with = "mail")]
        no_mail: bool,
        #[arg(
            long,
            conflicts_with = "no_autostart",
            help = "Start the project when LocalWeb starts"
        )]
        autostart: bool,
        #[arg(
            long = "no-autostart",
            conflicts_with = "autostart",
            help = "Keep the project stopped when LocalWeb starts"
        )]
        no_autostart: bool,
        #[arg(long)]
        json: bool,
    },
    /// Start a project.
    Start { name: String },
    /// Stop a project.
    Stop { name: String },
    /// Restart a project.
    Restart { name: String },
    /// Remove a project. Keep source files and databases.
    #[command(visible_alias = "delete")]
    Remove { name: String },
}

#[derive(Clone, Debug, Default)]
enum SupervisorUpdate {
    #[default]
    Keep,
    Enable,
    Disable,
    Command(Vec<String>),
}

#[derive(Default)]
struct UpdateOptions {
    document_root: Option<PathBuf>,
    profile: Option<Profile>,
    web_server: Option<WebServer>,
    php: Option<String>,
    node: Option<String>,
    database: Option<Database>,
    cache: Option<Cache>,
    composer: Option<String>,
    supervisor: SupervisorUpdate,
    cron: Option<Vec<localweb_core::CronEntry>>,
    cron_preset: bool,
    clear_cron: bool,
    mail: Option<bool>,
    autostart: Option<bool>,
}

/// Merges the flags supplied to `project update` with the existing project.
///
/// `Project::from_update` receives a complete input so unset flags keep their
/// current values instead of falling back to global defaults.
fn update_input(existing: &Project, options: UpdateOptions) -> NewProject {
    let supervisor = match options.supervisor {
        SupervisorUpdate::Keep => existing.supervisor.clone(),
        SupervisorUpdate::Enable => SupervisorConfig {
            enabled: true,
            command: existing.supervisor.command.clone(),
        },
        SupervisorUpdate::Disable => SupervisorConfig {
            enabled: false,
            command: Vec::new(),
        },
        SupervisorUpdate::Command(command) => SupervisorConfig {
            enabled: true,
            command,
        },
    };
    let cron = options.cron.unwrap_or_else(|| {
        existing
            .cron
            .iter()
            .filter(|entry| entry.enabled)
            .cloned()
            .collect()
    });
    NewProject {
        path: existing.path.clone(),
        name: Some(existing.name.clone()),
        hostname: Some(existing.hostname.clone()),
        web_port: existing.web_port,
        database_port: existing.database_port,
        document_root: options
            .document_root
            .or_else(|| Some(existing.document_root.clone())),
        profile: options.profile.or(Some(existing.profile)),
        web_server: options.web_server.or(Some(existing.web_server)),
        php: options.php.or_else(|| Some(existing.php.clone())),
        composer: options.composer.or_else(|| Some(existing.composer.clone())),
        node: options.node.or_else(|| Some(existing.node.clone())),
        database: options.database.or(Some(existing.database)),
        cache: options.cache.or(Some(existing.cache)),
        supervisor,
        cron,
        cron_preset: options.cron_preset,
        clear_cron: options.clear_cron,
        mail: options.mail.unwrap_or(existing.mail),
        autostart: options.autostart.unwrap_or(existing.autostart),
    }
}

async fn project_status(paths: &Paths, name: &str) -> Result<localweb_core::ProjectStatus> {
    let response = send_request(
        paths,
        Request::ProjectGet {
            name: name.to_owned(),
        },
    )
    .await
    .context("Can't reach LocalWeb. Run `localweb start`.")?;
    match response {
        Response::Project(status) => Ok(status),
        Response::Error { code, message } => bail!("{code}: {message}"),
        _ => bail!(copy::UNEXPECTED_RESPONSE),
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum ProfileArg {
    #[value(name = "genericphp", alias = "generic-php")]
    GenericPhp,
    Laravel,
    Symfony,
    Drupal,
    Wordpress,
    #[value(name = "processwire", alias = "process-wire")]
    ProcessWire,
}
impl From<ProfileArg> for Profile {
    fn from(value: ProfileArg) -> Self {
        match value {
            ProfileArg::GenericPhp => Self::GenericPhp,
            ProfileArg::Laravel => Self::Laravel,
            ProfileArg::Symfony => Self::Symfony,
            ProfileArg::Drupal => Self::Drupal,
            ProfileArg::Wordpress => Self::Wordpress,
            ProfileArg::ProcessWire => Self::ProcessWire,
        }
    }
}

#[derive(Clone, ValueEnum)]
enum WebArg {
    Nginx,
    Apache,
}
#[derive(Clone, ValueEnum)]
enum DbArg {
    None,
    Mariadb,
    Mysql,
    Postgres,
    Sqlite,
}
#[derive(Clone, ValueEnum)]
enum CacheArg {
    None,
    Redis,
}

#[derive(Clone, Copy, ValueEnum)]
enum EnvFormat {
    Human,
    Dotenv,
}
impl From<WebArg> for WebServer {
    fn from(value: WebArg) -> Self {
        match value {
            WebArg::Nginx => Self::Nginx,
            WebArg::Apache => Self::Apache,
        }
    }
}
impl From<DbArg> for Database {
    fn from(value: DbArg) -> Self {
        match value {
            DbArg::None => Self::None,
            DbArg::Mariadb => Self::Mariadb,
            DbArg::Mysql => Self::Mysql,
            DbArg::Postgres => Self::Postgres,
            DbArg::Sqlite => Self::Sqlite,
        }
    }
}
impl From<CacheArg> for Cache {
    fn from(value: CacheArg) -> Self {
        match value {
            CacheArg::None => Self::None,
            CacheArg::Redis => Self::Redis,
        }
    }
}

#[tokio::main]
#[expect(
    clippy::too_many_lines,
    reason = "CLI dispatch mirrors the public command tree"
)]
async fn main() -> Result<()> {
    if let Some(tool) = shim_tool() {
        let paths = Paths::discover().ok();
        return run_tool_shim(paths.as_ref(), &tool, std::env::args_os().skip(1).collect()).await;
    }
    let cli = Cli::parse();
    let paths = Paths::discover()?;
    let mut json = false;
    let mut sync_after = false;
    let mut env_format = None;
    let request = match cli.command {
        CommandKind::Setup => {
            setup(&paths)?;
            return Ok(());
        }
        CommandKind::Integration { command } => {
            integration(&paths, command)?;
            return Ok(());
        }
        CommandKind::Shim { tool, args } => {
            return run_tool_shim(Some(&paths), &tool, args).await;
        }
        CommandKind::Ca {
            command: CaCommand::Install,
        } => {
            install_ca(&paths)?;
            return Ok(());
        }
        CommandKind::Ca {
            command: CaCommand::Remove,
        } => {
            remove_ca(&paths)?;
            return Ok(());
        }
        CommandKind::Hosts {
            command: HostsCommand::Sync,
        } => {
            sync_hosts(&paths).await?;
            return Ok(());
        }
        CommandKind::Php { name, args } => {
            return execute_project(&paths, &name, ExecutionKind::Php(args)).await;
        }
        CommandKind::Composer { name, args } => {
            return execute_project(&paths, &name, ExecutionKind::Composer(args)).await;
        }
        CommandKind::Node { name, args } => {
            return execute_project(&paths, &name, ExecutionKind::Node(args)).await;
        }
        CommandKind::Npm { name, args } => {
            return execute_project(&paths, &name, ExecutionKind::Npm(args)).await;
        }
        CommandKind::Npx { name, args } => {
            return execute_project(&paths, &name, ExecutionKind::Npx(args)).await;
        }
        CommandKind::Exec { name, command } => {
            return execute_project(&paths, &name, ExecutionKind::Exec(command)).await;
        }
        CommandKind::Shell { name } => {
            return execute_project(&paths, &name, ExecutionKind::Shell).await;
        }
        CommandKind::Env { name, format } => {
            env_format = Some(format);
            Request::ProjectEnvironment { name }
        }
        CommandKind::Start => {
            start_daemon(&paths).await?;
            return Ok(());
        }
        CommandKind::Autostart { command } => {
            let (enable, action) = match command {
                Toggle::Enable => (true, "enable"),
                Toggle::Disable => (false, "disable"),
            };
            localweb_core::service::set_autostart(enable)?;
            println!("LocalWeb login autostart {action}d");
            return Ok(());
        }
        CommandKind::Shutdown => Request::Shutdown,
        CommandKind::Status { json: enabled } => {
            json = enabled;
            Request::Status
        }
        CommandKind::Doctor => Request::Doctor,
        CommandKind::Runtime {
            command: RuntimeCommand::List,
        } => Request::RuntimeList,
        CommandKind::Data {
            command: DataCommand::Purge { dry_run, yes },
        } => {
            data_purge(&paths, dry_run, yes).await?;
            return Ok(());
        }
        CommandKind::Ca {
            command: CaCommand::Status,
        } => Request::CaStatus,
        CommandKind::Project { command } => match command {
            ProjectCommand::List { json: enabled } => {
                json = enabled;
                Request::ProjectList
            }
            ProjectCommand::Add {
                path,
                name,
                hostname,
                document_root,
                web_server,
                php,
                node,
                database,
                cache,
                composer,
                supervisor,
                supervisor_command,
                supervisor_args,
                cron,
                cron_preset,
                mail,
                web_port,
                database_port,
                autostart,
            } => {
                sync_after = true;
                let supervisor_enabled = supervisor || supervisor_command.is_some();
                let mut supervisor_command = supervisor_command.into_iter().collect::<Vec<_>>();
                supervisor_command.extend(supervisor_args);
                let mut cron_entries = Vec::new();
                for line in cron {
                    cron_entries.push(localweb_core::parse_cron_line(&line)?);
                }
                Request::ProjectAdd(NewProject {
                    path,
                    name,
                    hostname,
                    document_root,
                    profile: None,
                    web_server: web_server.map(Into::into),
                    php,
                    node,
                    composer,
                    database: database.map(Into::into),
                    cache: cache.map(Into::into),
                    supervisor: SupervisorConfig {
                        enabled: supervisor_enabled,
                        command: supervisor_command,
                    },
                    cron: cron_entries,
                    cron_preset,
                    clear_cron: false,
                    mail,
                    web_port,
                    database_port,
                    autostart,
                })
            }
            ProjectCommand::Info {
                name,
                json: enabled,
            } => {
                json = enabled;
                Request::ProjectGet { name }
            }
            ProjectCommand::Update {
                name,
                document_root,
                profile,
                web_server,
                php,
                node,
                database,
                cache,
                composer,
                supervisor,
                no_supervisor,
                supervisor_command,
                supervisor_args,
                cron,
                cron_preset,
                clear_cron,
                mail,
                no_mail,
                autostart,
                no_autostart,
                json: enabled,
            } => {
                json = enabled;
                sync_after = true;
                let existing = project_status(&paths, &name).await?;
                let cron = if clear_cron || (cron_preset && cron.is_empty()) {
                    Some(Vec::new())
                } else if cron.is_empty() {
                    None
                } else {
                    let mut entries = Vec::new();
                    for line in &cron {
                        entries.push(localweb_core::parse_cron_line(line)?);
                    }
                    Some(entries)
                };
                let supervisor = if let Some(command) = supervisor_command {
                    let mut command = vec![command];
                    command.extend(supervisor_args);
                    SupervisorUpdate::Command(command)
                } else if no_supervisor {
                    SupervisorUpdate::Disable
                } else if supervisor {
                    SupervisorUpdate::Enable
                } else {
                    SupervisorUpdate::Keep
                };
                let options = UpdateOptions {
                    document_root,
                    profile: profile.map(Into::into),
                    web_server: web_server.map(Into::into),
                    php,
                    node,
                    database: database.map(Into::into),
                    cache: cache.map(Into::into),
                    composer,
                    supervisor,
                    cron,
                    cron_preset,
                    clear_cron,
                    mail: if mail {
                        Some(true)
                    } else if no_mail {
                        Some(false)
                    } else {
                        None
                    },
                    autostart: if autostart {
                        Some(true)
                    } else if no_autostart {
                        Some(false)
                    } else {
                        None
                    },
                };
                Request::ProjectUpdateIfCurrent {
                    name,
                    input: update_input(&existing.project, options),
                    expected: Box::new(existing.project),
                }
            }
            ProjectCommand::Start { name } => Request::ProjectStart { name },
            ProjectCommand::Stop { name } => Request::ProjectStop { name },
            ProjectCommand::Restart { name } => Request::ProjectRestart { name },
            ProjectCommand::Remove { name } => {
                sync_after = true;
                Request::ProjectRemove { name }
            }
        },
    };
    let response = send_request(&paths, request)
        .await
        .context("Can't reach LocalWeb. Run `localweb start`.")?;
    if let Response::Error { code, message } = &response {
        bail!("{code}: {message}");
    }
    if sync_after {
        sync_hosts(&paths)
            .await
            .context("Project saved, but domain sync failed. Run `localweb hosts sync`.")?;
    }
    match response {
        other if json => println!("{}", serde_json::to_string_pretty(&other)?),
        Response::Status(status) => {
            let gateway = if status.gateway_running {
                match (status.gateway_bind_address, status.gateway_https_port) {
                    (Some(address), Some(443)) => format!("running on https://{address}"),
                    (Some(address), Some(port)) => {
                        format!("running on https://{address}:{port}")
                    }
                    _ => "running".to_owned(),
                }
            } else {
                "stopped".to_owned()
            };
            println!(
                "LocalWeb: running\nHTTPS gateway: {gateway}\nProjects: {} total, {} running",
                status.project_count, status.active_projects
            );
        }
        Response::Projects(projects) => {
            for item in projects {
                println!(
                    "{:<20} {:<30} {:<8} {}",
                    item.project.name,
                    item.project.hostname,
                    if item.running { "running" } else { "stopped" },
                    item.project.path.display()
                );
            }
        }
        Response::Project(item) => {
            let direct_url = item
                .project
                .web_port
                .map_or_else(|| "-".to_owned(), |port| format!("http://127.0.0.1:{port}"));
            println!(
                "{} ({})\nSource folder: {}\nDocument root: {}\nPHP: {}\nWeb server: {:?}\nHTTPS URL: {}\nHTTP URL: {}\nDatabase: {:?}\nDatabase address: {}\nCache: {:?}\nWorker: {}\nStatus: {}",
                item.project.name,
                item.project.hostname,
                item.project.path.display(),
                item.project.document_root.display(),
                item.project.php,
                item.project.web_server,
                item.public_url.as_deref().unwrap_or("gateway stopped"),
                direct_url,
                item.project.database,
                item.project
                    .database_port
                    .map_or_else(|| "-".to_owned(), |port| format!("127.0.0.1:{port}")),
                item.project.cache,
                if item.project.supervisor.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                if item.running { "running" } else { "stopped" }
            );
        }
        Response::Message(message) => println!("{message}"),
        Response::Ca(status) => println!(
            "HTTPS certificate: {}\nSystem trust: {}\nBrowser trust: {}\nCertificate file: {}",
            if status.exists {
                "available"
            } else {
                "missing"
            },
            if status.system_trusted {
                "trusted"
            } else {
                "not trusted"
            },
            if status.browser_trusted {
                "trusted"
            } else {
                "not trusted"
            },
            status.certificate.display()
        ),
        Response::Execution(_) => bail!("unexpected execution response"),
        Response::Environment(values) => {
            for (key, value) in values {
                match env_format.unwrap_or(EnvFormat::Human) {
                    EnvFormat::Human => println!("{key}: {value}"),
                    EnvFormat::Dotenv => println!("{key}={value}"),
                }
            }
        }
        Response::Error { .. } => unreachable!("errors were handled before rendering"),
        Response::Diagnostics(items) => {
            for item in items {
                println!("{:<7} {:<20} {}", item.level, item.check, item.detail);
                if let Some(fix) = item.fix {
                    println!("        fix: {fix}");
                }
            }
        }
    }
    Ok(())
}

async fn start_daemon(paths: &Paths) -> Result<()> {
    // Restart so a stale daemon left behind by a package upgrade is replaced.
    localweb_core::service::restart_daemon()?;
    wait_for_daemon(paths).await?;
    println!("{}", copy::LOCALWEB_STARTED);
    Ok(())
}

async fn wait_for_daemon(paths: &Paths) -> Result<()> {
    let deadline = tokio::time::Instant::now() + STARTUP_READY_TIMEOUT;
    loop {
        let error = match tokio::time::timeout(
            Duration::from_secs(2),
            send_request(paths, Request::Status),
        )
        .await
        {
            Ok(Ok(Response::Status(status))) if status.ready => return Ok(()),
            Ok(Ok(Response::Status(_))) => "LocalWeb is still restoring projects".to_owned(),
            Ok(Ok(response)) => format!("unexpected daemon response: {response:?}"),
            Ok(Err(error)) => error.to_string(),
            Err(_) => copy::NO_RESPONSE.to_owned(),
        };
        if tokio::time::Instant::now() >= deadline {
            bail!("LocalWeb didn't start: {error}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn setup(paths: &Paths) -> Result<()> {
    paths.create()?;
    let _ = localweb_core::GlobalConfig::load_or_create(paths)?;
    CaPaths::new(paths).ensure_ca()?;
    register_paths(paths)?;
    localweb_core::service::write_user_units(paths)?;
    if cfg!(target_os = "macos") {
        println!(
            "Setup complete. Run `localweb ca install`, then `localweb start`. Login autostart is on; turn it off with `localweb autostart disable`."
        );
    } else {
        println!(
            "Setup complete. Run `localweb ca install`, then `localweb start`. Login autostart is off."
        );
    }
    Ok(())
}

fn register_paths(paths: &Paths) -> Result<()> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "config": paths.config,
        "data": paths.data,
        "state": paths.state,
        "runtime": paths.runtime,
    }))?;
    run_helper("register-paths", &payload)
}

enum ExecutionKind {
    Php(Vec<OsString>),
    Composer(Vec<OsString>),
    Node(Vec<OsString>),
    Npm(Vec<OsString>),
    Npx(Vec<OsString>),
    Exec(Vec<OsString>),
    Shell,
}

async fn execute_project(paths: &Paths, name: &str, kind: ExecutionKind) -> Result<()> {
    let spec = resolve_execution(paths, name).await?;
    let resolved = if spec.project_name.is_empty() {
        name
    } else {
        &spec.project_name
    };
    run_project_command(&spec, resolved, kind)
}

/// Resolves the runtime of a named project, failing when the daemon is away.
async fn resolve_execution(paths: &Paths, name: &str) -> Result<ExecutionSpec> {
    let response = send_request(paths, Request::ResolveExecution { name: name.into() })
        .await
        .context("Can't load the project runtime. Run `localweb start`.")?;
    match response {
        Response::Execution(spec) => Ok(spec),
        Response::Error { code, message } => bail!("{code}: {message}"),
        _ => bail!(copy::UNEXPECTED_RESPONSE),
    }
}

/// Runs a shim tool for an explicit `LOCALWEB_PROJECT`, as `localweb shell` sets it.
async fn execute_shim_project(paths: &Paths, name: &str, kind: ExecutionKind) -> Result<()> {
    let spec = resolve_execution(paths, name).await?;
    let resolved = if spec.project_name.is_empty() {
        name.to_owned()
    } else {
        spec.project_name.clone()
    };
    run_shim_command(&spec, &resolved, kind)
}

fn run_project_command(spec: &ExecutionSpec, name: &str, kind: ExecutionKind) -> Result<()> {
    execute_command(build_execution(spec, name, kind)?)
}

/// Runs a shim tool where the caller stands, not at the project root.
///
/// `localweb php NAME -- ...` intentionally starts at the project root; a shim
/// has to behave like the tool it replaces, so relative paths and Composer
/// lookups keep working in the folder the person typed the command in.
fn run_shim_command(spec: &ExecutionSpec, name: &str, kind: ExecutionKind) -> Result<()> {
    let mut command = build_execution(spec, name, kind)?;
    if let Ok(cwd) = std::env::current_dir() {
        command.current_dir(cwd);
    }
    execute_command(command)
}

fn build_execution(spec: &ExecutionSpec, name: &str, kind: ExecutionKind) -> Result<Command> {
    let mut command = match kind {
        ExecutionKind::Php(args) => {
            let mut cmd = Command::new(&spec.php);
            if let Some(ini) = &spec.php_ini {
                cmd.arg("-n").arg("-c").arg(ini);
            }
            cmd.args(args);
            cmd
        }
        ExecutionKind::Composer(args) => {
            let phar = spec
                .composer_phar
                .as_ref()
                .context("Composer 2 is not installed in LocalWeb runtime storage")?;
            let mut cmd = Command::new(&spec.php);
            if let Some(ini) = &spec.php_ini {
                cmd.arg("-n").arg("-c").arg(ini);
            }
            cmd.arg(phar).args(args);
            cmd
        }
        ExecutionKind::Node(args) => {
            let node = spec
                .node
                .as_ref()
                .context("Node.js is not available for this project")?;
            let mut cmd = Command::new(node);
            cmd.args(args);
            cmd
        }
        ExecutionKind::Npm(args) => {
            let node = spec
                .node
                .as_ref()
                .context("Node.js is not available for this project")?;
            let npm = spec
                .npm_cli
                .as_ref()
                .context("npm is not available for this project")?;
            let mut cmd = Command::new(node);
            cmd.arg(npm).args(args);
            cmd
        }
        ExecutionKind::Npx(args) => {
            let node = spec
                .node
                .as_ref()
                .context("Node.js is not available for this project")?;
            let npm = spec
                .npm_cli
                .as_ref()
                .context("npm is not available for this project")?;
            let npx = npm.with_file_name("npx-cli.js");
            if !npx.is_file() {
                bail!("npx is not available for this project");
            }
            let mut cmd = Command::new(node);
            cmd.arg(npx).args(args);
            cmd
        }
        ExecutionKind::Exec(args) => {
            let mut args = args.into_iter();
            let program = args.next().context("missing command after --")?;
            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd
        }
        ExecutionKind::Shell => {
            #[cfg(windows)]
            let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| OsString::from("cmd.exe"));
            #[cfg(not(windows))]
            let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
            Command::new(shell)
        }
    };
    spec.configure_command(&mut command, name)?;
    Ok(command)
}

/// Replaces the current process with `command` so exit codes, signals and
/// interactive programs behave exactly as if the tool ran directly.
#[cfg(unix)]
fn execute_command(mut command: Command) -> Result<()> {
    use std::os::unix::process::CommandExt as _;
    let error = command.exec();
    Err(error).context("failed to launch the command")
}

#[cfg(windows)]
fn execute_command(mut command: Command) -> Result<()> {
    let status = command.status().context("failed to launch the command")?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Tools routed to the current project while the shim directory is on `PATH`.
const SHIM_TOOLS: [&str; 5] = ["php", "composer", "node", "npm", "npx"];

/// Opens the block `localweb integration` owns in a shell startup file.
#[cfg(unix)]
const SHELL_BLOCK_BEGIN: &str = "# BEGIN LOCALWEB SHELL";
/// Closes the block `localweb integration` owns in a shell startup file.
#[cfg(unix)]
const SHELL_BLOCK_END: &str = "# END LOCALWEB SHELL";

/// Returns the tool name when `localweb` was started through a shim link.
fn shim_tool() -> Option<String> {
    shim_tool_name(std::env::args_os().next())
}

fn shim_tool_name(argv0: Option<OsString>) -> Option<String> {
    let name = PathBuf::from(argv0?).file_name()?.to_str()?.to_owned();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    SHIM_TOOLS.contains(&name).then(|| name.to_owned())
}

/// `LOCALWEB_NO_SHIM=1` keeps the shim on PATH but always runs the system tool.
fn shim_disabled() -> bool {
    std::env::var_os("LOCALWEB_NO_SHIM")
        .is_some_and(|value| !matches!(value.to_str(), Some("" | "0")))
}

/// Runs a tool from the project that owns the working directory.
async fn run_tool_shim(paths: Option<&Paths>, tool: &str, args: Vec<OsString>) -> Result<()> {
    let Some(paths) = paths else {
        return run_system_tool(None, tool, args);
    };
    if shim_disabled() {
        return run_system_tool(Some(paths), tool, args);
    }
    let kind = execution_kind(tool, args.clone())?;
    if let Some(name) = std::env::var("LOCALWEB_PROJECT")
        .ok()
        .filter(|name| !name.is_empty())
    {
        return execute_shim_project(paths, &name, kind).await;
    }
    let cwd = std::env::current_dir().context("cannot determine the current directory")?;
    match send_request(paths, Request::ResolveExecutionAt { path: cwd }).await {
        Ok(Response::Execution(spec)) => {
            let name = spec.project_name.clone();
            if name.is_empty() {
                bail!("{}", copy::UNEXPECTED_RESPONSE);
            }
            run_shim_command(&spec, &name, kind)
        }
        Ok(Response::Error { code, .. }) if code == "ProjectNotFound" => {
            run_system_tool(Some(paths), tool, args)
        }
        Ok(Response::Error { code, message }) => bail!("{code}: {message}"),
        Ok(_) => bail!("{}", copy::UNEXPECTED_RESPONSE),
        Err(_) => {
            warn_fallback(Some(paths), tool);
            run_system_tool(Some(paths), tool, args)
        }
    }
}

fn execution_kind(tool: &str, args: Vec<OsString>) -> Result<ExecutionKind> {
    Ok(match tool {
        "php" => ExecutionKind::Php(args),
        "composer" => ExecutionKind::Composer(args),
        "node" => ExecutionKind::Node(args),
        "npm" => ExecutionKind::Npm(args),
        "npx" => ExecutionKind::Npx(args),
        other => bail!("unknown tool shim: {other}"),
    })
}

/// Explains the system fallback once, and only when a person is watching.
fn warn_fallback(paths: Option<&Paths>, tool: &str) {
    if !std::io::stderr().is_terminal() {
        return;
    }
    let target = system_tool(paths, tool).map_or_else(
        |_| format!("the system {tool}"),
        |path| path.display().to_string(),
    );
    eprintln!("LocalWeb: the daemon did not answer, so {tool} uses {target}.");
    eprintln!("LocalWeb: run `localweb start` to use project runtimes here.");
}

fn run_system_tool(paths: Option<&Paths>, tool: &str, args: Vec<OsString>) -> Result<()> {
    let program = system_tool(paths, tool)?;
    let mut command = Command::new(program);
    command.args(args);
    execute_command(command)
}

/// Finds the tool outside the shim directory, so the fallback never recurses.
fn system_tool(paths: Option<&Paths>, tool: &str) -> Result<PathBuf> {
    let shim = paths.map(|paths| {
        let dir = integration_dir(paths);
        dir.canonicalize().unwrap_or(dir)
    });
    let search = std::env::var_os("PATH").context("PATH is unset")?;
    for dir in std::env::split_paths(&search) {
        let dir = dir.canonicalize().unwrap_or(dir);
        if shim.as_ref() == Some(&dir) {
            continue;
        }
        for name in tool_candidates(tool) {
            let candidate = dir.join(name);
            if is_runnable(&candidate) && !is_localweb_binary(&candidate) {
                return Ok(candidate);
            }
        }
    }
    bail!("{tool} is not installed on PATH outside LocalWeb projects")
}

/// Guards against a shim found outside the integration directory.
fn is_localweb_binary(candidate: &Path) -> bool {
    let Ok(target) = candidate.canonicalize() else {
        return false;
    };
    std::env::current_exe()
        .is_ok_and(|executable| executable.canonicalize().is_ok_and(|exe| exe == target))
}

fn tool_candidates(tool: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![
            format!("{tool}.exe"),
            format!("{tool}.cmd"),
            format!("{tool}.bat"),
        ]
    } else {
        vec![tool.to_owned()]
    }
}

#[cfg(unix)]
fn is_runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_runnable(path: &Path) -> bool {
    path.is_file()
}

fn integration(paths: &Paths, command: IntegrationCommand) -> Result<()> {
    match command {
        IntegrationCommand::Install => install_integration(paths),
        IntegrationCommand::Remove => remove_integration(paths),
        IntegrationCommand::Status => integration_status(paths),
    }
}

/// Directory whose entries make `php`, `composer` and Node tools follow the project.
fn integration_dir(paths: &Paths) -> PathBuf {
    paths.data.join("bin")
}

fn install_integration(paths: &Paths) -> Result<()> {
    paths.create()?;
    let dir = install_shims(paths)?;
    println!("LocalWeb shims are ready in {}", dir.display());
    configure_shell(&dir)?;
    println!("Run `php -v` inside a project folder to use its PHP.");
    Ok(())
}

#[cfg(unix)]
fn configure_shell(dir: &Path) -> Result<()> {
    let (startup, fish) = shell_startup_file()?;
    let changed = write_managed_block(&startup, &shell_block(dir, fish))?;
    println!(
        "{} {}",
        if changed {
            "Updated shell startup file"
        } else {
            "Shell startup file already configured:"
        },
        startup.display()
    );
    println!("Open a new terminal, or run: source {}", startup.display());
    Ok(())
}

#[cfg(windows)]
fn configure_shell(dir: &Path) -> Result<()> {
    if install_path_entry(dir)? {
        println!(
            "Added {} to the user PATH; open a new terminal.",
            dir.display()
        );
    } else {
        println!("{} is already on the user PATH.", dir.display());
    }
    Ok(())
}

fn remove_integration(paths: &Paths) -> Result<()> {
    let mut changed = remove_shims(paths)?;
    #[cfg(unix)]
    for startup in remove_managed_blocks()? {
        println!("Cleaned shell startup file {}", startup.display());
        changed = true;
    }
    #[cfg(windows)]
    if remove_path_entry_from_environment(&integration_dir(paths))? {
        println!("Removed the LocalWeb entry from the user PATH.");
        changed = true;
    }
    if changed {
        println!("Shell integration removed.");
    } else {
        println!("Shell integration is not installed.");
    }
    Ok(())
}

fn integration_status(paths: &Paths) -> Result<()> {
    let dir = integration_dir(paths);
    let missing = SHIM_TOOLS
        .iter()
        .filter(|tool| !shim_installed(&dir, tool))
        .copied()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        println!("Shims: installed in {}", dir.display());
    } else {
        println!("Shims: missing {}", missing.join(", "));
    }
    let on_path = std::env::var_os("PATH")
        .is_some_and(|value| std::env::split_paths(&value).any(|entry| same_dir(&entry, &dir)));
    println!("PATH: {}", if on_path { "active" } else { "not active" });
    let configured = report_shell_status(&dir)?;
    if missing.is_empty() && configured && on_path {
        println!("Run `php -v` inside a project folder to use its PHP.");
    } else if missing.is_empty() && configured {
        println!("Open a new terminal to activate the shims.");
    } else {
        println!("Run `localweb integration install` to set it up.");
    }
    Ok(())
}

#[cfg(unix)]
fn report_shell_status(_dir: &Path) -> Result<bool> {
    let (startup, _) = shell_startup_file()?;
    let configured = std::fs::read_to_string(&startup).is_ok_and(|text| has_managed_block(&text));
    println!(
        "Shell startup file: {} ({})",
        startup.display(),
        if configured {
            "configured"
        } else {
            "not configured"
        }
    );
    Ok(configured)
}

#[cfg(windows)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "mirrors the Unix signature so the caller stays platform-neutral"
)]
fn report_shell_status(dir: &Path) -> Result<bool> {
    let configured = read_user_path()
        .is_ok_and(|value| env_paths(&value).any(|entry| same_dir(Path::new(&entry), dir)));
    println!(
        "User PATH: {}",
        if configured {
            "configured"
        } else {
            "not configured"
        }
    );
    Ok(configured)
}

fn same_dir(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(unix)]
fn install_shims(paths: &Paths) -> Result<PathBuf> {
    use std::os::unix::fs::symlink;
    let dir = integration_dir(paths);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let localweb = std::env::current_exe().context("cannot locate the localweb executable")?;
    let localweb = localweb.canonicalize().unwrap_or(localweb);
    for tool in SHIM_TOOLS {
        let link = dir.join(tool);
        match std::fs::symlink_metadata(&link) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                std::fs::remove_file(&link)
                    .with_context(|| format!("cannot replace {}", link.display()))?;
            }
            Ok(_) => bail!(
                "{} already exists and is not a LocalWeb shim; move it away first",
                link.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("cannot inspect {}", link.display()));
            }
        }
        symlink(&localweb, &link).with_context(|| format!("cannot create {}", link.display()))?;
    }
    Ok(dir)
}

#[cfg(windows)]
fn install_shims(paths: &Paths) -> Result<PathBuf> {
    let dir = integration_dir(paths);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let localweb = std::env::current_exe().context("cannot locate the localweb executable")?;
    for tool in SHIM_TOOLS {
        let script = dir.join(format!("{tool}.cmd"));
        if let Ok(existing) = std::fs::read_to_string(&script)
            && !existing.contains(SHIM_MARKER)
        {
            bail!(
                "{} already exists and is not a LocalWeb shim; move it away first",
                script.display()
            );
        }
        let body = format!(
            "@echo off\r\nrem {SHIM_MARKER}\r\n\"{}\" shim {tool} -- %*\r\nexit /b %errorlevel%\r\n",
            localweb.display()
        );
        std::fs::write(&script, body)
            .with_context(|| format!("cannot write {}", script.display()))?;
    }
    Ok(dir)
}

#[cfg(unix)]
fn remove_shims(paths: &Paths) -> Result<bool> {
    let dir = integration_dir(paths);
    let mut removed = false;
    for tool in SHIM_TOOLS {
        let link = dir.join(tool);
        if std::fs::symlink_metadata(&link).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            std::fs::remove_file(&link)
                .with_context(|| format!("cannot remove {}", link.display()))?;
            removed = true;
        }
    }
    if removed {
        let _ = std::fs::remove_dir(&dir);
    }
    Ok(removed)
}

#[cfg(windows)]
fn remove_shims(paths: &Paths) -> Result<bool> {
    let dir = integration_dir(paths);
    let mut removed = false;
    for tool in SHIM_TOOLS {
        let script = dir.join(format!("{tool}.cmd"));
        if std::fs::read_to_string(&script).is_ok_and(|text| text.contains(SHIM_MARKER)) {
            std::fs::remove_file(&script)
                .with_context(|| format!("cannot remove {}", script.display()))?;
            removed = true;
        }
    }
    if removed {
        let _ = std::fs::remove_dir(&dir);
    }
    Ok(removed)
}

#[cfg(unix)]
fn shim_installed(dir: &Path, tool: &str) -> bool {
    std::fs::symlink_metadata(dir.join(tool))
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
}

#[cfg(windows)]
fn shim_installed(dir: &Path, tool: &str) -> bool {
    std::fs::read_to_string(dir.join(format!("{tool}.cmd")))
        .is_ok_and(|text| text.contains(SHIM_MARKER))
}

#[cfg(unix)]
fn shell_startup_file() -> Result<(PathBuf, bool)> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is unset")?;
    let shell = std::env::var("SHELL").unwrap_or_default();
    let shell = PathBuf::from(shell);
    let name = shell
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let (path, fish) = match name {
        "zsh" => (home.join(".zshrc"), false),
        "bash" => {
            let profile = home.join(".bash_profile");
            if !home.join(".bashrc").exists() && profile.exists() {
                (profile, false)
            } else {
                (home.join(".bashrc"), false)
            }
        }
        "fish" => (home.join(".config").join("fish").join("config.fish"), true),
        _ => (home.join(".profile"), false),
    };
    Ok((path, fish))
}

#[cfg(unix)]
fn shell_startup_candidates() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    [
        home.join(".zshrc"),
        home.join(".bashrc"),
        home.join(".bash_profile"),
        home.join(".profile"),
        home.join(".config").join("fish").join("config.fish"),
    ]
    .into_iter()
    .filter(|path| path.is_file())
    .collect()
}

#[cfg(unix)]
fn shell_block(dir: &Path, fish: bool) -> String {
    let value = escape_shell_path(&dir.display().to_string());
    if fish {
        format!(
            "{SHELL_BLOCK_BEGIN}\nset -l localweb_bin \"{value}\"\nif not contains -- $localweb_bin $PATH\n    set -gx PATH $localweb_bin $PATH\nend\n{SHELL_BLOCK_END}\n"
        )
    } else {
        format!(
            "{SHELL_BLOCK_BEGIN}\nlocalweb_bin=\"{value}\"\ncase \":$PATH:\" in\n  *\":$localweb_bin:\"*) ;;\n  *) PATH=\"$localweb_bin:$PATH\"; export PATH ;;\nesac\nunset localweb_bin\n{SHELL_BLOCK_END}\n"
        )
    }
}

#[cfg(unix)]
fn escape_shell_path(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

#[cfg(unix)]
fn has_managed_block(text: &str) -> bool {
    match (text.find(SHELL_BLOCK_BEGIN), text.find(SHELL_BLOCK_END)) {
        (Some(begin), Some(end)) => begin < end,
        _ => false,
    }
}

#[cfg(unix)]
fn strip_managed_block(text: &str) -> String {
    if !has_managed_block(text) {
        return text.to_owned();
    }
    let mut output = String::new();
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        let marker = line.trim_end_matches(['\r', '\n']).trim();
        if marker == SHELL_BLOCK_BEGIN {
            inside = true;
            continue;
        }
        if marker == SHELL_BLOCK_END {
            inside = false;
            continue;
        }
        if !inside {
            output.push_str(line);
        }
    }
    output
}

#[cfg(unix)]
fn write_managed_block(startup: &Path, block: &str) -> Result<bool> {
    let existing = match std::fs::read_to_string(startup) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", startup.display()));
        }
    };
    let mut updated = existing
        .as_deref()
        .map_or_else(String::new, strip_managed_block);
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(block);
    if existing.as_deref() == Some(updated.as_str()) {
        return Ok(false);
    }
    if let Some(parent) = startup.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(startup, updated)
        .with_context(|| format!("cannot write {}", startup.display()))?;
    Ok(true)
}

#[cfg(unix)]
fn remove_managed_blocks() -> Result<Vec<PathBuf>> {
    let mut cleaned = Vec::new();
    for startup in shell_startup_candidates() {
        let Ok(text) = std::fs::read_to_string(&startup) else {
            continue;
        };
        if !has_managed_block(&text) {
            continue;
        }
        std::fs::write(&startup, strip_managed_block(&text))
            .with_context(|| format!("cannot write {}", startup.display()))?;
        cleaned.push(startup);
    }
    Ok(cleaned)
}

#[cfg(windows)]
const SHIM_MARKER: &str = "LocalWeb shell integration";

/// Splits a `reg query` line into the `REG_*` value it carries.
#[cfg(any(windows, test))]
fn parse_reg_value(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.trim();
        let (name, rest) = line.split_once(char::is_whitespace)?;
        if !name.eq_ignore_ascii_case("PATH") {
            return None;
        }
        let (kind, value) = rest.trim_start().split_once(char::is_whitespace)?;
        kind.starts_with("REG_")
            .then(|| value.trim_start().to_owned())
    })
}

/// Puts `entry` first, keeping the rest of the user PATH intact.
#[cfg(any(windows, test))]
fn add_path_entry(current: &str, entry: &str) -> String {
    let mut parts = vec![entry.to_owned()];
    parts.extend(
        env_paths(current)
            .filter(|part| !part.eq_ignore_ascii_case(entry))
            .map(str::to_owned),
    );
    parts.join(";")
}

/// Drops `entry` from the user PATH.
#[cfg(any(windows, test))]
fn remove_path_entry(current: &str, entry: &str) -> String {
    env_paths(current)
        .filter(|part| !part.eq_ignore_ascii_case(entry))
        .collect::<Vec<_>>()
        .join(";")
}

#[cfg(any(windows, test))]
fn env_paths(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
}

#[cfg(windows)]
fn read_user_path() -> Result<String> {
    let output = Command::new("reg")
        .args(["query", "HKCU\\Environment", "/v", "PATH"])
        .output()
        .context("cannot read the user PATH")?;
    if !output.status.success() {
        return Ok(String::new());
    }
    Ok(parse_reg_value(&String::from_utf8_lossy(&output.stdout)).unwrap_or_default())
}

#[cfg(windows)]
fn write_user_path(value: &str) -> Result<()> {
    let mut command = Command::new("reg");
    if value.is_empty() {
        command.args(["delete", "HKCU\\Environment", "/v", "PATH", "/f"]);
    } else {
        command.args([
            "add",
            "HKCU\\Environment",
            "/v",
            "PATH",
            "/t",
            "REG_EXPAND_SZ",
            "/d",
            value,
            "/f",
        ]);
    }
    let status = command.status().context("cannot update the user PATH")?;
    if !status.success() {
        bail!("cannot update the user PATH");
    }
    Ok(())
}

#[cfg(windows)]
fn install_path_entry(dir: &Path) -> Result<bool> {
    let current = read_user_path()?;
    let entry = dir.display().to_string();
    if env_paths(&current).any(|part| part.eq_ignore_ascii_case(&entry)) {
        return Ok(false);
    }
    write_user_path(&add_path_entry(&current, &entry))?;
    Ok(true)
}

#[cfg(windows)]
fn remove_path_entry_from_environment(dir: &Path) -> Result<bool> {
    let current = read_user_path()?;
    let entry = dir.display().to_string();
    if !env_paths(&current).any(|part| part.eq_ignore_ascii_case(&entry)) {
        return Ok(false);
    }
    write_user_path(&remove_path_entry(&current, &entry))?;
    Ok(true)
}

fn helper_path() -> Result<PathBuf> {
    #[cfg(unix)]
    {
        let installed = PathBuf::from("/usr/lib/localweb/localweb-helper");
        if installed.is_file() {
            return Ok(installed);
        }
    }
    let development = std::env::current_exe()?
        .with_file_name(format!("localweb-helper{}", std::env::consts::EXE_SUFFIX));
    if development.is_file() {
        return Ok(development);
    }
    bail!("localweb-helper is missing; install the LocalWeb package")
}

fn run_helper(operation: &str, input: &[u8]) -> Result<()> {
    let helper = helper_path()?;
    Ok(localweb_core::privilege::run_helper(
        &helper, operation, input,
    )?)
}

async fn data_purge(paths: &Paths, dry_run: bool, yes: bool) -> Result<()> {
    if localweb_core::send_request(paths, Request::Status)
        .await
        .is_ok()
    {
        bail!("LocalWeb is running. Run `localweb shutdown` before purging data.");
    }
    #[cfg(unix)]
    if localweb_core::current_uid() == 0 {
        bail!("run `localweb data purge` as the account that owns the data, not as root");
    }
    if CaPaths::new(paths).cert.is_file() {
        println!("Run `localweb ca remove` before purging to revoke HTTPS trust.");
    }
    let targets = [
        ("configuration", paths.config.clone()),
        ("data", paths.data.clone()),
        ("state", paths.state.clone()),
        ("runtime", paths.runtime.clone()),
    ];
    #[cfg(unix)]
    let current_uid = localweb_core::current_uid();
    let mut existing = Vec::new();
    for (label, path) in targets {
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("cannot inspect {}", path.display()));
            }
        };
        if metadata.file_type().is_symlink() {
            bail!(
                "refusing to purge {} because it is a symbolic link",
                path.display()
            );
        }
        if !metadata.is_dir() {
            bail!(
                "refusing to purge {} because it is not a directory",
                path.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.uid() != current_uid {
                bail!(
                    "refusing to purge {} because it is owned by uid {}, not by the current user",
                    path.display(),
                    metadata.uid()
                );
            }
        }
        let size = directory_size(&path);
        existing.push((label, path, size));
    }
    if existing.is_empty() {
        println!("No LocalWeb data found for this account.");
        return Ok(());
    }
    println!("LocalWeb data for this account:");
    for (label, path, size) in &existing {
        println!("  {label:14} {:>10}  {}", human_size(*size), path.display());
    }
    if dry_run {
        println!("Dry run. No files deleted.");
        return Ok(());
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("refusing to purge without --yes (standard input is not a terminal)");
        }
        print!("Type 'purge' to delete these directories permanently: ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if answer.trim() != "purge" {
            bail!("purge cancelled");
        }
    }
    for (label, path, _) in &existing {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("cannot remove {label} directory {}", path.display()))?;
        println!("Removed {label}: {}", path.display());
    }
    println!("LocalWeb data deleted for this account.");
    Ok(())
}

fn directory_size(path: &std::path::Path) -> u64 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if metadata.file_type().is_symlink() {
        return 0;
    }
    if metadata.is_file() {
        return metadata.len();
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| directory_size(&entry.path()))
        .sum()
}

#[expect(
    clippy::cast_precision_loss,
    reason = "human-readable sizes do not need exact integer precision"
)]
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn install_ca(paths: &Paths) -> Result<()> {
    let ca = CaPaths::new(paths);
    ca.ensure_ca()?;
    #[cfg(windows)]
    localweb_core::platform::install_current_user_ca(&ca.cert)?;
    #[cfg(not(windows))]
    run_helper("install-ca", &std::fs::read(&ca.cert)?)?;
    localweb_core::platform::install_browser_ca(&ca.cert)?;
    println!("{}", copy::HTTPS_TRUSTED);
    Ok(())
}

fn remove_ca(paths: &Paths) -> Result<()> {
    let ca = CaPaths::new(paths);
    if !ca.cert.is_file() {
        bail!("No LocalWeb HTTPS certificate found for this account.");
    }
    localweb_core::platform::remove_browser_ca()?;
    #[cfg(windows)]
    localweb_core::platform::remove_current_user_ca(&ca.cert)?;
    #[cfg(not(windows))]
    run_helper("remove-ca", &std::fs::read(&ca.cert)?)?;
    println!("{}", copy::HTTPS_TRUST_REMOVED);
    Ok(())
}

async fn sync_hosts(paths: &Paths) -> Result<()> {
    // Serialize the whole snapshot-and-write sequence with other CLI/MCP
    // processes; otherwise an older project list can overwrite a newer block.
    let _lock = HostsSyncLock::acquire(paths)?;
    let projects = match send_request(paths, Request::ProjectList).await? {
        Response::Projects(items) => items,
        Response::Error { code, message } => bail!("{code}: {message}"),
        _ => bail!(copy::UNEXPECTED_RESPONSE),
    };
    let names: Vec<_> = projects
        .into_iter()
        .map(|item| item.project.hostname)
        .collect();
    let address = match send_request(paths, Request::Status).await? {
        Response::Status(status) => status
            .gateway_bind_address
            .unwrap_or(localweb_core::hosts::DEFAULT_GATEWAY_ADDRESS),
        Response::Error { code, message } => bail!("{code}: {message}"),
        _ => bail!(copy::UNEXPECTED_RESPONSE),
    };
    let request = HostsSync {
        address,
        domains: names,
    };
    run_helper("sync-hosts", &serde_json::to_vec(&request)?)?;
    println!("{}", copy::domains_synced(request.domains.len(), address));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_uses_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(1024 * 1024), "1.0 MiB");
    }

    fn sample_project() -> (Project, PathBuf) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "localweb-cli-update-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(path.join("public")).unwrap();
        let project = Project::from_new(
            NewProject {
                path: path.clone(),
                name: Some("cli-update".into()),
                hostname: Some("cli-update.test".into()),
                web_port: Some(9_123),
                database_port: Some(33_123),
                document_root: Some(PathBuf::from("public")),
                profile: Some(Profile::Laravel),
                web_server: Some(WebServer::Apache),
                php: Some("8.4".into()),
                composer: Some("2".into()),
                node: Some("22".into()),
                database: Some(Database::Mariadb),
                cache: Some(Cache::Redis),
                supervisor: SupervisorConfig {
                    enabled: true,
                    command: vec!["@php".into(), "artisan".into(), "queue:work".into()],
                },
                cron: vec![localweb_core::CronEntry {
                    enabled: true,
                    schedule: "*/5 * * * *".into(),
                    command: vec!["@php".into(), "artisan".into(), "schedule:run".into()],
                }],
                cron_preset: false,
                clear_cron: false,
                mail: true,
                autostart: true,
            },
            &localweb_core::GlobalConfig::default(),
        )
        .unwrap();
        (project, path)
    }

    #[test]
    fn update_merges_unset_flags_with_existing_project() {
        let (mut existing, path) = sample_project();
        existing.enabled = false;
        existing
            .environment
            .insert("APP_ENV".into(), "local".into());
        existing.cron.push(localweb_core::CronEntry {
            enabled: false,
            schedule: "0 3 * * *".into(),
            command: vec!["@php".into(), "artisan".into(), "queue:prune".into()],
        });
        let input = update_input(
            &existing,
            UpdateOptions {
                php: Some("8.3".into()),
                ..Default::default()
            },
        );
        let updated =
            Project::from_update(&existing, input, &localweb_core::GlobalConfig::default())
                .unwrap();
        assert_eq!(updated.id, existing.id);
        assert_eq!(updated.name, existing.name);
        assert_eq!(updated.path, existing.path);
        assert_eq!(updated.hostname, existing.hostname);
        assert_eq!(updated.web_port, existing.web_port);
        assert_eq!(updated.database_port, existing.database_port);
        assert_eq!(updated.document_root, existing.document_root);
        assert_eq!(updated.profile, existing.profile);
        assert_eq!(updated.web_server, existing.web_server);
        assert_eq!(updated.composer, existing.composer);
        assert_eq!(updated.node, existing.node);
        assert_eq!(updated.database, existing.database);
        assert_eq!(updated.cache, existing.cache);
        assert_eq!(updated.supervisor, existing.supervisor);
        assert_eq!(updated.mail, existing.mail);
        assert_eq!(updated.autostart, existing.autostart);
        assert_eq!(updated.enabled, existing.enabled);
        assert_eq!(updated.environment, existing.environment);
        assert_eq!(updated.created_at, existing.created_at);
        assert_eq!(updated.php, "8.3");
        assert_eq!(updated.cron.len(), 2);
        assert_eq!(
            updated.cron.iter().filter(|entry| !entry.enabled).count(),
            1
        );
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn update_can_disable_services_and_clear_cron() {
        let (existing, path) = sample_project();
        let input = update_input(
            &existing,
            UpdateOptions {
                supervisor: SupervisorUpdate::Disable,
                cron: Some(Vec::new()),
                clear_cron: true,
                mail: Some(false),
                autostart: Some(false),
                ..Default::default()
            },
        );
        let updated =
            Project::from_update(&existing, input, &localweb_core::GlobalConfig::default())
                .unwrap();
        assert!(!updated.supervisor.enabled);
        assert_eq!(updated.cron, Vec::new());
        assert!(!updated.mail);
        assert!(!updated.autostart);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn update_enables_the_profile_worker_preset() {
        let (mut existing, path) = sample_project();
        existing.supervisor = SupervisorConfig::default();
        let input = update_input(
            &existing,
            UpdateOptions {
                supervisor: SupervisorUpdate::Enable,
                ..Default::default()
            },
        );
        let updated =
            Project::from_update(&existing, input, &localweb_core::GlobalConfig::default())
                .unwrap();
        assert!(updated.supervisor.enabled);
        assert_eq!(
            updated.supervisor.command,
            [
                "@php",
                "artisan",
                "queue:work",
                "--sleep=3",
                "--tries=3",
                "--timeout=90"
            ]
        );
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn update_flags_are_tri_state() {
        let cli = Cli::try_parse_from([
            "localweb",
            "project",
            "update",
            "app",
            "--no-mail",
            "--no-autostart",
            "--no-supervisor",
            "--clear-cron",
        ])
        .unwrap();
        match cli.command {
            CommandKind::Project {
                command:
                    ProjectCommand::Update {
                        no_mail,
                        no_autostart,
                        no_supervisor,
                        clear_cron,
                        ..
                    },
            } => {
                assert!(no_mail);
                assert!(no_autostart);
                assert!(no_supervisor);
                assert!(clear_cron);
            }
            _ => panic!("expected project update"),
        }
        assert!(
            Cli::try_parse_from([
                "localweb",
                "project",
                "update",
                "app",
                "--mail",
                "--no-mail"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "localweb",
                "project",
                "update",
                "app",
                "--cron-preset",
                "--clear-cron"
            ])
            .is_err()
        );
    }

    #[test]
    fn directory_size_ignores_symlinks() {
        let root = std::env::temp_dir().join(format!("localweb-cli-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("a"), b"1234").unwrap();
        std::fs::write(root.join("nested/b"), b"12").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("a"), root.join("link")).unwrap();
        assert_eq!(directory_size(&root), 6);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn shims_are_only_detected_for_known_tools() {
        assert_eq!(
            shim_tool_name(Some(OsString::from("/home/dev/.local/share/bin/php"))),
            Some("php".to_owned())
        );
        assert_eq!(
            shim_tool_name(Some(OsString::from("/opt/localweb/bin/composer.exe"))),
            Some("composer".to_owned())
        );
        assert_eq!(
            shim_tool_name(Some(OsString::from("/usr/bin/localweb"))),
            None
        );
        assert_eq!(shim_tool_name(None), None);
    }

    #[test]
    fn user_path_edits_are_idempotent() {
        let current = r"C:\Windows;C:\Tools";
        let entry = r"C:\Users\dev\AppData\Local\LocalWeb\bin";
        let added = add_path_entry(current, entry);
        assert_eq!(added, format!("{entry};{current}"));
        assert_eq!(add_path_entry(&added, entry), added);
        assert_eq!(remove_path_entry(&added, entry), current);
        assert_eq!(
            remove_path_entry(current, &entry.to_ascii_uppercase()),
            current
        );
        assert_eq!(add_path_entry("", entry), entry);
        assert_eq!(remove_path_entry(entry, entry), "");
    }

    #[test]
    fn registry_query_values_are_read_back() {
        let query = "HKEY_CURRENT_USER\\Environment\r\n    PATH    REG_EXPAND_SZ    C:\\Windows;C:\\Tools\r\n\r\n";
        assert_eq!(
            parse_reg_value(query).as_deref(),
            Some(r"C:\Windows;C:\Tools")
        );
        let plain = "    Path    REG_SZ    C:\\Only\r\n";
        assert_eq!(parse_reg_value(plain).as_deref(), Some(r"C:\Only"));
        assert_eq!(parse_reg_value("    TEMP    REG_SZ    C:\\Temp\r\n"), None);
        assert_eq!(parse_reg_value("PATH"), None);
    }

    #[cfg(unix)]
    #[test]
    fn managed_shell_block_is_replaced_in_place() {
        let block =
            "# BEGIN LOCALWEB SHELL\nlocalweb_bin=\"/opt/localweb/bin\"\n# END LOCALWEB SHELL\n";
        let original = "export EDITOR=vim\n";
        let updated = format!("{original}{block}");
        assert!(has_managed_block(&updated));
        assert_eq!(strip_managed_block(&updated), original);
        assert_eq!(strip_managed_block(original), original);
        assert!(!has_managed_block(
            "# END LOCALWEB SHELL\n# BEGIN LOCALWEB SHELL\n"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn shell_blocks_quote_the_shim_directory() {
        let posix = shell_block(Path::new("/home/$user/LocalWeb bin"), false);
        assert!(posix.contains(r#"localweb_bin="/home/\$user/LocalWeb bin""#));
        assert!(posix.starts_with(SHELL_BLOCK_BEGIN));
        assert!(posix.ends_with(&format!("{SHELL_BLOCK_END}\n")));
        let fish = shell_block(Path::new("/home/dev/.local/share/localweb/bin"), true);
        assert!(fish.contains("set -gx PATH $localweb_bin $PATH"));
        assert!(fish.contains("/home/dev/.local/share/localweb/bin"));
    }

    #[test]
    fn system_candidates_cover_platform_executables() {
        let candidates = tool_candidates("php");
        if cfg!(windows) {
            assert_eq!(
                candidates,
                vec![
                    "php.exe".to_owned(),
                    "php.cmd".to_owned(),
                    "php.bat".to_owned()
                ]
            );
        } else {
            assert_eq!(candidates, vec!["php".to_owned()]);
        }
    }
}
