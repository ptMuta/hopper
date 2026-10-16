//! Scope-aware instance storage and service orchestration. System pack work always runs
//! in a transient unprivileged service; only trusted coordination runs as root.
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use hopper_core::fs as hfs;
use hopper_core::server::systemd::Schedule;
use serde::{Deserialize, Serialize};

use crate::interface::{RuntimeOptions, Scope, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Ready,
    Detached,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub format: u32,
    pub name: String,
    pub scope: Scope,
    pub root: PathBuf,
    pub source: Source,
    pub runtime: RuntimeOptions,
    pub state: State,
    pub update: String,
    pub restart: String,
    pub warn: u32,
    pub started: bool,
    pub backup: Option<PathBuf>,
    pub original: Option<PathBuf>,
    #[serde(default)]
    pub onboarding_target: Option<Source>,
    #[serde(default)]
    pub storage_ready: bool,
    #[serde(default)]
    pub account_created: bool,
    #[serde(default)]
    pub game_port: Option<u16>,
    #[serde(default)]
    pub rcon_port: Option<u16>,
    #[serde(default)]
    pub listen_address: Option<String>,
    #[serde(default)]
    pub installed_version: Option<String>,
    #[serde(default = "crate::recovery::default_keep")]
    pub backup_keep: usize,
    #[serde(default = "crate::recovery::default_timeout")]
    pub startup_timeout: u64,
    #[serde(default)]
    pub backups: Vec<crate::recovery::Snapshot>,
    #[serde(default)]
    pub maintenance: Option<crate::recovery::Transaction>,
    #[serde(default)]
    pub last_maintenance: Option<String>,
}

impl Instance {
    pub fn new(scope: Scope, name: &str, source: Source, runtime: RuntimeOptions) -> Result<Self> {
        validate_name(name)?;
        Ok(Self {
            format: 1,
            name: name.into(),
            scope,
            root: data_root(scope)?.join(name),
            source,
            runtime,
            state: State::Pending,
            update: "off".into(),
            restart: "off".into(),
            warn: 300,
            started: false,
            backup: None,
            original: None,
            onboarding_target: None,
            storage_ready: false,
            account_created: false,
            game_port: None,
            rcon_port: None,
            listen_address: None,
            installed_version: None,
            backup_keep: crate::recovery::default_keep(),
            startup_timeout: crate::recovery::default_timeout(),
            backups: Vec::new(),
            maintenance: None,
            last_maintenance: None,
        })
    }
    pub fn server(&self) -> PathBuf {
        self.root.join("server")
    }
    pub fn account(&self) -> String {
        format!("hopper-{}", self.name)
    }
    pub fn unit(&self) -> String {
        format!("hopper-{}.service", self.name)
    }
    pub fn cache(&self) -> Result<PathBuf> {
        #[cfg(test)]
        if let Some(fake) = crate::maintenance_test::get() {
            return Ok(fake.root.join("cache"));
        }
        Ok(match self.scope {
            Scope::User => crate::cache_dir()?.join("instances").join(&self.name),
            Scope::System => PathBuf::from("/var/cache/hopper").join(&self.name),
        })
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 24
            && name.as_bytes()[0].is_ascii_lowercase()
            && name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'),
        "instance names must start with a lowercase letter and contain 1–24 lowercase letters, digits or hyphens"
    );
    Ok(())
}

pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}
pub fn data_root(scope: Scope) -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(fake) = crate::maintenance_test::get() {
        return Ok(fake.root.join("instances"));
    }
    Ok(match scope {
        Scope::User => std::env::var_os("HOPPER_INSTANCE_ROOT")
            .map(PathBuf::from)
            .unwrap_or(crate::runtime::data_dir()?.join("servers")),
        Scope::System => PathBuf::from("/var/lib/hopper"),
    })
}
fn config_root(scope: Scope) -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(fake) = crate::maintenance_test::get() {
        return Ok(fake.root.join("config"));
    }
    Ok(match scope {
        Scope::User => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .context("HOME/XDG_CONFIG_HOME is required")?
            .join("hopper/instances"),
        Scope::System => PathBuf::from("/etc/hopper/instances"),
    })
}
pub fn record_path(scope: Scope, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(config_root(scope)?.join(format!("{name}.json")))
}

pub fn load(scope: Scope, name: &str) -> Result<Instance> {
    let path = record_path(scope, name)?;
    if scope == Scope::System {
        trusted(&path, false)?;
    }
    let record: Instance = serde_json::from_slice(&fs::read(&path).with_context(|| {
        format!(
            "instance {name:?} not found in {} scope; use hopper list --scope {}",
            scope.name(),
            scope.name()
        )
    })?)?;
    ensure!(
        record.format == 1
            && record.name == name
            && record.scope == scope
            && record.root == data_root(scope)?.join(name),
        "invalid instance record"
    );
    ensure!(
        record.backup_keep > 0 && record.startup_timeout > 0,
        "backup retention and startup timeout must be positive"
    );
    record.source.validate()?;
    Ok(record)
}

/// Service units already carry an explicit server path. Their manager need not inherit
/// the HOPPER_INSTANCE_ROOT environment used when the instance was created.
pub(crate) fn record_for_server(scope: Scope, server: &Path) -> Result<Option<Instance>> {
    let Some(root) = server.parent() else {
        return Ok(None);
    };
    let Some(name) = root.file_name().and_then(|n| n.to_str()) else {
        return Ok(None);
    };
    if validate_name(name).is_err() {
        return Ok(None);
    }
    let path = record_path(scope, name)?;
    let Some(bytes) = hfs::read_optional(&path)? else {
        ensure!(
            root.parent() != Some(data_root(scope)?.as_path()),
            "managed instance record missing; start refused"
        );
        return Ok(None);
    };
    if scope == Scope::System {
        trusted(&path, false)?;
    }
    let record: Instance = serde_json::from_slice(&bytes)?;
    ensure!(
        record.format == 1
            && record.name == name
            && record.scope == scope
            && record.server() == server,
        "instance record does not match service directory"
    );
    record.source.validate()?;
    Ok(Some(record))
}
pub fn save(record: &Instance) -> Result<()> {
    let path = record_path(record.scope, &record.name)?;
    if record.scope == Scope::System {
        ensure!(is_root(), "system instance configuration requires root");
        trusted_dir(path.parent().unwrap())?;
    } else {
        fs::create_dir_all(path.parent().unwrap())?;
    }
    if path.exists() && record.scope == Scope::System {
        trusted(&path, false)?;
    }
    let bytes = serde_json::to_vec_pretty(record)?;
    if record.scope == Scope::User {
        hfs::write_private_atomic(&path, &bytes)?;
    } else {
        hfs::write_atomic(&path, &bytes, false)?;
    }
    fs::set_permissions(
        &path,
        fs::Permissions::from_mode(if record.scope == Scope::System {
            0o644
        } else {
            0o600
        }),
    )?;
    Ok(())
}
pub fn list(scope: Scope) -> Result<Vec<Instance>> {
    list_lenient(scope)?
        .into_iter()
        .map(|entry| entry.map_err(|(_, e)| e))
        .collect()
}

/// Every record, with unreadable ones reported by name instead of failing the whole listing.
/// A record, or the name of one that could not be read and why.
pub type Listed = std::result::Result<Instance, (String, anyhow::Error)>;

pub fn list_lenient(scope: Scope) -> Result<Vec<Listed>> {
    let root = config_root(scope)?;
    if !root.exists() {
        return Ok(vec![]);
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.extension().is_some_and(|s| s == "json") {
            let name = path
                .file_stem()
                .context("missing instance name")?
                .to_str()
                .context("invalid instance name")?;
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok(names
        .into_iter()
        .map(|name| load(scope, &name).map_err(|e| (name, e)))
        .collect())
}

pub fn preflight(scope: Scope) -> Result<()> {
    ensure!(
        cfg!(target_os = "linux"),
        "managed instances require Linux and systemd"
    );
    if scope == Scope::System {
        ensure!(is_root(), "system installs require sudo and --scope system");
    } else {
        ensure!(
            !is_root(),
            "user installs must run as the login user, not root; system installs require --scope system"
        );
    }
    let out = Command::new("/usr/bin/systemctl")
        .arg("--version")
        .output()?;
    let version = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u32>().ok())
        .context("cannot determine systemd version")?;
    ensure!(version >= 247, "systemd 247 or newer is required");
    let out = ctl(scope)
        .args(["show", "--property=Version", "--value"])
        .output()?;
    ensure!(
        out.status.success(),
        "systemd {} manager is unavailable; establish a user session or enable lingering before installation",
        scope.name()
    );
    Ok(())
}
fn trusted(path: &Path, directory: bool) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.uid() == 0
            && meta.mode() & 0o022 == 0
            && !meta.file_type().is_symlink()
            && if directory {
                meta.is_dir()
            } else {
                meta.is_file()
            },
        "{} must be root-owned, non-symlink and not group/world-writable",
        path.display()
    );
    Ok(())
}
fn trusted_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            trusted_dir(parent)?;
        }
    }
    if !path.exists() {
        fs::create_dir(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    trusted(path, true)
}

pub fn prepare(record: &mut Instance) -> Result<()> {
    ensure!(
        !record.root.exists(),
        "managed destination already exists; use onboarding/repair rather than overwrite"
    );
    ensure!(
        !unit_dir(record.scope)?.join(record.unit()).exists(),
        "service name already exists without this instance record; refusing to replace an unrelated unit"
    );
    // Persist intent before accounts/directories: a failed setup remains discoverable.
    save(record)?;
    resume_prepare(record)
}

pub fn prepare_backup_parent(path: &Path) -> Result<()> {
    ensure!(is_root(), "system backups require root coordination");
    trusted_dir(path)
}

pub fn refresh_system_helper() -> Result<()> {
    ensure!(is_root(), "root required to refresh the system helper");
    trusted_dir(Path::new("/usr/local/libexec"))?;
    let helper = Path::new("/usr/local/libexec/hopper");
    if helper.exists() {
        trusted(helper, false)?;
    }
    hfs::write_atomic(helper, &fs::read(std::env::current_exe()?)?, true)?;
    Ok(())
}

pub fn resume_prepare(record: &mut Instance) -> Result<()> {
    check_credentials(record.scope, &record.name, record.source.provider)?;
    ensure!(
        record.state == State::Pending && !record.storage_ready,
        "storage is already initialized"
    );
    assign_ports(record, record.game_port, record.rcon_port)?;
    if record.scope == Scope::System {
        preflight(Scope::System)?;
        trusted_dir(&data_root(Scope::System)?)?;
        trusted_dir(Path::new("/var/cache/hopper"))?;
        let exists = Command::new("/usr/bin/id")
            .arg(record.account())
            .output()?
            .status
            .success();
        ensure!(
            !exists || record.account_created,
            "service account already exists; refusing to reuse an unrelated account"
        );
        if !exists {
            checked(
                Command::new("/usr/sbin/useradd").args([
                    "--system",
                    "--user-group",
                    "--no-create-home",
                    "--home-dir",
                    record.root.to_str().context("invalid storage path")?,
                    "--shell",
                    "/usr/sbin/nologin",
                    &record.account(),
                ]),
                "creating dedicated service account",
            )?;
            record.account_created = true;
            save(record)?;
        }
        refresh_system_helper()?;
    }
    fs::create_dir_all(record.root.parent().context("missing storage parent")?)?;
    for path in [
        &record.root,
        &record.server(),
        &record.root.join("toolchains"),
        &record.cache()?,
    ] {
        if path.exists() {
            ensure!(
                fs::symlink_metadata(path)?.file_type().is_dir(),
                "unsafe storage directory {}",
                path.display()
            );
        } else {
            fs::create_dir_all(path)?;
        }
    }
    fs::set_permissions(&record.root, fs::Permissions::from_mode(0o700))?;
    if record.scope == Scope::System {
        for path in [
            &record.root,
            &record.server(),
            &record.root.join("toolchains"),
            &record.cache()?,
        ] {
            checked(
                Command::new("/usr/bin/chown")
                    .args([&format!("{}:{}", record.account(), record.account())])
                    .arg(path),
                "assigning instance storage",
            )?;
        }
    }
    if record.scope == Scope::System {
        if let Some(source) = record.source.file.clone() {
            let destination = record.root.join("source.mrpack");
            if source != destination {
                let mut input = File::open(source.canonicalize()?)?;
                let temporary = record.root.join("source.mrpack.copying");
                let mut output = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .mode(0o644)
                    .open(&temporary)?;
                std::io::copy(&mut input, &mut output)?;
                output.sync_all()?;
                fs::rename(temporary, &destination)?;
                record.source.file = Some(destination);
            }
        }
        trusted_dir(Path::new("/run/hopper"))?;
    }
    if let Some(args) = record.runtime.jvm_args_file.clone() {
        fs::copy(args, record.server().join("jvm.args"))?;
        if record.scope == Scope::System {
            checked(
                Command::new("/usr/bin/chown")
                    .arg(format!("{}:{}", record.account(), record.account()))
                    .arg(record.server().join("jvm.args")),
                "assigning JVM arguments",
            )?;
        }
    }
    record.storage_ready = true;
    store_credentials(record)?;
    save(record)
}

pub fn check_worker_owner(record: &Instance) -> Result<()> {
    ensure!(
        fs::metadata(&record.root)?.uid() == unsafe { libc::geteuid() },
        "worker is not running as this instance's owner"
    );
    Ok(())
}

pub fn ctl(scope: Scope) -> Command {
    let mut cmd = Command::new("/usr/bin/systemctl");
    if scope == Scope::User {
        cmd.arg("--user");
    }
    cmd
}
pub(crate) fn checked(cmd: &mut Command, action: &str) -> Result<()> {
    #[cfg(test)]
    if let Some(fake) = crate::maintenance_test::get() {
        return fake.control(cmd, action);
    }
    let status = cmd.status().with_context(|| action.to_owned())?;
    ensure!(status.success(), "{action} failed ({status})");
    Ok(())
}
pub fn property(record: &Instance, property: &str) -> Result<String> {
    #[cfg(test)]
    if let Some(fake) = crate::maintenance_test::get() {
        return fake.property(property);
    }
    let out = ctl(record.scope)
        .args(["show", &record.unit(), "--property", property, "--value"])
        .output()?;
    ensure!(out.status.success(), "could not read service property");
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}
fn executable(record: &Instance) -> Result<PathBuf> {
    Ok(if record.scope == Scope::System {
        PathBuf::from("/usr/local/libexec/hopper")
    } else {
        std::env::current_exe()?.canonicalize()?
    })
}

/// Execute network/archive/JVM work as the owner, with no shell or root execution.
fn worker_command(record: &Instance, operation: &str, args: &[String]) -> Result<Command> {
    if matches!(operation, "install" | "check" | "stage") {
        // systemd-run does not inherit the coordinator's environment. Refresh the
        // private credential before handing it to the worker via LoadCredential.
        store_credentials(record)?;
    }
    let executable = executable(record)?;
    let mut cmd = if record.scope == Scope::System {
        ensure!(is_root(), "system operations require sudo --scope system");
        trusted(&executable, false)?;
        let mut cmd = Command::new("/usr/bin/systemd-run");
        cmd.args(["--quiet", "--wait", "--pipe", "--collect"])
            .arg(format!("--uid={}", record.account()))
            .arg(format!("--gid={}", record.account()));
        for setting in hardening(&[record.root.clone(), record.cache()?])? {
            cmd.arg(format!("--property={setting}"));
        }
        cmd.arg(format!(
            "--setenv=HOPPER_DATA_DIR={}",
            record.root.display()
        ))
        .arg(format!(
            "--setenv=HOPPER_CACHE_DIR={}",
            record.cache()?.display()
        ))
        .arg("--setenv=HOPPER_NO_INPUT=1");
        let key = credential_path(record)?;
        if key.exists() {
            cmd.arg(format!(
                "--property=LoadCredential=curseforge:{}",
                key.display()
            ));
        }
        cmd.arg(executable);
        cmd
    } else {
        let mut cmd = Command::new(executable);
        cmd.env("HOPPER_DATA_DIR", &record.root)
            .env("HOPPER_CACHE_DIR", record.cache()?)
            .env("HOPPER_NO_INPUT", "1");
        // Workers must use the same registry root, not the toolchain data override.
        cmd.env("HOPPER_INSTANCE_ROOT", data_root(Scope::User)?);
        let key = credential_path(record)?;
        if key.exists() {
            cmd.env("HOPPER_CREDENTIAL_FILE", key);
        }
        cmd
    };
    // Keep registry lookup independent of toolchain provisioning location.
    if record.scope == Scope::System { /* system registry is fixed */ }
    cmd.args([
        "--scope",
        record.scope.name(),
        "__worker",
        &record.name,
        operation,
    ]);
    if crate::prompt::json_output() {
        cmd.arg("--json");
    }
    if !args.is_empty() {
        cmd.arg("--").args(args);
    }
    if operation == "restore" && record.scope == Scope::System {
        let path = record.backup.as_ref().context("missing backup")?;
        trusted_dir(path.parent().context("missing backup parent")?)?;
        trusted(path, false)?;
        ensure!(
            fs::metadata(path)?.mode() & 0o077 == 0,
            "backup must be private"
        );
        cmd.stdin(
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?,
        );
    }
    Ok(cmd)
}
pub fn invoke(record: &Instance, operation: &str, args: &[String]) -> Result<i32> {
    #[cfg(test)]
    if let Some(fake) = crate::maintenance_test::get() {
        return fake.invoke(record, operation, args);
    }
    if crate::prompt::json_output() {
        let output = worker_command(record, operation, args)?
            .output()
            .context("running instance worker")?;
        use std::io::Write;
        std::io::stderr().write_all(&output.stderr)?;
        return Ok(output.status.code().unwrap_or(1));
    }
    let status = worker_command(record, operation, args)?
        .status()
        .context("running instance worker")?;
    Ok(status.code().unwrap_or(1))
}
pub fn capture(record: &Instance, operation: &str) -> Result<String> {
    capture_args(record, operation, &[])
}

/// Review payloads can exceed the operating system's per-argument size limit.
pub(crate) fn invoke_input(record: &Instance, operation: &str, bytes: &[u8]) -> Result<i32> {
    #[cfg(test)]
    if let Some(fake) = crate::maintenance_test::get() {
        return fake.invoke(record, operation, &[std::str::from_utf8(bytes)?.to_owned()]);
    }
    use std::io::Write;
    let mut command = worker_command(record, operation, &[])?;
    command.stdin(std::process::Stdio::piped());
    if crate::prompt::json_output() {
        command.stdout(std::process::Stdio::null());
    }
    let mut child = command.spawn().context("starting instance worker")?;
    let written = child
        .stdin
        .take()
        .context("worker stdin unavailable")
        .and_then(|mut stdin| stdin.write_all(bytes).context("sending worker input"));
    let status = child.wait()?;
    written?;
    Ok(status.code().unwrap_or(1))
}

pub fn capture_args(record: &Instance, operation: &str, args: &[String]) -> Result<String> {
    #[cfg(test)]
    if let Some(fake) = crate::maintenance_test::get() {
        return fake.capture(record, operation, args);
    }
    let output = worker_command(record, operation, args)?.output()?;
    ensure!(
        output.status.success(),
        "instance {operation} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
pub fn refresh(record: &mut Instance) -> Result<()> {
    let info: serde_json::Value = serde_json::from_str(&capture(record, "inspect")?)?;
    record.game_port = info["game_port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok());
    record.rcon_port = info["rcon_port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok());
    record.listen_address = info["listen_address"].as_str().map(str::to_owned);
    record.installed_version = info["version"].as_str().map(str::to_owned);
    if record.source.provider == crate::interface::Provider::Gtnh
        && record.runtime.java_major.is_none()
    {
        record.runtime.java_major = info["java_major"]
            .as_u64()
            .and_then(|m| u32::try_from(m).ok());
    }
    if record.source.pack.is_some() {
        if let Some(id) = info["project_id"].as_str() {
            record.source.pack = Some(id.to_owned());
            record.source.validate()?;
        }
    }
    save(record)
}

/// Explicit ports are operator choices; allocate only missing ports.
pub fn assign_ports(record: &mut Instance, game: Option<u16>, rcon: Option<u16>) -> Result<()> {
    ensure!(game != Some(0) && rcon != Some(0), "ports must be nonzero");
    if game.is_some() && rcon.is_some() {
        record.game_port = game;
        record.rcon_port = rcon;
        return Ok(());
    }
    let mut records = list(record.scope)?;
    if record.scope == Scope::User {
        records.extend(list(Scope::System)?);
    }
    let mut taken = std::collections::BTreeSet::new();
    for other in records.into_iter().filter(|other| {
        other.state != State::Detached && (other.scope != record.scope || other.name != record.name)
    }) {
        taken.extend(other.game_port);
        taken.extend(other.rcon_port);
    }
    taken.extend(rcon);
    let game = hopper_core::server::ports::resolve(
        game,
        record.game_port.unwrap_or(25565),
        &taken,
        crate::instances::port_free,
    )
    .context("no free game port")?;
    taken.insert(game);
    let rcon = hopper_core::server::ports::resolve(
        rcon,
        record.rcon_port.unwrap_or(25575),
        &taken,
        crate::instances::port_free,
    )
    .context("no free RCON port")?;
    record.game_port = Some(game);
    record.rcon_port = Some(rcon);
    Ok(())
}

fn hardening(writable: &[PathBuf]) -> Result<Vec<String>> {
    let mut settings: Vec<String> = [
        "NoNewPrivileges=yes",
        "CapabilityBoundingSet=",
        "AmbientCapabilities=",
        "UMask=0077",
        "PrivateTmp=yes",
        "PrivateDevices=yes",
        "ProtectSystem=strict",
        "ProtectHome=yes",
        "ProtectKernelTunables=yes",
        "ProtectKernelModules=yes",
        "ProtectKernelLogs=yes",
        "ProtectControlGroups=yes",
        "RestrictSUIDSGID=yes",
        "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for path in writable {
        settings.push(format!(
            "ReadWritePaths={}",
            setting_path(path)?.replace("$$", "$")
        ));
    }
    Ok(settings)
}

fn unit_dir(scope: Scope) -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(fake) = crate::maintenance_test::get() {
        return Ok(fake.root.join("units"));
    }
    if scope == Scope::System {
        Ok(PathBuf::from("/etc/systemd/system"))
    } else {
        crate::service::unit_dir()
    }
}
pub fn validate_schedule(value: &str) -> Result<()> {
    if let Some(schedule) = Schedule::parse(value)? {
        ensure!(
            Command::new("/usr/bin/systemd-analyze")
                .args(["calendar", schedule.on_calendar()])
                .output()?
                .status
                .success(),
            "invalid systemd calendar expression"
        );
    }
    Ok(())
}
fn quote(value: &str) -> Result<String> {
    ensure!(
        !value.chars().any(char::is_control),
        "control character in unit argument"
    );
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    ))
}
fn setting_path(path: &Path) -> Result<String> {
    quote(path.to_str().context("non-UTF8 service path")?)
}
fn executable_path(path: &Path) -> Result<String> {
    // systemd does not perform environment expansion on the executable (argv[0]).
    Ok(setting_path(path)?.replace("$$", "$"))
}
fn working_directory(path: &Path) -> Result<String> {
    let value = path.to_str().context("non-UTF8 service path")?;
    ensure!(
        path.is_absolute() && !value.chars().any(char::is_control),
        "invalid service directory"
    );
    // This directive takes one raw path, not an ExecStart-style quoted argument list.
    Ok(value.replace('%', "%%"))
}
fn credential_path(record: &Instance) -> Result<PathBuf> {
    Ok(config_root(record.scope)?.join(format!("{}.curseforge-key", record.name)))
}
pub fn check_credentials(
    scope: Scope,
    name: &str,
    provider: crate::interface::Provider,
) -> Result<()> {
    if provider == crate::interface::Provider::Curseforge {
        api_key_for_instance(scope, name)?;
    }
    Ok(())
}

fn read_credential(path: &Path, scope: Scope, supplied: Option<String>) -> Result<String> {
    let supplied = supplied.filter(|k| !k.trim().is_empty());
    // Validate an existing destination even when replacing its contents.
    let stored = match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink() && meta.mode() & 0o077 == 0,
                "credential must be a private regular file"
            );
            if scope == Scope::System {
                trusted(path, false)?;
            }
            if supplied.is_none() {
                Some(fs::read_to_string(path).context("reading saved CurseForge credential")?)
            } else {
                None
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context("reading saved CurseForge credential"),
    };
    crate::curseforge::require_api_key(supplied.or(stored))
}

pub fn instance_api_key(record: &Instance) -> Result<String> {
    api_key_for_instance(record.scope, &record.name)
}

pub fn api_key_for_instance(scope: Scope, name: &str) -> Result<String> {
    validate_name(name)?;
    read_credential(
        &config_root(scope)?.join(format!("{name}.curseforge-key")),
        scope,
        crate::curseforge::api_key(None),
    )
}

pub fn store_credentials(record: &Instance) -> Result<()> {
    if record.source.provider != crate::interface::Provider::Curseforge {
        return Ok(());
    }
    let path = credential_path(record)?;
    let key = read_credential(&path, record.scope, crate::curseforge::api_key(None))?;
    if record.update == "off" && record.scope == Scope::User && !path.exists() {
        return Ok(());
    }
    write_credential(&path, record.scope, &key)
}

fn write_credential(path: &Path, scope: Scope, key: &str) -> Result<()> {
    if fs::read(path).ok().as_deref() == Some(key.as_bytes()) {
        return Ok(());
    }
    if scope == Scope::System {
        trusted_dir(path.parent().context("missing credential parent")?)?;
    }
    hfs::write_private_atomic(path, key.as_bytes())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub fn server_unit(record: &Instance, executable: &Path) -> Result<String> {
    let mut out = format!(
        "# Managed by hopper; use hopper configure.\n[Unit]\nDescription=Hopper instance {}\nAfter=network-online.target\nStartLimitIntervalSec=600\nStartLimitBurst=5\n\n[Service]\nType=simple\nWorkingDirectory={}\nExecStartPre={} --scope {} __start-check --dir {}\nExecStart={}\nExecStop={} --scope {} __stop --dir {} --pid $MAINPID\nKillSignal=SIGTERM\nTimeoutStopSec=180\nSuccessExitStatus=143\nRestart=on-failure\nRestartSec=15\nNoNewPrivileges=yes\nUMask=0077\n",
        record.name,
        working_directory(&record.server())?,
        executable_path(executable)?,
        record.scope.name(),
        setting_path(&record.server())?,
        executable_path(&record.server().join(".hopper-launch.sh"))?,
        executable_path(executable)?,
        record.scope.name(),
        setting_path(&record.server())?
    );
    if record.scope == Scope::System {
        out.push_str(&format!(
            "User={}\nGroup={}\n",
            record.account(),
            record.account()
        ));
        out.push_str(&hardening(&[record.server()])?.join("\n"));
        out.push('\n');
    }
    out.push_str(if record.scope == Scope::System {
        "\n[Install]\nWantedBy=multi-user.target\n"
    } else {
        "\n[Install]\nWantedBy=default.target\n"
    });
    Ok(out)
}

pub fn install_units(record: &Instance) -> Result<()> {
    install_units_impl(record, true)
}

pub(crate) fn install_units_offline(record: &Instance) -> Result<()> {
    install_units_impl(record, false)
}

fn install_units_impl(record: &Instance, refresh_credentials: bool) -> Result<()> {
    #[cfg(test)]
    if let Some(fake) = crate::maintenance_test::get() {
        return fake.install_units(record);
    }
    validate_schedule(&record.update)?;
    validate_schedule(&record.restart)?;
    if refresh_credentials {
        store_credentials(record)?;
    }
    let root = unit_dir(record.scope)?;
    if record.scope == Scope::System {
        ensure!(is_root(), "root required");
        trusted_dir(&root)?;
        refresh_system_helper()?;
    } else {
        fs::create_dir_all(&root)?;
    }
    let executable = executable(record)?;
    hfs::write_atomic(
        &root.join(record.unit()),
        server_unit(record, &executable)?.as_bytes(),
        false,
    )?;
    let combined = record.update != "off" && record.update == record.restart;
    for (kind, schedule, update, restart) in [
        ("update", &record.update, true, combined),
        ("restart", &record.restart, false, true),
    ] {
        let stem = format!("hopper-{}-{kind}", record.name);
        let enabled = Schedule::parse(schedule)?.is_some() && !(kind == "restart" && combined);
        if enabled {
            let mut maintenance = format!(
                "[Unit]\nDescription=Hopper maintenance {}\n\n[Service]\nType=oneshot\nExecStart={} --scope {} __maintenance {} {} {}\nTimeoutStartSec=infinity\nNoNewPrivileges=yes\nUMask=0077\n",
                record.name,
                executable_path(&executable)?,
                record.scope.name(),
                record.name,
                if update { "--update" } else { "" },
                if restart { "--restart" } else { "" }
            );
            if record.scope == Scope::System {
                maintenance.push_str("ProtectSystem=strict\nProtectHome=yes\nPrivateTmp=yes\nPrivateDevices=yes\nProtectKernelTunables=yes\nProtectKernelModules=yes\nProtectControlGroups=yes\nRestrictAddressFamilies=AF_UNIX\nCapabilityBoundingSet=\nReadWritePaths=/run/hopper /etc/hopper/instances /etc/systemd/system /usr/local/libexec\n");
            }
            let timer = format!(
                "[Unit]\nDescription=Hopper {kind} schedule {}\n\n[Timer]\nOnCalendar={}\nPersistent={}\nRandomizedDelaySec={}\n\n[Install]\nWantedBy=timers.target\n",
                record.name,
                Schedule::parse(schedule)?.unwrap().on_calendar(),
                if update { "true" } else { "false" },
                if combined || !update { "0" } else { "15min" }
            );
            hfs::write_atomic(
                &root.join(format!("{stem}.service")),
                maintenance.as_bytes(),
                false,
            )?;
            hfs::write_atomic(&root.join(format!("{stem}.timer")), timer.as_bytes(), false)?;
        } else {
            if root.join(format!("{stem}.timer")).exists() {
                checked(
                    ctl(record.scope).args(["disable", "--now", &format!("{stem}.timer")]),
                    "disabling maintenance timer",
                )?;
            }
            for extension in ["timer", "service"] {
                let path = root.join(format!("{stem}.{extension}"));
                if path.exists() {
                    fs::remove_file(path)?;
                }
            }
        }
    }
    checked(ctl(record.scope).arg("daemon-reload"), "reloading systemd")?;
    if record.started {
        activate_timers(record)?;
    } else {
        checked(
            ctl(record.scope).args(["disable", &record.unit()]),
            "leaving new service disabled",
        )?;
    }
    Ok(())
}

fn activate_timers(record: &Instance) -> Result<()> {
    for kind in ["update", "restart"] {
        let name = format!("hopper-{}-{kind}.timer", record.name);
        if unit_dir(record.scope)?.join(&name).exists() {
            checked(
                ctl(record.scope).args(["enable", "--now", &name]),
                "enabling maintenance timer",
            )?;
        }
    }
    Ok(())
}
pub fn start(record: &mut Instance) -> Result<()> {
    let _guard = operation_lock(record)?;
    ensure!(
        record.state == State::Ready,
        "instance is not ready; run repair or onboard it again"
    );
    let validate = record
        .maintenance
        .as_ref()
        .is_some_and(|m| m.phase == crate::recovery::Phase::AwaitingStart);
    ensure!(
        record.maintenance.is_none() || validate,
        "interrupted maintenance; run repair"
    );
    if validate {
        let token = fs::read(cancellation(record)?).ok();
        let maintenance = record
            .maintenance
            .as_mut()
            .context("missing startup validation")?;
        maintenance.phase = crate::recovery::Phase::Validating;
        maintenance.was_running = true;
        maintenance.cancellation = token;
        save(record)?;
    }
    let outcome: Result<()> = (|| {
        if validate {
            checked(
                ctl(record.scope).args(["reset-failed", &record.unit()]),
                "resetting startup failures",
            )?;
        }
        ensure!(
            invoke(record, "start-check", &[])? == 0,
            "start refused: resolve the .new conflicts listed above before starting"
        );
        if record.scope == Scope::System {
            ensure!(is_root(), "system start requires root");
        }
        checked(
            ctl(record.scope).args(["enable", "--now", &record.unit()]),
            "starting instance",
        )?;
        record.started = true;
        save(record)?;
        activate_timers(record)?;
        if validate {
            crate::recovery::wait_ready(record)?;
            crate::recovery::complete(record, "update succeeded; startup verified")?;
        }
        Ok(())
    })();
    if let Err(error) = outcome {
        if validate {
            checked(
                ctl(record.scope).args(["stop", &record.unit()]),
                "stopping failed update",
            )?;
            let id = record
                .maintenance
                .as_ref()
                .context("missing recovery backup")?
                .backup_id
                .clone();
            crate::recovery::rollback(record, &id, true)
                .context("automatic revert failed; run repair")?;
            return Err(error.context("startup failed; pre-update installation and world restored"));
        }
        return Err(error);
    }
    if record.scope == Scope::User {
        println!(
            "User service enabled. For startup without a login, enable lingering with loginctl enable-linger."
        );
    }
    Ok(())
}

fn lock_root(scope: Scope) -> Result<PathBuf> {
    let path = if scope == Scope::System {
        PathBuf::from("/run/hopper")
    } else {
        data_root(Scope::User)?.join(".locks")
    };
    if scope == Scope::System {
        ensure!(is_root(), "system coordination requires root");
        trusted_dir(&path)?;
    } else {
        fs::create_dir_all(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}
pub struct OperationLock(File);
pub fn operation_lock(record: &Instance) -> Result<OperationLock> {
    named_lock(record.scope, &record.name)
}
pub fn allocation_lock(scope: Scope) -> Result<OperationLock> {
    named_lock(scope, ".allocation")
}
fn named_lock(scope: Scope, name: &str) -> Result<OperationLock> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(lock_root(scope)?.join(format!("{name}.lock")))?;
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "another operation is running for {}; stop can cancel maintenance",
        name
    );
    Ok(OperationLock(file))
}
impl Drop for OperationLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
pub(crate) fn cancellation(record: &Instance) -> Result<PathBuf> {
    Ok(lock_root(record.scope)?.join(format!("{}.cancel", record.name)))
}
pub fn stop(record: &Instance) -> Result<()> {
    let marker = format!("{:?}", SystemTime::now());
    hfs::write_atomic(&cancellation(record)?, marker.as_bytes(), false)?;
    checked(
        ctl(record.scope).args(["stop", &record.unit()]),
        "stopping instance",
    )
}

/// A last question before maintenance goes ahead.
pub type Confirm<'a> = &'a dyn Fn(&Instance) -> Result<bool>;

/// `confirm` is asked once the update is staged and before players are warned, so what is
/// confirmed is what will be applied; timers pass `None`.
pub fn maintain(
    record: &Instance,
    update: bool,
    restart: bool,
    confirm: Option<Confirm>,
) -> Result<i32> {
    let mut owned = record.clone();
    let record = &mut owned;
    ensure!(record.state == State::Ready, "instance is not ready");
    ensure!(
        record.maintenance.is_none(),
        "pending update validation or recovery; start, revert, or repair first"
    );
    if update {
        check_credentials(record.scope, &record.name, record.source.provider)?;
    }
    let _guard = operation_lock(record)?;
    let cancellation = cancellation(record)?;
    let token = fs::read(&cancellation).ok();
    let mut changed = if update {
        match invoke(record, "check", &[])? {
            0 => false,
            10 => true,
            code => bail!("update check failed ({code}); service untouched"),
        }
    } else {
        false
    };
    let running = property(record, "ActiveState")? == "active";
    if !changed && !(restart && running) {
        record.last_maintenance = Some("no maintenance needed".into());
        save(record)?;
        if !crate::prompt::json_output() {
            println!("{}: no maintenance needed", record.name);
        }
        return Ok(0);
    }
    ensure!(
        invoke(record, "start-check", &[])? == 0,
        "maintenance refused: unresolved conflicts or incomplete recovery; service untouched"
    );
    if changed {
        let audit: crate::clean_update::Audit =
            serde_json::from_str(&capture(record, "stage-audit")?)?;
        if !crate::prompt::json_output() {
            audit.report();
        }
        let code = invoke(record, "stage", &[])?;
        ensure!(
            code == 0 || code == 10,
            "staging failed ({code}); service untouched"
        );
        changed = code == 10;
        if changed {
            if !crate::recovery::review(record)? {
                record.last_maintenance = Some("update skipped: unresolved file conflicts".into());
                save(record)?;
                return Ok(crate::cli::exit::CHANGES_PENDING);
            }
            ensure!(
                invoke(record, "snapshot-preflight", &[])? == 0,
                "backup preflight failed; service untouched"
            );
        }
    }
    if !changed && !restart {
        return Ok(0);
    }
    if let Some(confirm) = confirm.filter(|_| running) {
        if !confirm(record)? {
            println!(
                "Staged, not applied. Run hopper update {} when ready.",
                record.name
            );
            return Ok(crate::cli::exit::DECLINED);
        }
    }
    let pid = property(record, "MainPID")?;
    let cancelled = || -> bool { fs::read(&cancellation).ok() != token };
    if running {
        let mut remaining = record.warn;
        while remaining > 0 {
            ensure!(
                !cancelled()
                    && property(record, "ActiveState")? == "active"
                    && property(record, "MainPID")? == pid,
                "maintenance cancelled; server will not be restarted"
            );
            let message = format!(
                "say Server {} in {} seconds. Please reach a safe place.",
                if changed { "update/restart" } else { "restart" },
                remaining
            );
            ensure!(
                invoke(record, "rcon", &[message])? == 0,
                "warning delivery failed; service untouched"
            );
            let wait = remaining.min(if remaining > 60 {
                60
            } else if remaining > 10 {
                30
            } else {
                1
            });
            for _ in 0..wait {
                std::thread::sleep(Duration::from_secs(1));
                ensure!(!cancelled(), "maintenance cancelled");
            }
            remaining -= wait;
        }
        ensure!(
            !cancelled()
                && property(record, "ActiveState")? == "active"
                && property(record, "MainPID")? == pid,
            "maintenance cancelled; server changed during countdown"
        );
        ensure!(
            invoke(record, "rcon", &["save-all".into()])? == 0,
            "world save failed; service untouched"
        );
        ensure!(!cancelled(), "maintenance cancelled");
        checked(
            ctl(record.scope).args(["stop", &record.unit()]),
            "stopping before maintenance",
        )?;
        ensure!(
            property(record, "ActiveState")? == "inactive",
            "server did not stop; no changes applied"
        );
    }
    ensure!(!cancelled(), "maintenance cancelled; no changes applied");
    ensure!(
        property(record, "ActiveState")? == "inactive",
        "server started during maintenance; no changes applied"
    );
    if changed {
        if invoke(record, "stage-validate", &[])? != 0 {
            if running && !cancelled() {
                checked(
                    ctl(record.scope).args(["start", &record.unit()]),
                    "restarting unchanged server",
                )?;
            }
            bail!("files changed after review; update not applied; rerun update");
        }
        if let Err(error) = crate::recovery::begin(record, running, token.clone()) {
            if record.maintenance.is_some() {
                crate::recovery::repair(record)
                    .context("backup failed; recovery also failed; run repair")?;
            }
            return Err(error.context("update aborted before apply"));
        }
        let result: Result<()> = (|| {
            ensure!(!cancelled(), "maintenance cancelled before apply");
            ensure!(invoke(record, "apply", &[])? == 0, "applying update failed");
            refresh(record)?;
            crate::recovery::finish(record, running)
        })();
        if let Err(error) = result {
            checked(
                ctl(record.scope).args(["stop", &record.unit()]),
                "stopping failed update",
            )?;
            let id = record
                .maintenance
                .as_ref()
                .context("missing recovery snapshot")?
                .backup_id
                .clone();
            crate::recovery::rollback(record, &id, running)
                .context("automatic revert failed; server stopped; run repair")?;
            return Err(error.context("update failed; pre-update installation and world restored"));
        }
        return Ok(0);
    }
    if running && !cancelled() {
        ensure!(
            invoke(record, "start-check", &[])? == 0,
            "restart refused: unresolved .new conflicts; server remains stopped; review the files listed above before starting"
        );
        checked(
            ctl(record.scope).args(["start", &record.unit()]),
            "restarting after maintenance",
        )?;
    }
    Ok(0)
}

pub fn remove(record: &mut Instance) -> Result<()> {
    let server_unit = unit_dir(record.scope)?.join(record.unit());
    if server_unit.exists() {
        stop(record)?;
    }
    let _guard = operation_lock(record)?;
    for kind in ["update", "restart"] {
        let stem = format!("hopper-{}-{kind}", record.name);
        let timer = format!("{stem}.timer");
        if unit_dir(record.scope)?.join(&timer).exists() {
            checked(
                ctl(record.scope).args(["disable", "--now", &timer]),
                "disabling timer",
            )?;
        }
        for ext in ["timer", "service"] {
            let path = unit_dir(record.scope)?.join(format!("{stem}.{ext}"));
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
    }
    if server_unit.exists() {
        checked(
            ctl(record.scope).args(["disable", &record.unit()]),
            "disabling server",
        )?;
        fs::remove_file(server_unit)?;
    }
    checked(ctl(record.scope).arg("daemon-reload"), "reloading systemd")?;
    record.state = State::Detached;
    record.started = false;
    save(record)?;
    println!(
        "Removed service {}. Data, backups and service account retained at {}.",
        record.unit(),
        record.root.display()
    );
    Ok(())
}
pub fn logs(record: &Instance, args: &[String]) -> Result<i32> {
    let mut cmd = Command::new("/usr/bin/journalctl");
    if record.scope == Scope::User {
        cmd.arg("--user");
    }
    cmd.args(["--unit", &record.unit()]);
    if args.is_empty() {
        cmd.arg("--pager-end");
    } else {
        cmd.args(args);
    }
    Ok(cmd.status()?.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_managed_ports_need_no_allocation_or_privilege_probe() {
        let mut record = record(Scope::System);
        assert_eq!((record.game_port, record.rcon_port), (None, None));
        assign_ports(&mut record, Some(25565), Some(25575)).unwrap();
        assert_eq!(
            (record.game_port, record.rcon_port),
            (Some(25565), Some(25575))
        );
    }
    #[test]
    fn supplied_key_replaces_saved_key_and_is_available_without_environment() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("curseforge-key");
        hfs::write_private_atomic(&path, b"old-key").unwrap();
        let key = read_credential(&path, Scope::User, Some("new-key".into())).unwrap();
        assert_eq!(key, "new-key");
        write_credential(&path, Scope::User, &key).unwrap();
        assert_eq!(
            read_credential(&path, Scope::User, None).unwrap(),
            "new-key"
        );
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        fs::write(&path, [0xff]).unwrap();
        assert_eq!(
            read_credential(&path, Scope::User, Some("replacement-key".into())).unwrap(),
            "replacement-key"
        );
    }

    #[test]
    fn missing_empty_and_malformed_credentials_fail_without_writing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("curseforge-key");
        for supplied in [None, Some("".into()), Some(" \n".into())] {
            let error = read_credential(&path, Scope::User, supplied).unwrap_err();
            assert!(error.to_string().contains("CURSEFORGE_API_KEY"));
            assert!(!path.exists());
        }
        hfs::write_private_atomic(&path, b" \n").unwrap();
        assert!(read_credential(&path, Scope::User, None).is_err());
        hfs::write_private_atomic(&path, b"saved-key").unwrap();
        assert_eq!(
            read_credential(&path, Scope::User, Some(" ".into())).unwrap(),
            "saved-key"
        );
        let error = read_credential(&path, Scope::User, Some("secret\ninjection".into()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid CurseForge API key"));
        assert!(!error.contains("secret"));
        assert_eq!(fs::read(&path).unwrap(), b"saved-key");
    }

    #[test]
    fn credential_rotation_rejects_unsafe_existing_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("curseforge-key");
        let target = temp.path().join("target");
        hfs::write_private_atomic(&target, b"saved-key").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(read_credential(&path, Scope::User, Some("new-key".into())).is_err());
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"saved-key").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_credential(&path, Scope::User, Some("new-key".into())).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"saved-key");
    }

    fn record(scope: Scope) -> Instance {
        let source = crate::interface::App::try_parse_from([
            "hopper",
            "install",
            "world",
            "--provider",
            "gtnh",
        ])
        .unwrap();
        let Some(crate::interface::Action::Install(create)) = source.command else {
            panic!()
        };
        Instance::new(
            scope,
            "world",
            create.source.source(create.source.provider.unwrap()),
            create.runtime,
        )
        .unwrap()
    }
    use clap::Parser;
    #[test]
    fn strict_names_prevent_path_and_account_collisions() {
        for name in ["../x", "World", "x/y", "-x", "", "a\nExecStart=bad"] {
            assert!(validate_name(name).is_err());
        }
        assert!(validate_name("world-2").is_ok());
    }
    #[test]
    fn hardened_system_service_and_unprivileged_stop() {
        let unit = server_unit(
            &record(Scope::System),
            Path::new("/usr/local/libexec/hopper"),
        )
        .unwrap();
        for expected in [
            "User=hopper-world",
            "ProtectSystem=strict",
            "ProtectHome=yes",
            "CapabilityBoundingSet=",
            "WantedBy=multi-user.target",
            "__stop",
            "__start-check",
        ] {
            assert!(unit.contains(expected));
        }
        assert!(!unit.contains("MemoryDenyWriteExecute"));
        assert!(!unit.contains("ExecStop=+"));
    }
    #[test]
    fn user_services_do_not_hide_home_storage() {
        let unit = server_unit(&record(Scope::User), Path::new("/home/test/hopper")).unwrap();
        assert!(unit.contains("NoNewPrivileges=yes"));
        assert!(unit.contains("__start-check"));
        assert!(!unit.contains("ProtectHome"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn generated_server_units_pass_systemd_syntax_verification() {
        if !Path::new("/usr/bin/systemd-analyze").exists() {
            return;
        }
        for scope in [Scope::User, Scope::System] {
            let temp = tempfile::tempdir().unwrap();
            let mut instance = record(scope);
            instance.root = temp.path().join("instance space $ percent%");
            fs::create_dir_all(instance.server()).unwrap();
            let launcher = instance.server().join(".hopper-launch.sh");
            fs::write(&launcher, b"#!/bin/sh\nexec /usr/bin/true\n").unwrap();
            fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
            let file = temp.path().join(instance.unit());
            fs::write(
                &file,
                server_unit(&instance, Path::new("/usr/bin/true")).unwrap(),
            )
            .unwrap();
            let output = Command::new("/usr/bin/systemd-analyze")
                .arg("verify")
                .arg(file)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !String::from_utf8_lossy(&output.stderr).contains("Ignoring"),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
