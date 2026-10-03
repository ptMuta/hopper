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
