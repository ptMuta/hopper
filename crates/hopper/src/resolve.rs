//! Turning partial command lines into complete ones: from flags, from what can be inferred,
//! from a person at a terminal, or else one usage error naming everything that is missing.
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::interface::Scope;
use crate::managed::{self, Instance, State};
use crate::prompt::{self, Item, Missing};

/// What a command needs from the instance it acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Reads only: with a single candidate, it is taken without asking.
    Look,
    /// Changes something: always shown, even with one candidate.
    Change,
    /// Needs a running server.
    Running,
    /// Needs a stopped server.
    Stopped,
}

fn missing(args: &[&str], usage: &str) -> anyhow::Error {
    Missing {
        args: args.iter().map(|s| s.to_string()).collect(),
        usage: usage.into(),
    }
    .into()
}

/// `ActiveState` for many units in one `systemctl show`.
fn active_states(scope: Scope, units: &[String]) -> BTreeMap<String, String> {
    let mut states = BTreeMap::new();
    if units.is_empty() {
        return states;
    }
    let Ok(out) = managed::ctl(scope)
        .args(["show", "--property=Id,ActiveState"])
        .args(units)
        .output()
    else {
        return states;
    };
    for block in String::from_utf8_lossy(&out.stdout).split("\n\n") {
        let mut id = None;
        let mut state = None;
        for line in block.lines() {
            match line.split_once('=') {
                Some(("Id", v)) => id = Some(v.to_owned()),
                Some(("ActiveState", v)) => state = Some(v.to_owned()),
                _ => {}
            }
        }
        if let (Some(id), Some(state)) = (id, state) {
            states.insert(id, state);
        }
    }
    states
}

pub fn running(state: &str) -> bool {
    matches!(
        state,
        "active" | "activating" | "reloading" | "deactivating"
    )
}

/// What the picker says about an instance: `running · gtnh stable 2.8.4`.
fn describe(record: &Instance, state: &str) -> String {
    let sep = prompt::sep();
    let life = match record.state {
        State::Pending => "pending repair".to_owned(),
        State::Detached => "detached".to_owned(),
        State::Ready if running(state) => "running".to_owned(),
        State::Ready => "stopped".to_owned(),
    };
    let target = record
        .source
        .pack_version
        .clone()
        .or_else(|| record.source.channel.clone())
        .unwrap_or_else(|| "stable".into());
    let mut source = format!("{:?}", record.source.provider).to_lowercase();
    if let Some(pack) = &record.source.pack {
        source.push(' ');
        source.push_str(pack);
    }
    let installed = record
        .installed_version
        .as_deref()
        .map(|v| format!(" {v}"))
        .unwrap_or_default();
    format!("{life} {sep} {source} {target}{installed}")
}

/// Why `need` rules an instance out, if it does.
fn excluded(record: &Instance, state: &str, need: Need) -> Option<&'static str> {
    match need {
        Need::Running if record.state != State::Ready => Some("not ready"),
        Need::Running if !running(state) => Some("not running"),
        Need::Stopped if running(state) => Some("running"),
        _ => None,
    }
}

fn other_scope(scope: Scope) -> Scope {
    match scope {
        Scope::User => Scope::System,
        Scope::System => Scope::User,
    }
}

/// The instance a command acts on: named, or picked by a person.
pub fn instance(scope: Scope, name: Option<&str>, need: Need, command: &str) -> Result<Instance> {
    if let Some(name) = name {
        return managed::load(scope, name);
    }
    if !prompt::enabled() {
        return Err(missing(&["NAME"], &format!("hopper {command} NAME")));
    }
    let mut records = vec![];
    let mut broken = vec![];
    for entry in managed::list_lenient(scope)? {
        match entry {
            Ok(record) => records.push(record),
            Err((name, e)) => broken.push((name, e)),
        }
    }
    if records.is_empty() && broken.is_empty() {
        let elsewhere = managed::list_lenient(other_scope(scope))
            .map(|l| l.len())
            .unwrap_or(0);
        let mut message = format!("no instances in {} scope", scope.name());
        if elsewhere > 0 {
            message.push_str(&format!(
                "\nhelp: {elsewhere} {} instance{}; add --scope {}",
                other_scope(scope).name(),
                if elsewhere == 1 { "" } else { "s" },
                other_scope(scope).name()
            ));
        } else {
            message.push_str("\nhelp: create one with hopper install");
        }
        bail!(message);
    }
    let units: Vec<String> = records.iter().map(Instance::unit).collect();
    let states = active_states(scope, &units);
    let mut items: Vec<Item<String>> = records
        .iter()
        .map(|record| {
            let state = states.get(&record.unit()).map_or("", String::as_str);
            Item {
                label: record.name.clone(),
                hint: describe(record, state),
                value: record.name.clone(),
                disabled: excluded(record, state, need).map(str::to_owned),
            }
        })
        .collect();
    for (name, e) in broken {
        items.push(Item {
            label: format!("! {name}"),
            hint: String::new(),
            value: name,
            disabled: Some(format!("unreadable: {e}")),
        });
    }
    let usable: Vec<&Item<String>> = items.iter().filter(|i| i.disabled.is_none()).collect();
    match usable.as_slice() {
        [] => {
            let why = match need {
                Need::Running => "no instance is running".to_owned(),
                Need::Stopped => "every instance is running".to_owned(),
                _ => items
                    .iter()
                    .find_map(|i| i.disabled.clone())
                    .unwrap_or_default(),
            };
            bail!("nothing to {command}: {why}")
        }
        [only] if need == Need::Look && items.len() == 1 => {
            prompt::note("Instance", &format!("{} (only one)", only.label));
            managed::load(scope, &only.value)
        }
        _ => {
            let name = prompt::select("Instance", &items, None)?;
            managed::load(scope, &name)
        }
    }
}

/// Players online, asked of the server itself; `None` when it does not answer within a second.
pub fn players(record: &Instance) -> Option<u32> {
    let port = record.game_port?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(hopper_core::server::ping::ping("127.0.0.1", port).map(|s| s.online));
    });
    rx.recv_timeout(Duration::from_secs(1)).ok()?.ok()
}

/// `Stop horizons? 2 players online` — the question before interrupting a running server.
pub fn interrupt(verb: &str, record: &Instance) -> String {
    match players(record) {
        Some(0) => format!("{verb} {}? nobody online", record.name),
        Some(1) => format!("{verb} {}? 1 player online", record.name),
        Some(n) => format!("{verb} {}? {n} players online", record.name),
        None => format!("{verb} {}?", record.name),
    }
}

/// Installed mods that can be toggled, as (file name, currently enabled).
pub fn mods(server: &Path) -> Result<Vec<(String, bool)>> {
    let lock = crate::read_lockfile(server)?.context("no pack is installed here")?;
    let mut mods: Vec<(String, bool)> = lock
        .files
        .iter()
        .filter(|f| f.path.starts_with_dir("mods"))
        .map(|f| {
            let live = f.path.resolve_under(server).exists();
            (f.path.file_name().to_owned(), live)
        })
        .collect();
    mods.sort_by_key(|(name, _)| name.to_lowercase());
    Ok(mods)
}

/// The mods to toggle: named, or picked from those in the opposite state.
pub fn mod_files(record: &Instance, files: &[String], disable: bool) -> Result<Vec<String>> {
    if !files.is_empty() {
        return Ok(files.to_vec());
    }
    let verb = if disable { "disable" } else { "enable" };
    if !prompt::enabled() {
        return Err(missing(
            &["FILE"],
            &format!("hopper {verb} {} FILE...", record.name),
        ));
    }
    let items: Vec<Item<String>> = mods(&record.server())?
        .into_iter()
        .filter(|(_, enabled)| *enabled == disable)
        .map(|(name, _)| Item::new(name.clone(), name))
        .collect();
    if items.is_empty() {
        bail!(
            "no {} mods in {}",
            if disable { "enabled" } else { "disabled" },
            record.name
        );
    }
    let key = if disable { "Disable" } else { "Enable" };
    prompt::multiselect(key, &items, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_states() {
        assert!(running("active"));
        assert!(running("activating"));
        assert!(!running("inactive"));
        assert!(!running("failed"));
        assert!(!running(""));
    }
}

// ---------------------------------------------------------------- install and onboard

use crate::discover::{self, Discovery, Pick};
use crate::interface::{App, Create, Provider, Source, SourceArgs};
use std::path::PathBuf;

/// Everything `install`/`onboard` needs, settled.
#[derive(Debug, Clone)]
pub struct Plan {
    pub name: String,
    pub source: Source,
    pub from: Option<PathBuf>,
    /// Accepted by flag or by the person; still subject to the existing-EULA check.
    pub eula: bool,
    pub start: bool,
    /// Something was asked, so the equivalent command is worth showing.
    pub asked: bool,
}

/// Single-quote for a POSIX shell when needed.
pub fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c));
    if plain {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// A URL with its query and fragment cut, so signed tokens never reach a scrollback.
fn redact(url: &str) -> String {
    match url.find(['?', '#']) {
        Some(i) => format!("{}?...", &url[..i]),
        None => url.to_owned(),
    }
}

/// The command that does the same without asking anything.
pub fn command_line(
    app: &App,
    verb: &str,
    plan: &Plan,
    create: &Create,
    extra: &[String],
) -> String {
    let mut args: Vec<String> = vec!["hopper".into(), verb.into(), plan.name.clone()];
    let mut flag = |name: &str, value: Option<String>| {
        args.push(name.into());
        if let Some(v) = value {
            args.push(v);
        }
    };
    let s = &plan.source;
    flag(
        "--provider",
        Some(format!("{:?}", s.provider).to_lowercase()),
    );
    if let Some(v) = &s.pack {
        flag("--pack", Some(v.clone()));
    }
    if let Some(v) = &s.collection {
        flag("--collection", Some(v.clone()));
    }
    if let Some(v) = &s.file {
        let abs = v.canonicalize().unwrap_or_else(|_| v.clone());
        flag("--file", Some(abs.display().to_string()));
    }
    if let Some(v) = &s.url {
        flag("--url", Some(redact(v)));
    }
    if let Some(v) = &s.channel {
        flag("--channel", Some(v.clone()));
    }
    if let Some(v) = &s.pack_version {
        flag("--pack-version", Some(v.clone()));
    }
    if let Some(v) = &s.mc {
        flag("--mc", Some(v.clone()));
    }
    if let Some(v) = s.loader {
        flag(
            "--loader",
            Some(
                clap::ValueEnum::to_possible_value(&v)
                    .map_or_else(String::new, |p| p.get_name().to_owned()),
            ),
        );
    }
    let r = &create.runtime;
    if let Some(v) = &r.java {
        flag("--java", Some(v.display().to_string()));
    }
    if let Some(v) = r.java_major {
        flag("--java-major", Some(v.to_string()));
    }
    if let Some(v) = r.java_vendor {
        flag(
            "--java-vendor",
            Some(
                clap::ValueEnum::to_possible_value(&v)
                    .map_or_else(String::new, |p| p.get_name().to_owned()),
            ),
        );
    }
    if let Some(v) = &r.jvm_args_file {
        flag("--jvm-args-file", Some(v.display().to_string()));
    }
    if r.no_optional {
        flag("--no-optional", None);
    }
    for v in &r.force_include {
        flag("--force-include", Some(v.clone()));
    }
    for v in &r.force_exclude {
        flag("--force-exclude", Some(v.clone()));
    }
    if r.allow_client_pack {
        flag("--allow-client-pack", None);
    }
    if r.skip_blocked {
        flag("--skip-blocked", None);
    }
    if let Some(from) = &plan.from {
        let abs = from.canonicalize().unwrap_or_else(|_| from.clone());
        flag("--from", Some(abs.display().to_string()));
    }
    for v in extra {
        flag(v, None);
    }
    if plan.eula {
        flag("--eula", None);
    }
    if plan.start {
        flag("--start", None);
    }
    if create.rcon_firewall_confirmed {
        flag("--rcon-firewall-confirmed", None);
    }
    if app.scope == Scope::System {
        flag("--scope", Some("system".into()));
    }
    if app.dry_run {
        flag("-n", None);
    }
    args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

/// Show the equivalent command, dimmed, before anything runs.
pub fn echo(line: &str) {
    let paint = prompt::paint();
    eprintln!("\n  {}", paint.dim(&format!("$ {line}")));
}

/// A valid, unused instance name close to `seed`.
fn name_for(scope: Scope, seed: &str) -> String {
    let mut base: String = seed
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while base.contains("--") {
        base = base.replace("--", "-");
    }
    let mut base = base.trim_matches('-').to_owned();
    if !base.starts_with(|c: char| c.is_ascii_lowercase()) {
        base.insert_str(0, "mc-");
    }
    base.truncate(24);
    let base = base.trim_end_matches('-').to_owned();
    let taken = |n: &str| managed::record_path(scope, n).map_or(true, |p| p.exists());
    if !taken(&base) {
        return base;
    }
    for i in 2.. {
        let suffix = format!("-{i}");
        let mut candidate = base.clone();
        candidate.truncate(24 - suffix.len());
        candidate.push_str(&suffix);
        if !taken(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn provider_items() -> Vec<Item<Provider>> {
    vec![
        Item::new("modrinth", Provider::Modrinth).hint("Modrinth modpacks by slug"),
        Item::new("curseforge", Provider::Curseforge).hint("CurseForge modpacks by ID"),
        Item::new("gtnh", Provider::Gtnh).hint("GregTech: New Horizons"),
        Item::new("mrpack", Provider::Mrpack).hint("a .mrpack file or https URL"),
    ]
}

fn valid_pack(text: &str) -> Result<()> {
    anyhow::ensure!(
        !text.is_empty()
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "a slug or project ID: letters, digits, - and _"
    );
    Ok(())
}

/// The pack, by live search where the network may be used, else by typing it.
async fn ask_pack(
    found: &Option<Discovery>,
    provider: Option<Provider>,
    initial: &str,
) -> Result<Pick> {
    let typed = |provider: Provider| -> Result<Pick> {
        Ok(match provider {
            Provider::Gtnh => Pick::gtnh(),
            Provider::Mrpack => {
                let check = |t: &str| {
                    discover::literal(t)
                        .map(|_| ())
                        .context("an existing .mrpack file or an allowed https URL")
                };
                let text = prompt::text("Pack", None, &check)?;
                discover::literal(&text).context("not a pack file or URL")?
            }
            registry => {
                let pack = prompt::text("Pack", None, &valid_pack)?;
                Pick {
                    provider: registry,
                    pack: Some(pack.clone()),
                    file: None,
                    url: None,
                    slug: pack,
                }
            }
        })
    };
    let searchable = matches!(
        provider,
        None | Some(Provider::Modrinth | Provider::Curseforge)
    );
    let (Some(discovery), true) = (found, searchable) else {
        // Root may not touch the network, and mrpack has no catalogue: ask plainly.
        let provider = match provider {
            Some(p) => p,
            None => prompt::select("Provider", &provider_items(), Some(0))?,
        };
        return typed(provider);
    };
    let mut pinned = vec![];
    if provider.is_none() {
        pinned.push(
            Item::new("GT New Horizons", Pick::gtnh())
                .hint(format!("gtnh {} GregTech: New Horizons", prompt::sep())),
        );
    }
    let search = prompt::search::Search {
        key: "Pack",
        initial: initial.to_owned(),
        pinned,
        fetch: Box::new(move |query: String| {
            Box::pin(async move {
                Ok(discovery
                    .search(provider, &query)
                    .await?
                    .into_iter()
                    .map(|hit| {
                        let hint = hit.hint();
                        Item::new(hit.title.clone(), hit.pick).hint(hint)
                    })
                    .collect())
            })
        }),
        literal: Box::new(move |text: &str| {
            if let Some(pick) = discover::literal(text) {
                let label = pick
                    .file
                    .as_ref()
                    .map(|f| f.display().to_string())
                    .or_else(|| pick.url.clone())
                    .unwrap_or_default();
                return Some(Item::new(label, pick).hint("mrpack"));
            }
            // Offline, or a pack search does not surface: take the slug as typed.
            let registry = provider.unwrap_or(Provider::Modrinth);
            (text.len() >= 2 && valid_pack(text).is_ok()).then(|| {
                Item::new(
                    format!("use \"{text}\""),
                    Pick {
                        provider: registry,
                        pack: Some(text.to_owned()),
                        file: None,
                        url: None,
                        slug: text.to_owned(),
                    },
                )
                .hint(format!("{registry:?} slug as typed").to_lowercase())
            })
        }),
    };
    prompt::search::search(search).await
}

/// The release to track: `(channel, pin)`. Not asked when only one answer makes sense.
async fn ask_release(
    found: &Option<Discovery>,
    provider: Provider,
    pack: Option<&str>,
) -> Result<(Option<String>, Option<String>, usize)> {
    let sep = prompt::sep();
    let releases = match (found, provider, pack) {
        (Some(d), Provider::Gtnh, _) => {
            prompt::busy("Version");
            let r = d.releases(Provider::Gtnh, "").await;
            prompt::unbusy();
            r.unwrap_or_default()
        }
        (Some(d), p @ (Provider::Modrinth | Provider::Curseforge), Some(pack)) => {
            prompt::busy("Version");
            let r = d.releases(p, pack).await;
            prompt::unbusy();
            r?
        }
        _ if provider != Provider::Gtnh => return Ok((None, None, 0)),
        _ => vec![],
    };
    let stable = releases.iter().find(|r| r.stable);
    let testing = releases
        .iter()
        .find(|r| !r.stable && stable.is_none_or(|s| r.published > s.published));
    let label = |r: Option<&discover::Release>| r.map(|r| r.label.clone()).unwrap_or_default();
    let mut items = vec![];
    if stable.is_some() || provider == Provider::Gtnh {
        items.push(
            Item::new("stable", "stable").hint(format!("{} {sep} recommended", label(stable))),
        );
    }
    if testing.is_some() || (provider == Provider::Gtnh && releases.is_empty()) {
        items.push(
            Item::new("testing", "testing").hint(format!("{} {sep} prereleases", label(testing))),
        );
    }
    if provider == Provider::Gtnh {
        items.push(Item::new("daily", "daily").hint("development builds"));
        items.push(Item::new("experimental", "experimental").hint("development builds"));
    }
    if items.len() <= 1 {
        // Nothing newer than stable: there is no choice to make.
        return Ok(match items.first() {
            Some(_) => (None, None, 0),
            None if testing.is_some() => (Some("testing".into()), None, 0),
            None => bail!("this pack has no releases"),
        });
    }
    if !releases.is_empty() {
        items.push(Item::new("pin...", "pin").hint("one exact version, never updated"));
    }
    let choice = prompt::select("Version", &items, Some(0))?;
    if choice != "pin" {
        let channel = (choice != "stable").then(|| choice.to_owned());
        return Ok((channel, None, 1));
    }
    let versions: Vec<Item<String>> = releases
        .iter()
        .map(|r| {
            let kind = if r.stable { "stable" } else { "testing" };
            let mc = r.minecraft.as_deref().unwrap_or("");
            let date = r.published.get(..10).unwrap_or("");
            Item::new(r.label.clone(), r.id.clone()).hint(format!("{kind} {sep} {mc} {sep} {date}"))
        })
        .collect();
    match prompt::select("Pin", &versions, Some(0)) {
        Ok(pin) => Ok((None, Some(pin), 2)),
        Err(e) if e.is::<prompt::Back>() => {
            prompt::rewind(1);
            Err(e)
        }
        Err(e) => Err(e),
    }
}

fn valid_from(text: &str) -> Result<()> {
    let path = Path::new(text);
    anyhow::ensure!(
        path.join("server.properties").is_file(),
        "a server directory has server.properties"
    );
    Ok(())
}

/// Settle every input of `install`/`onboard`, asking a person for what is missing.
pub async fn install(app: &App, create: &Create, from: Option<Option<&Path>>) -> Result<Plan> {
    let onboarding = from.is_some();
    let from = from.flatten().map(Path::to_path_buf);
    let verb = if onboarding { "onboard" } else { "install" };
    if !prompt::enabled() {
        let mut args: Vec<String> = vec![];
        if create.name.is_none() {
            args.push("NAME".into());
        }
        if onboarding && from.is_none() {
            args.push("--from".into());
        }
        args.extend(create.source.missing().iter().map(|s| s.to_string()));
        if !args.is_empty() {
            return Err(Missing {
                args,
                usage: format!(
                    "hopper {verb} NAME{} --provider modrinth|curseforge|gtnh|mrpack [--pack ID] --eula\n      see hopper providers for what each provider needs",
                    if onboarding { " --from DIR" } else { "" }
                ),
            }
            .into());
        }
        return Ok(Plan {
            name: create.name.clone().unwrap_or_default(),
            source: create
                .source
                .source(create.source.provider.expect("checked above")),
            from,
            eula: create.eula,
            start: create.start,
            asked: false,
        });
    }

    // Lookups run as the invoking user; root never touches the registries.
    let found = if managed::is_root() {
        None
    } else {
        Discovery::new().ok()
    };
    let mut args = create.source.clone();
    let mut name = create.name.clone();
    let mut from = from;
    let mut eula = create.eula;
    let mut start = create.start;
    let mut slug = String::new();
    let mut asked = false;
    // Rows each step left on screen, so Esc can take them back.
    let mut rows = [0usize; 6];
    let mut step = 0;
    while step < rows.len() {
        let outcome: Result<usize> = match step {
            0 if onboarding && from.is_none() => {
                let here = std::env::current_dir()
                    .ok()
                    .filter(|d| d.join("server.properties").is_file())
                    .map(|d| d.display().to_string());
                prompt::text("From", here.as_deref(), &valid_from).map(|t| {
                    from = Some(PathBuf::from(t));
                    1
                })
            }
            1 if !args.identified() => {
                let initial = args.pack.clone().unwrap_or_default();
                ask_pack(&found, args.provider, &initial).await.map(|pick| {
                    slug = pick.slug.clone();
                    args.provider = Some(pick.provider);
                    args.pack = pick.pack;
                    args.file = pick.file;
                    args.url = pick.url;
                    1
                })
            }
            2 if !onboarding
                && args.channel.is_none()
                && args.pack_version.is_none()
                && args.collection.is_none() =>
            {
                let provider = args.provider.expect("pack step settles the provider");
                ask_release(&found, provider, args.pack.as_deref())
                    .await
                    .map(|(channel, pin, rows)| {
                        args.channel = channel;
                        args.pack_version = pin;
                        rows
                    })
            }
            3 if name.is_none() => {
                let seed = if !slug.is_empty() {
                    slug.clone()
                } else if let Some(dir) = from.as_ref().and_then(|f| f.file_name()) {
                    dir.to_string_lossy().into_owned()
                } else {
                    args.pack
                        .clone()
                        .or_else(|| args.provider.map(|p| format!("{p:?}")))
                        .unwrap_or_else(|| "server".into())
                };
                let default = name_for(app.scope, &seed);
                let scope = app.scope;
                let check = move |n: &str| {
                    managed::validate_name(n)?;
                    anyhow::ensure!(
                        !managed::record_path(scope, n)?.exists(),
                        "an instance named {n} exists"
                    );
                    Ok(())
                };
                prompt::text("Name", Some(&default), &check).map(|n| {
                    name = Some(n);
                    1
                })
            }
            4 if !eula && !app.dry_run && !from.as_deref().is_some_and(crate::eula_accepted) => {
                prompt::agree("EULA", "accept https://aka.ms/MinecraftEULA").map(|yes| {
                    eula = yes;
                    1
                })
            }
            5 if eula && !start && !app.dry_run => prompt::ask("Start", false).map(|yes| {
                start = yes;
                1
            }),
            _ => {
                rows[step] = 0;
                step += 1;
                continue;
            }
        };
        match outcome {
            Ok(n) => {
                rows[step] = n;
                asked = true;
                step += 1;
            }
            Err(e) if e.is::<prompt::Back>() => {
                // Back to the closest earlier question that was actually asked.
                match (0..step).rev().find(|&s| rows[s] > 0) {
                    Some(previous) => {
                        prompt::rewind(rows[previous]);
                        rows[previous] = 0;
                        // Forget its answer so it is asked again.
                        match previous {
                            0 => from = None,
                            1 => {
                                args.provider = create.source.provider;
                                args.pack = create.source.pack.clone();
                                args.file = create.source.file.clone();
                                args.url = create.source.url.clone();
                            }
                            2 => {
                                args.channel = None;
                                args.pack_version = None;
                            }
                            3 => name = None,
                            4 => eula = false,
                            _ => start = false,
                        }
                        step = previous;
                    }
                    None => return Err(prompt::Cancelled.into()),
                }
            }
            Err(e) => return Err(e),
        }
    }
    let provider = args.provider.context("no provider chosen")?;
    if provider == Provider::Curseforge && found.as_ref().is_some_and(|d| !d.has_curseforge()) {
        bail!("CurseForge needs an API key\nhelp: set CURSEFORGE_API_KEY, see hopper providers");
    }
    Ok(Plan {
        name: name.context("no name chosen")?,
        source: args.source(provider),
        from,
        eula,
        start,
        asked,
    })
}

/// The source for `versions`: complete, or asked for.
pub async fn versions_source(args: &SourceArgs) -> Result<Source> {
    if args.identified() {
        return Ok(args.source(args.provider.expect("identified")));
    }
    if !prompt::enabled() {
        return Err(Missing {
            args: args.missing().iter().map(|s| s.to_string()).collect(),
            usage: "hopper versions --provider modrinth|curseforge|gtnh --pack ID".into(),
        }
        .into());
    }
    anyhow::ensure!(!managed::is_root(), "run provider discovery without sudo");
    let pick = ask_pack(&Discovery::new().ok(), args.provider, "").await?;
    let mut args = args.clone();
    args.provider = Some(pick.provider);
    args.pack = pick.pack;
    args.file = pick.file;
    args.url = pick.url;
    Ok(args.source(pick.provider))
}

/// `search` at a terminal: pick a pack and get the command that installs it.
pub async fn search(provider: Option<Provider>, query: Option<&str>) -> Result<Option<String>> {
    if let (Some(_), Some(_)) = (provider, query) {
        return Ok(None);
    }
    if !prompt::enabled() {
        let mut args = vec![];
        if query.is_none() {
            args.push("QUERY".to_owned());
        }
        if provider.is_none() {
            args.push("--provider".to_owned());
        }
        return Err(Missing {
            args,
            usage: "hopper search QUERY --provider modrinth|curseforge".into(),
        }
        .into());
    }
    anyhow::ensure!(!managed::is_root(), "run provider discovery without sudo");
    let pick = ask_pack(&Discovery::new().ok(), provider, query.unwrap_or("")).await?;
    let name = name_for(Scope::User, &pick.slug);
    let mut line = vec![
        "hopper".to_owned(),
        "install".into(),
        name,
        "--provider".into(),
        format!("{:?}", pick.provider).to_lowercase(),
    ];
    if let Some(pack) = pick.pack {
        line.extend(["--pack".into(), pack]);
    }
    if let Some(file) = pick.file {
        line.extend(["--file".into(), file.display().to_string()]);
    }
    if let Some(url) = pick.url {
        line.extend(["--url".into(), url]);
    }
    Ok(Some(
        line.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "),
    ))
}

#[cfg(test)]
mod install_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn quoting_survives_a_shell() {
        assert_eq!(quote("atm9"), "atm9");
        assert_eq!(quote("/srv/my pack.mrpack"), "'/srv/my pack.mrpack'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote(""), "''");
        assert_eq!(
            redact("https://cdn.example/p.mrpack?token=secret"),
            "https://cdn.example/p.mrpack?..."
        );
    }

    #[test]
    fn names_are_derived_valid_and_unused() {
        let root = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded within this test's use of the variable.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", root.path()) };
        assert_eq!(name_for(Scope::User, "All The Mods 9"), "all-the-mods-9");
        assert_eq!(name_for(Scope::User, "9lives"), "mc-9lives");
        assert!(name_for(Scope::User, &"x".repeat(40)).len() <= 24);
        let dir = root.path().join("hopper/instances");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("atm9.json"), "{}").unwrap();
        assert_eq!(name_for(Scope::User, "atm9"), "atm9-2");
    }

    /// The echoed command parses back to the same settled source.
    #[test]
    fn the_echo_round_trips() {
        let app = App::try_parse_from(["hopper", "install"]).unwrap();
        let crate::interface::Action::Install(create) = app.command.as_ref().unwrap() else {
            panic!()
        };
        for source in [
            SourceArgs {
                provider: Some(Provider::Modrinth),
                pack: Some("adrenaserver".into()),
                ..Default::default()
            },
            SourceArgs {
                provider: Some(Provider::Gtnh),
                pack_version: Some("2.8.4".into()),
                ..Default::default()
            },
            SourceArgs {
                provider: Some(Provider::Curseforge),
                pack: Some("925200".into()),
                channel: Some("testing".into()),
                ..Default::default()
            },
        ] {
            let plan = Plan {
                name: "box".into(),
                source: source.source(source.provider.unwrap()),
                from: None,
                eula: true,
                start: true,
                asked: true,
            };
            let line = command_line(&app, "install", &plan, create, &[]);
            let argv: Vec<&str> = line.split(' ').collect();
            let parsed = App::try_parse_from(&argv).unwrap();
            let crate::interface::Action::Install(again) = parsed.command.unwrap() else {
                panic!()
            };
            assert_eq!(again.name.as_deref(), Some("box"));
            assert!(again.eula && again.start);
            assert_eq!(
                again.source.source(again.source.provider.unwrap()),
                plan.source,
                "{line}"
            );
        }
    }
}
