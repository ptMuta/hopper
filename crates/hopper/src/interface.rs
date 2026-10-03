//! Public, explicit instance CLI. The directory-oriented engine is an implementation detail.
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::managed::{self, Instance};

#[derive(Debug, Parser)]
#[command(
    name = "hopper",
    version,
    about = "Manage named Minecraft servers under systemd"
)]
pub struct App {
    #[arg(long, global = true, value_enum, default_value = "user")]
    pub scope: Scope,
    #[arg(short, long, global = true)]
    pub yes: bool,
    #[arg(short = 'n', long, global = true)]
    pub dry_run: bool,
    #[arg(short, long, global = true)]
    pub quiet: bool,
    #[arg(short, long, global = true)]
    pub verbose: bool,
    #[arg(long, global = true)]
    pub json: bool,
    /// Never ask; fail listing missing flags instead (also HOPPER_NO_INPUT=1 or CI).
    /// HOPPER_ACCESSIBLE=1 asks numbered questions line by line instead of drawing menus.
    #[arg(long, global = true)]
    pub no_input: bool,
    #[command(subcommand)]
    pub command: Option<Action>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    User,
    System,
}
impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Modrinth,
    Curseforge,
    Gtnh,
    Mrpack,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct Source {
    #[arg(long, value_enum)]
    pub provider: Provider,
    /// Registry slug or canonical project ID (never inferred from a URL).
    #[arg(long, conflicts_with_all = ["collection", "file", "url"])]
    pub pack: Option<String>,
    #[arg(long, conflicts_with_all = ["pack", "file", "url"])]
    pub collection: Option<String>,
    #[arg(long, conflicts_with_all = ["pack", "collection", "url"])]
    pub file: Option<PathBuf>,
    #[arg(long, conflicts_with_all = ["pack", "collection", "file"])]
    pub url: Option<String>,
    /// Floating channel: stable/testing; GTNH also daily/experimental.
    #[arg(long, conflicts_with = "pack_version")]
    pub channel: Option<String>,
    /// Exact release version or registry version/file ID.
    #[arg(long, conflicts_with = "channel")]
    pub pack_version: Option<String>,
    #[arg(long)]
    pub mc: Option<String>,
    #[arg(long, value_enum)]
    pub loader: Option<crate::cli::Loader>,
}

impl Source {
    pub fn validate(&self) -> Result<()> {
        let channel = self.channel.as_deref().unwrap_or("stable");
        ensure!(
            matches!(channel, "stable" | "testing")
                || (self.provider == Provider::Gtnh && matches!(channel, "daily" | "experimental")),
            "unsupported channel {channel:?} for {:?}",
            self.provider
        );
        match self.provider {
            Provider::Modrinth => ensure!(
                (self.pack.is_some() ^ self.collection.is_some())
                    && self.file.is_none()
                    && self.url.is_none(),
                "modrinth requires --pack or --collection"
            ),
            Provider::Curseforge => ensure!(
                self.pack.is_some()
                    && self.collection.is_none()
                    && self.file.is_none()
                    && self.url.is_none(),
                "curseforge requires --pack"
            ),
            Provider::Gtnh => ensure!(
                self.pack.is_none()
                    && self.collection.is_none()
                    && self.file.is_none()
                    && self.url.is_none(),
                "gtnh is a single-pack provider; omit --pack/--file/--url"
            ),
            Provider::Mrpack => ensure!(
                (self.file.is_some() ^ self.url.is_some())
                    && self.pack.is_none()
                    && self.collection.is_none()
                    && self.channel.is_none()
                    && self.pack_version.is_none(),
                "mrpack requires --file or --url and has no registry channel/version selector"
            ),
        }
        if let Some(pack) = &self.pack {
            ensure!(
                !pack.is_empty()
                    && pack
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "--pack must be a slug or project ID, not a URL/path"
            );
        }
        if let Some(id) = &self.collection {
            ensure!(
                self.pack_version.is_none(),
                "collections have no pack version; choose --channel and explicit --mc/--loader"
            );
            ensure!(
                self.mc.is_some() && self.loader.is_some(),
                "collections require explicit --mc and --loader"
            );
            ensure!(
                id.chars().all(|c| c.is_ascii_alphanumeric()),
                "invalid collection ID"
            );
        }
        if let Some(url) = &self.url {
            let url = url::Url::parse(url)?;
            hopper_core::net::HostAllowlist::packs().check(&url)?;
        }
        if let Some(version) = &self.pack_version {
            ensure!(
                !version.contains(['/', '\\', '\n', '\r']),
                "invalid version"
            );
        }
        ensure!(
            !(self.pack_version.is_some() && matches!(channel, "daily" | "experimental")),
            "development channels cannot use release pins"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Args, Serialize, Deserialize)]
pub struct RuntimeOptions {
    #[arg(long)]
    pub java: Option<PathBuf>,
    #[arg(long)]
    pub java_major: Option<u32>,
    #[arg(long, value_enum)]
    pub java_vendor: Option<crate::cli::JavaVendorArg>,
    #[arg(long)]
    pub jvm_args_file: Option<PathBuf>,
    #[arg(long)]
    pub no_optional: bool,
    #[arg(long)]
    pub force_include: Vec<String>,
    #[arg(long)]
    pub force_exclude: Vec<String>,
    #[arg(long)]
    pub allow_client_pack: bool,
    #[arg(long)]
    pub skip_blocked: bool,
}

#[derive(Debug, Args)]
pub struct Create {
    pub name: String,
    #[command(flatten)]
    pub source: Source,
    #[command(flatten)]
    pub runtime: RuntimeOptions,
    /// Explicitly accept the Minecraft EULA; --yes never implies this.
    #[arg(long)]
    pub eula: bool,
    /// Enable and start after successful installation.
    #[arg(long)]
    pub start: bool,
    #[arg(long)]
    pub rcon_firewall_confirmed: bool,
}

#[derive(Debug, Subcommand)]
pub enum Action {
    /// Create a managed instance and its mandatory systemd service.
    Install(Create),
    /// Back up and move a stopped existing installation into managed storage.
    Onboard {
        #[command(flatten)]
        create: Create,
        #[arg(long)]
        from: PathBuf,
        #[arg(long)]
        installed_version: Option<String>,
        #[arg(long)]
        backup: Option<PathBuf>,
    },
    /// Change the desired target or schedules without updating pack files.
    Configure {
        name: String,
        #[arg(long)]
        java_major: Option<u32>,
        #[arg(long, conflicts_with = "pack_version")]
        channel: Option<String>,
        #[arg(long, conflicts_with = "channel")]
        pack_version: Option<String>,
        #[arg(long)]
        update: Option<String>,
        #[arg(long)]
        restart: Option<String>,
        #[arg(long)]
        warn: Option<u32>,
        #[arg(long)]
        rcon_firewall_confirmed: bool,
    },
    List,
    Status {
        name: String,
    },
    Check {
        name: String,
    },
    Update {
        name: String,
    },
    Start {
        name: String,
        #[arg(long)]
        rcon_firewall_confirmed: bool,
    },
    Stop {
        name: String,
    },
    Restart {
        name: String,
    },
    Remove {
        name: String,
    },
    Repair {
        name: String,
    },
    Disable {
        name: String,
        file: String,
    },
    Enable {
        name: String,
        file: String,
    },
    Show {
        name: String,
        #[arg(long)]
        plain: bool,
    },
    Chat {
        name: String,
        #[arg(long)]
        nickname: Option<String>,
    },
    Console {
        name: String,
        #[arg(last = true)]
        command: Vec<String>,
    },
    Logs {
        name: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
    Providers,
    Search {
        #[arg(long, value_enum)]
        provider: Provider,
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    Versions {
        #[command(flatten)]
        source: Source,
    },
    SelfUpdate {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        force: bool,
    },
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    #[command(name = "__worker", hide = true)]
    Worker {
        name: String,
        operation: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
    #[command(name = "__maintenance", hide = true)]
    Maintenance {
        name: String,
        #[arg(long)]
        update: bool,
        #[arg(long)]
        restart: bool,
    },
    #[command(name = "__start-check", hide = true)]
    StartCheck {
        #[arg(long)]
        dir: PathBuf,
    },
    #[command(name = "__stop", hide = true)]
    StopWorker {
        #[arg(long)]
        dir: PathBuf,
        // systemd omits $MAINPID after the main process has already exited.
        #[arg(long, num_args = 0..=1, default_missing_value = "0", default_value = "0")]
        pid: u32,
    },
}

#[tokio::main(flavor = "current_thread")]
pub async fn run(app: &App) -> Result<i32> {
    crate::service::set_scope(app.scope);
    let hidden = matches!(
        app.command,
        Some(
            Action::Worker { .. }
                | Action::Maintenance { .. }
                | Action::StartCheck { .. }
                | Action::StopWorker { .. }
        )
    );
    crate::prompt::init(app.no_input, app.json, app.yes, hidden);
    let Some(action) = &app.command else {
        App::command().print_help()?;
        println!();
        return Ok(0);
    };
    match action {
        Action::Completions { shell } => {
            clap_complete::generate(
                *shell,
                &mut App::command(),
                "hopper",
                &mut std::io::stdout(),
            );
            Ok(0)
        }
        Action::Providers => {
            println!(
                "modrinth   --pack ID|SLUG or --collection ID --mc VERSION --loader LOADER; stable/testing; search\ncurseforge --pack ID|SLUG; stable/testing; search; CURSEFORGE_API_KEY required\ngtnh       no pack argument; stable/testing/daily/experimental\nmrpack     --file PATH or --url HTTPS_URL; no registry channels or search"
            );
            Ok(0)
        }
        Action::Search {
            provider,
            query,
            limit,
            offset,
        } => discovery(*provider, Some(query), None, *limit, *offset, app.json).await,
        Action::Versions { source } => {
            source.validate()?;
            discovery(source.provider, None, Some(source), 50, 0, app.json).await
        }
        Action::Install(create) => create_instance(app, create, None).await,
        Action::Onboard {
            create,
            from,
            installed_version,
            backup,
        } => {
            create_instance(
                app,
                create,
                Some((from, installed_version.as_deref(), backup.as_deref())),
            )
            .await
        }
        Action::List => {
            for record in managed::list(app.scope)? {
                if app.json {
                    println!("{}", serde_json::to_string(&record)?);
                } else {
                    println!(
                        "{}\t{:?}\t{}\t{}",
                        record.name,
                        record.state,
                        record.source.channel.as_deref().unwrap_or("stable"),
                        record.server().display()
                    );
                }
            }
            Ok(0)
        }
        Action::Worker {
            name,
            operation,
            args,
        } => worker(app, &managed::load(app.scope, name)?, operation, args).await,
        Action::Maintenance {
            name,
            update,
            restart,
        } => managed::maintain(&managed::load(app.scope, name)?, *update, *restart),
        Action::StartCheck { dir } => {
            ensure!(!managed::is_root(), "start checks must run unprivileged");
            crate::service::ensure_conflicts_resolved(dir)?;
            Ok(0)
        }
        Action::StopWorker { dir, pid } => {
            ensure!(!managed::is_root(), "stop workers must run unprivileged");
            crate::service::stop_server(dir, *pid)
        }
        Action::SelfUpdate { check, force } => {
            crate::selfupdate::run(&crate::selfupdate::Options {
                check: *check || app.dry_run,
                force: *force,
                yes: app.yes,
                quiet: app.quiet,
                user_agent: &crate::user_agent(),
                cache: &crate::cache_dir()?,
            })
            .await
        }
        other => instance_action(app, other),
    }
}

fn engine(app: &App, record: Option<&Instance>) -> crate::cli::Cli {
    let mut cli = crate::cli::Cli::parse_from(["hopper"]);
    cli.global.yes = app.yes;
    cli.global.quiet = app.quiet;
    cli.global.verbose = app.verbose;
    cli.global.json = app.json;
    cli.global.dry_run = app.dry_run;
    if let Some(record) = record {
        cli.global.dir = record.server();
        cli.install.pack = Some(format!(
            "{:?}:{}",
            record.source.provider,
            record.source.pack.as_deref().unwrap_or(&record.name)
        ));
        cli.install.mc = record.source.mc.clone();
        cli.install.loader = record.source.loader;
        cli.install.java = record.runtime.java.clone();
        cli.install.java_vendor = record.runtime.java_vendor;
        cli.install.no_optional = record.runtime.no_optional;
        cli.install.force_include = record.runtime.force_include.clone();
        cli.install.force_exclude = record.runtime.force_exclude.clone();
        cli.install.allow_client_pack = record.runtime.allow_client_pack;
        cli.install.skip_blocked = record.runtime.skip_blocked;
        cli.install.managed_launcher = true;
        cli.install.collection_channel = Some(
            record
                .source
                .channel
                .clone()
                .unwrap_or_else(|| "stable".into()),
        );
    }
    cli
}

async fn worker(app: &App, record: &Instance, operation: &str, args: &[String]) -> Result<i32> {
    ensure!(
        !managed::is_root(),
        "pack operations must never execute as root"
    );
    managed::check_worker_owner(record)?;
    match operation {
        "start-check" => {
            crate::service::ensure_conflicts_resolved(&record.server())?;
            Ok(0)
        }
        "status" => crate::status(&record.server()),
        "repair" => {
            if record.server().join(".hopper/prepared.json").is_file() {
                crate::apply_prepared(&record.server(), &record.cache()?)?;
            }
            crate::migration::repair(record)?;
            Ok(0)
        }
        "disable" | "enable" => crate::toggle(
            &record.server(),
            args.first().context("missing filename")?,
            operation == "disable",
        ),
        "show" => crate::show::run(&record.server(), args.first().is_some_and(|a| a == "plain")),
        "chat" => crate::chat::run(&record.server(), args.first().map(String::as_str)),
        "console" => crate::service::console(&record.server(), args),
        "bootstrap" => {
            crate::accept_eula(&record.server())?;
            let path = record.server().join("server.properties");
            let text = std::fs::read_to_string(&path)?;
            let game = record.game_port.context("missing game port")?.to_string();
            let rcon = record.rcon_port.context("missing RCON port")?.to_string();
            let text = hopper_core::server::properties::set(
                &text,
                &[
                    ("server-port", &game),
                    ("rcon.port", &rcon),
                    ("rcon.password", ""),
                ],
            );
            hopper_core::fs::write_private_atomic(&path, text.as_bytes())?;
            let setup = crate::service::ensure_rcon(&record.server())?;
            println!(
                "RCON port {} configured (changed={}); block external access.",
                setup.port, setup.changed
            );
            Ok(0)
        }
        "rcon-port" | "inspect" => {
            let text = std::fs::read_to_string(record.server().join("server.properties"))?;
            let port: u16 = hopper_core::server::properties::get(&text, "rcon.port")
                .context("missing RCON port")?
                .parse()?;
            if operation == "rcon-port" {
                println!("{port}");
            } else {
                let lock = crate::read_lockfile(&record.server())?.context("missing lock")?;
                let game: u16 = hopper_core::server::properties::get(&text, "server-port")
                    .context("missing game port")?
                    .parse()?;
                println!(
                    "{}",
                    serde_json::json!({"game_port":game,"rcon_port":port,"version":lock.pack.version_label,"project_id":lock.pack.project_id,"java_major":lock.server.java_major})
                );
            }
            Ok(0)
        }
        "rcon" => {
            let text = std::fs::read_to_string(record.server().join("server.properties"))?;
            let get = |key| hopper_core::server::properties::get(&text, key);
            let host = get("server-ip")
                .filter(|s| !s.is_empty())
                .unwrap_or("127.0.0.1");
            let port: u16 = get("rcon.port").context("RCON port missing")?.parse()?;
            let mut rcon = hopper_core::server::rcon::Rcon::connect(
                (host, port),
                get("rcon.password").context("RCON password missing")?,
            )?;
            for cmd in args {
                println!("{}", rcon.command(cmd)?);
            }
            Ok(0)
        }
        "restore" => {
            crate::migration::restore(record)?;
            Ok(0)
        }
        "apply" => crate::apply_prepared(&record.server(), &crate::cache_dir()?),
        "install" | "check" | "stage" => {
            let mut cli = engine(app, Some(record));
            cli.global.yes = true;
            cli.global.dry_run = operation == "check";
            cli.install.stage_only = operation == "stage";
            cli.install.pack_changes_only = operation == "check";
            cli.install.source_override = Some(select_source(record).await?);
            crate::install(&cli).await
        }
        _ => bail!("unknown worker operation"),
    }
}

pub async fn select_source(record: &Instance) -> Result<hopper_core::source::SourceSpec> {
    use hopper_core::{
        api,
        net::{HostAllowlist, HttpClient},
        source::SourceSpec,
    };
    let source = &record.source;
    let channel = source.channel.as_deref().unwrap_or("stable");
    let installed = crate::read_lockfile(&record.server())?;
    match source.provider {
        Provider::Mrpack => Ok(match (&source.file, &source.url) {
            (Some(path), _) => SourceSpec::File {
                path: path.to_string_lossy().into(),
            },
            (_, Some(url)) => SourceSpec::Url {
                url: url::Url::parse(url)?,
            },
            _ => bail!("missing mrpack source"),
        }),
        Provider::Gtnh => {
            let channel = source
                .pack_version
                .as_deref()
                .and_then(|v| v.split_once(':'))
                .filter(|(c, _)| matches!(*c, "daily" | "experimental"))
                .map(|(c, _)| c)
                .unwrap_or(channel);
            if let Some(lock) = &installed {
                if matches!(channel, "stable" | "testing") {
                    let client = HttpClient::new(&crate::user_agent(), HostAllowlist::gtnh())?;
                    let releases = client
                        .get_json::<std::collections::BTreeMap<String, crate::gtnh::Release>>(
                            crate::gtnh::CATALOG,
                        )
                        .await?;
                    let (target, _) =
                        crate::gtnh::choose(&releases, channel, source.pack_version.as_deref())?;
                    let old = lock
                        .pack
                        .version_label
                        .as_deref()
                        .unwrap_or("")
                        .split(':')
                        .find(|s| s.chars().next().is_some_and(|c| c.is_ascii_digit()))
                        .unwrap_or("");
                    let nums = |s: &str| -> Vec<u32> {
                        s.split(|c: char| !c.is_ascii_digit())
                            .filter_map(|p| p.parse().ok())
                            .take(3)
                            .collect()
                    };
                    ensure!(
                        nums(target) >= nums(old)
                            && crate::gtnh::natural(target) >= crate::gtnh::natural(old),
                        "target is older than the installed GTNH world; automatic downgrades are refused"
                    );
                }
            }
            Ok(SourceSpec::Gtnh {
                channel: channel.into(),
                version: source.pack_version.clone(),
                java_major: record.runtime.java_major,
            })
        }
        Provider::Modrinth => {
            if let Some(id) = &source.collection {
                return Ok(SourceSpec::Collection { id: id.clone() });
            }
            let client = HttpClient::new(&crate::user_agent(), HostAllowlist::api())?;
            let id = installed
                .as_ref()
                .and_then(|l| l.pack.project_id.as_deref())
                .unwrap_or(source.pack.as_deref().context("missing pack")?);
            let project = api::client::fetch_project(&client, id).await?;
            api::client::require_modpack(&project, id)?;
            let versions = api::client::fetch_versions(&client, project.id.as_str()).await?;
            let selected = if let Some(pin) = &source.pack_version {
                let matches: Vec<_> = versions
                    .iter()
                    .filter(|v| v.id.as_str() == pin || v.version_number == *pin)
                    .collect();
                ensure!(
                    matches.len() == 1,
                    "version pin is absent or ambiguous; use an exact version ID"
                );
                matches[0]
            } else {
                versions
                    .iter()
                    .filter(|v| {
                        source
                            .mc
                            .as_deref()
                            .is_none_or(|mc| v.game_versions.iter().any(|g| g == mc))
                    })
                    .find(|v| {
                        matches!(
                            (channel, v.version_type),
                            ("stable", Some(api::modrinth::VersionType::Release))
                                | (
                                    "testing",
                                    Some(
                                        api::modrinth::VersionType::Beta
                                            | api::modrinth::VersionType::Alpha
                                    )
                                )
                        )
                    })
                    .context(
                        "no version in requested channel; no prerelease fallback is performed",
                    )?
            };
            if let Some(old) = installed.as_ref().and_then(|l| l.pack.file_id.as_deref()) {
                if let Some(old) = versions.iter().find(|v| v.id.as_str() == old) {
                    ensure!(
                        selected.date_published >= old.date_published,
                        "automatic version downgrades are refused"
                    );
                }
            }
            Ok(SourceSpec::Pack {
                slug: project.id.to_string(),
                version: Some(selected.id.to_string()),
            })
        }
        Provider::Curseforge => {
            let client = cf_client()?;
            let project = api::curseforge::find_modpack(
                &client,
                source.pack.as_deref().context("missing pack")?,
            )
            .await?;
            let files = api::curseforge::list_files(&client, project.id).await?;
            let selected = if let Some(pin) = &source.pack_version {
                let matches: Vec<_> = files
                    .iter()
                    .filter(|f| f.id.to_string() == *pin || f.display_name == *pin)
                    .collect();
                ensure!(
                    matches.len() == 1,
                    "file pin is absent or ambiguous; use an exact file ID"
                );
                matches[0]
            } else {
                files
                    .iter()
                    .filter(|f| !f.is_server_pack && f.is_available != Some(false))
                    .filter(|f| {
                        source
                            .mc
                            .as_ref()
                            .is_none_or(|mc| f.game_versions.iter().any(|v| v == mc))
                    })
                    .find(|f| {
                        matches!(
                            (channel, f.release()),
                            ("stable", Some(api::curseforge::ReleaseType::Release))
                                | (
                                    "testing",
                                    Some(
                                        api::curseforge::ReleaseType::Beta
                                            | api::curseforge::ReleaseType::Alpha,
                                    ),
                                )
                        )
                    })
                    .context("no file in requested channel")?
            };
            if let Some(old) = installed.as_ref().and_then(|l| l.pack.file_id.as_deref()) {
                if let Some(old) = files.iter().find(|f| f.id.to_string() == old) {
                    ensure!(
                        selected.file_date >= old.file_date,
                        "automatic version downgrades are refused"
                    );
                }
            }
            Ok(SourceSpec::CurseForge {
                slug: project.slug,
                version: Some(selected.id.to_string()),
            })
        }
    }
}

fn cf_client() -> Result<hopper_core::net::HttpClient> {
    let key = crate::curseforge::api_key(None).context("CURSEFORGE_API_KEY is required")?;
    Ok(hopper_core::net::HttpClient::with_secret_header(
        &crate::user_agent(),
        hopper_core::net::HostAllowlist::curseforge_api(),
        "x-api-key",
        &key,
    )?)
}

async fn discovery(
    provider: Provider,
    query: Option<&str>,
    source: Option<&Source>,
    limit: u32,
    offset: u32,
    json: bool,
) -> Result<i32> {
    ensure!(!managed::is_root(), "run provider discovery without sudo");
    ensure!(
        (1..=50).contains(&limit),
        "--limit must be between 1 and 50"
    );
    let encode = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let mut value: serde_json::Value = match provider {
        Provider::Gtnh if query.is_some() => {
            serde_json::json!([{"id":"gtnh", "title":"GregTech: New Horizons", "channels":["stable","testing","daily","experimental"]}])
        }
        Provider::Gtnh => {
            let client = hopper_core::net::HttpClient::new(
                &crate::user_agent(),
                hopper_core::net::HostAllowlist::gtnh(),
            )?;
            if let Some(channel @ ("daily" | "experimental")) =
                source.and_then(|s| s.channel.as_deref())
            {
                client.get_json(&format!("https://raw.githubusercontent.com/GTNewHorizons/DreamAssemblerXXL/master/releases/manifests/{channel}.json")).await?
            } else {
                client.get_json(crate::gtnh::CATALOG).await?
            }
        }
        Provider::Modrinth => {
            let client = hopper_core::net::HttpClient::new(
                &crate::user_agent(),
                hopper_core::net::HostAllowlist::api(),
            )?;
            if let Some(query) = query {
                client.get_json(&format!("https://api.modrinth.com/v2/search?query={}&facets={}&limit={limit}&offset={offset}", encode(query), encode("[[\"project_type:modpack\"]]"))).await?
            } else {
                serde_json::Value::Array(hopper_core::api::client::fetch_versions(&client, source.and_then(|s| s.pack.as_deref()).context("versions requires --pack")?).await?.into_iter()
                .map(|v| serde_json::json!({"id":v.id,"version":v.version_number,"type":v.version_type.map(|t| format!("{t:?}")),"published":v.date_published,"minecraft":v.game_versions})).collect())
            }
        }
        Provider::Curseforge => {
            let client = cf_client()?;
            if let Some(query) = query {
                client.get_json(&format!("https://api.curseforge.com/v1/mods/search?gameId=432&classId=4471&searchFilter={}&pageSize={limit}&index={offset}", encode(query))).await?
            } else {
                let project = hopper_core::api::curseforge::find_modpack(
                    &client,
                    source
                        .and_then(|s| s.pack.as_deref())
                        .context("versions requires --pack")?,
                )
                .await?;
                serde_json::Value::Array(hopper_core::api::curseforge::list_files(&client, project.id).await?.into_iter().filter(|f| !f.is_server_pack && f.is_available != Some(false))
                    .map(|f| serde_json::json!({"id":f.id,"version":f.display_name,"type":f.release(),"published":f.file_date})).collect())
            }
        }
        Provider::Mrpack => {
            bail!("mrpack is a file/archive provider and has no search or version catalog")
        }
    };
    if let Some(source) = source {
        if let Some(channel) = source.channel.as_deref() {
            if let Some(rows) = value.as_array_mut() {
                rows.retain(|row| match channel {
                    "stable" => row["type"]
                        .as_str()
                        .is_some_and(|t| t.eq_ignore_ascii_case("release")),
                    "testing" => row["type"].as_str().is_some_and(|t| {
                        t.eq_ignore_ascii_case("beta") || t.eq_ignore_ascii_case("alpha")
                    }),
                    _ => true,
                });
            } else if provider == Provider::Gtnh && matches!(channel, "stable" | "testing") {
                if let Some(rows) = value.as_object_mut() {
                    rows.retain(|_, row| {
                        row["title"]
                            == if channel == "stable" {
                                "Stable release"
                            } else {
                                "Beta release"
                            }
                    });
                }
            }
        }
    }
    if json {
        println!("{}", serde_json::to_string(&value)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(0)
}

async fn create_instance(
    app: &App,
    create: &Create,
    onboarding: Option<(&PathBuf, Option<&str>, Option<&std::path::Path>)>,
) -> Result<i32> {
    create.source.validate()?;
    ensure!(
        create.runtime.java_major.is_none() || create.source.provider == Provider::Gtnh,
        "--java-major selects a GTNH distribution; other providers use their declared Java requirement or --java PATH"
    );
    managed::validate_name(&create.name)?;
    managed::preflight(app.scope)?;
    if managed::record_path(app.scope, &create.name)?.exists() {
        let mut existing = managed::load(app.scope, &create.name)?;
        ensure!(
            existing.state == managed::State::Detached,
            "instance already exists; use configure/update or repair"
        );
        let from = onboarding.context("detached instance retains data; use onboard --from its managed server path to reattach")?.0;
        ensure!(
            from.canonicalize()? == existing.server().canonicalize()?,
            "reattachment must use the existing managed server directory"
        );
        ensure!(
            serde_json::to_value(&existing.source)? == serde_json::to_value(&create.source)?,
            "reattachment cannot change pack identity/target; reattach with the recorded source, then configure it"
        );
        if app.dry_run {
            println!("Would reattach {}", existing.name);
            return Ok(10);
        }
        let _guard = managed::operation_lock(&existing)?;
        ensure!(
            managed::invoke(&existing, "repair", &[])? == 0,
            "retained installation is incomplete"
        );
        managed::install_units(&existing)?;
        existing.state = managed::State::Ready;
        managed::save(&existing)?;
        drop(_guard);
        if create.start {
            managed::start(&mut existing, create.rcon_firewall_confirmed)?;
        }
        println!("Reattached {}; retained data unchanged.", existing.name);
        return Ok(0);
    }
    let _allocation = if app.dry_run {
        None
    } else {
        Some(managed::allocation_lock(app.scope)?)
    };
    let mut record = Instance::new(
        app.scope,
        &create.name,
        create.source.clone(),
        create.runtime.clone(),
    )?;
    ensure!(
        !managed::record_path(app.scope, &create.name)?.exists(),
        "instance already exists; use configure/update or repair"
    );
    if app.dry_run {
        println!(
            "Would create {:?} service {} in {}",
            app.scope,
            record.unit(),
            record.server().display()
        );
        return Ok(10);
    }
    let _guard = managed::operation_lock(&record)?;
    let existing_eula = onboarding.is_some_and(|(from, _, _)| crate::eula_accepted(from));
    let accepted = existing_eula
        || create.eula
        || crate::prompt::consent("Accept the Minecraft EULA? https://aka.ms/MinecraftEULA")?;
    if !accepted {
        return Err(crate::prompt::Declined(
            "EULA acceptance is required: pass --eula (--yes never accepts it)".into(),
        )
        .into());
    }
    if let Some((from, installed, backup)) = onboarding {
        crate::migration::onboard(app, &mut record, from, installed, backup).await?;
    } else {
        if let Some(path) = &mut record.source.file {
            *path = path.canonicalize().context("reading source archive")?;
        }
        managed::prepare(&mut record)?;
        let code = managed::invoke(&record, "install", &[])?;
        ensure!(
            code == 0,
            "pack installation failed ({code}); run hopper repair {}",
            record.name
        );
    }
    drop(_allocation);
    let code = managed::invoke(&record, "bootstrap", &[])?;
    ensure!(code == 0, "service bootstrap failed ({code})");
    managed::refresh(&mut record)?;
    managed::install_units(&record)?;
    record.state = managed::State::Ready;
    managed::save(&record)?;
    crate::migration::finish_move(&record)?;
    println!(
        "Installed {} ({:?}) at {}. Service is stopped and disabled.",
        record.name,
        record.scope,
        record.server().display()
    );
    let start =
        create.start || (crate::prompt::enabled() && crate::prompt::ask("Start now?", false)?);
    if start {
        drop(_guard);
        managed::start(&mut record, create.rcon_firewall_confirmed)?;
    }
    Ok(0)
}

fn instance_action(app: &App, action: &Action) -> Result<i32> {
    use Action::*;
    let name = match action {
        Configure { name, .. }
        | Status { name }
        | Check { name }
        | Update { name }
        | Start { name, .. }
        | Stop { name }
        | Restart { name }
        | Remove { name }
        | Repair { name }
        | Disable { name, .. }
        | Enable { name, .. }
        | Show { name, .. }
        | Chat { name, .. }
        | Console { name, .. }
        | Logs { name, .. } => name,
        _ => unreachable!(),
    };
    let mut record = managed::load(app.scope, name)?;
    if app.dry_run && !matches!(action, Status { .. } | Check { .. } | Logs { .. }) {
        println!("Would operate on {} ({:?})", name, app.scope);
        return Ok(10);
    }
    match action {
        Configure {
            java_major,
            channel,
            pack_version,
            update,
            restart,
            warn,
            rcon_firewall_confirmed,
            ..
        } => {
            let _guard = managed::operation_lock(&record)?;
            if let Some(major) = java_major {
                ensure!(
                    record.source.provider == Provider::Gtnh && [8, 17, 21, 25].contains(major),
                    "--java-major is an explicit GTNH distribution selection (8/17/21/25)"
                );
                record.runtime.java_major = Some(*major);
            }
            if let Some(channel) = channel {
                record.source.channel = Some(channel.clone());
                record.source.pack_version = None;
            }
            if let Some(pin) = pack_version {
                record.source.pack_version = Some(pin.clone());
                record.source.channel = None;
            }
            record.source.validate()?;
            if let Some(update) = update {
                managed::validate_schedule(update)?;
                record.update = update.clone();
            }
            if let Some(restart) = restart {
                managed::validate_schedule(restart)?;
                record.restart = restart.clone();
            }
            if let Some(warn) = warn {
                ensure!(*warn <= 86400, "warning exceeds one day");
                record.warn = *warn;
            }
            if *rcon_firewall_confirmed {
                record.firewall_port = managed::rcon_port(&record).ok();
            }
            managed::save(&record)?;
            managed::install_units(&record)?;
            println!(
                "Configured {}; pack files unchanged. Run hopper update {} to apply the target.",
                name, name
            );
            Ok(0)
        }
        Status { .. } => {
            if app.json {
                println!("{}", serde_json::to_string(&record)?);
                Ok(0)
            } else {
                println!(
                    "{} {:?} {:?}\nDirectory: {}\nInstalled: {:?}\nTarget: {:?} {:?}\nSchedules: update={} restart={} warn={}s\nService: {}",
                    name,
                    record.scope,
                    record.state,
                    record.server().display(),
                    record.installed_version,
                    record.source.channel,
                    record.source.pack_version,
                    record.update,
                    record.restart,
                    record.warn,
                    managed::property(&record, "ActiveState")
                        .unwrap_or_else(|_| "manager unavailable".into())
                );
                let code = managed::invoke(&record, "status", &[])?;
                println!(
                    "\nCheck for updates: hopper check {name} --scope {}",
                    record.scope.name()
                );
                Ok(code)
            }
        }
        Check { .. } => {
            let _guard = managed::operation_lock(&record)?;
            managed::invoke(&record, "check", &[])
        }
        Update { .. } => managed::maintain(&record, true, false),
        Restart { .. } => managed::maintain(&record, false, true),
        Start {
            rcon_firewall_confirmed,
            ..
        } => {
            managed::start(&mut record, *rcon_firewall_confirmed)?;
            Ok(0)
        }
        Stop { .. } => {
            managed::stop(&record)?;
            Ok(0)
        }
        Remove { .. } => {
            managed::remove(&mut record)?;
            Ok(0)
        }
        Repair { .. } => {
            let _guard = managed::operation_lock(&record)?;
            ensure!(
                managed::property(&record, "ActiveState").or_else(|e| {
                    if record.state == managed::State::Pending && !record.started {
                        Ok("inactive".into())
                    } else {
                        Err(e)
                    }
                })? == "inactive",
                "stop the server before repair"
            );
            if record.scope == Scope::System {
                managed::refresh_system_helper()?;
            }
            if record.storage_ready && crate::read_lockfile(&record.server())?.is_some() {
                ensure!(
                    managed::invoke(&record, "repair", &[])? == 0,
                    "staged recovery failed; server remains stopped"
                );
            }
            if record.state == managed::State::Pending {
                if !record.storage_ready {
                    managed::resume_prepare(&mut record)?;
                }
                if crate::read_lockfile(&record.server())?.is_none() {
                    ensure!(
                        managed::invoke(&record, "install", &[])? == 0,
                        "installation retry failed; source and backup retained"
                    );
                }
                if record.original.is_some()
                    && !record.server().join(".hopper/adoption-complete").is_file()
                {
                    ensure!(
                        managed::invoke(&record, "restore", &[])? == 0,
                        "adoption retry failed; source retained"
                    );
                }
                if let Some(mut target) = record.onboarding_target.take() {
                    if record.scope == Scope::System {
                        target.file = record.source.file.clone();
                    } else if let (Some(file), Some(original)) = (&target.file, &record.original) {
                        if let Ok(relative) = file.canonicalize()?.strip_prefix(original) {
                            target.file = Some(record.server().join(relative));
                        }
                    }
                    record.source = target;
                }
                ensure!(
                    managed::invoke(&record, "bootstrap", &[])? == 0,
                    "bootstrap retry failed"
                );
                managed::refresh(&mut record)?;
            }
            let code = managed::invoke(&record, "repair", &[])?;
            ensure!(code == 0, "repair failed");
            managed::install_units(&record)?;
            if record.state == managed::State::Pending {
                record.state = managed::State::Ready;
                managed::save(&record)?;
            }
            crate::migration::finish_move(&record)?;
            Ok(0)
        }
        Disable { file, .. } | Enable { file, .. } => {
            let _guard = managed::operation_lock(&record)?;
            managed::invoke(
                &record,
                if matches!(action, Disable { .. }) {
                    "disable"
                } else {
                    "enable"
                },
                std::slice::from_ref(file),
            )
        }
        Show { plain, .. } => {
            let args = if *plain { vec!["plain".into()] } else { vec![] };
            managed::invoke(&record, "show", &args)
        }
        Chat { nickname, .. } => managed::invoke(
            &record,
            "chat",
            &nickname.iter().cloned().collect::<Vec<_>>(),
        ),
        Console { command, .. } => managed::invoke(&record, "console", command),
        Logs { args, .. } => managed::logs(&record, args),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn stop_helper_accepts_an_already_exited_main_process() {
        for args in [
            vec!["hopper", "__stop", "--dir", "/tmp/server", "--pid"],
            vec!["hopper", "__stop", "--dir", "/tmp/server"],
        ] {
            let app = App::try_parse_from(args).unwrap();
            let Some(Action::StopWorker { pid, .. }) = app.command else {
                panic!()
            };
            assert_eq!(pid, 0);
            assert_eq!(
                crate::service::stop_server(std::path::Path::new("/nonexistent"), pid).unwrap(),
                0
            );
        }
    }
    use super::*;
    #[test]
    fn explicit_syntax() {
        assert!(App::try_parse_from(["hopper", "some-pack"]).is_err());
        assert!(App::try_parse_from(["hopper", "--dir", "."]).is_err());
        assert!(
            App::try_parse_from([
                "hopper",
                "install",
                "x",
                "--provider",
                "gtnh",
                "--mods-only"
            ])
            .is_err()
        );
        let app = App::try_parse_from(["hopper", "install", "x", "--provider", "gtnh"]).unwrap();
        assert_eq!(app.scope, Scope::User);
        assert!(
            App::try_parse_from([
                "hopper",
                "install",
                "x",
                "--provider",
                "gtnh",
                "--channel",
                "testing",
                "--pack-version",
                "2.8.4"
            ])
            .is_err()
        );
    }
    #[test]
    fn journal_flags_require_separator() {
        let app = App::try_parse_from(["hopper", "logs", "x", "--", "-n", "20", "-f"]).unwrap();
        let Some(Action::Logs { args, .. }) = app.command else {
            panic!()
        };
        assert_eq!(args, ["-n", "20", "-f"]);
    }
}
