//! hopper — install and update Minecraft server modpacks from Modrinth.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;

use hopper_core::apply::{Summary, apply, next_lockfile, read_lockfile};
use hopper_core::cache::BlobStore;
use hopper_core::fs as hfs;
use hopper_core::model::{
    FileState, LOCK_VERSION, LockedFile, Lockfile, Managed, MinecraftVersion, RelPath,
    lock::{PackRecord, PolicyRecord, ServerRecord},
};
use hopper_core::net::{HostAllowlist, HttpClient};
use hopper_core::plan::{ConflictPolicy, DiskEntry, DiskState, reconcile};
use hopper_core::source::mrpack::{self, Mrpack};
use hopper_core::source::resolve::{Override, resolve_for_server};
use hopper_core::source::{SourceSpec, spec::SpecError};

mod chat;
mod cli;
mod curseforge;
mod discover;
#[cfg(test)]
mod engine_tests;
mod gtnh;
mod instances;
mod interface;
mod managed;
mod migration;
mod prompt;
mod render;
mod resolve;
mod runtime;
mod selfupdate;
mod service;
mod show;
mod style;

#[cfg(test)]
use cli::Command;
use cli::{Cli, exit};

fn main() {
    let cli = interface::App::parse();
    let code = match interface::run(&cli) {
        Ok(code) => code,
        Err(e) if e.is::<prompt::Cancelled>() || e.is::<prompt::Back>() => exit::CANCELLED,
        Err(e) => {
            eprintln!("error: {e}");
            for cause in e.chain().skip(1) {
                eprintln!("  caused by: {cause}");
            }
            exit_code_for(&e)
        }
    };
    std::process::exit(code);
}

/// `[--dir DIR | -d DIR | --dir=DIR] logs ARGS...` -> (dir, ARGS).
#[cfg(test)]
fn logs_invocation(args: Vec<String>) -> Option<(PathBuf, Vec<String>)> {
    let mut dir = PathBuf::from(".");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "logs" => return Some((dir, args[i + 1..].to_vec())),
            "-d" | "--dir" => {
                dir = PathBuf::from(args.get(i + 1)?);
                i += 2;
            }
            a if a.starts_with("--dir=") => {
                dir = PathBuf::from(&a["--dir=".len()..]);
                i += 1;
            }
            _ => return None,
        }
    }
    None
}

/// Map a failure to an exit code, so scripts can tell "try again later" from "this is
/// broken" and from "this was refused on security grounds".
fn exit_code_for(e: &anyhow::Error) -> i32 {
    use hopper_core::api::client::RegistryError;
    use hopper_core::api::curseforge::CurseForgeError;
    use hopper_core::net::HttpError;
    use hopper_core::source::MrpackError;
    use hopper_core::source::curseforge::PackError;

    fn http(e: &HttpError) -> i32 {
        match e {
            HttpError::Host { .. } | HttpError::Blob(_) => exit::SECURITY,
            _ if e.is_transient() => exit::NETWORK,
            _ => exit::GENERIC,
        }
    }
    fn pack(e: &MrpackError) -> i32 {
        match e {
            MrpackError::BadPath { .. }
            | MrpackError::BadUrl { .. }
            | MrpackError::SymlinkEntry(_)
            | MrpackError::EntryTooLarge { .. }
            | MrpackError::TotalTooLarge(_) => exit::SECURITY,
            _ => exit::GENERIC,
        }
    }
    fn curseforge(e: &CurseForgeError) -> i32 {
        match e {
            CurseForgeError::Http(h) => http(h),
            // Nothing to verify a download against is refused like a failed verification.
            CurseForgeError::NoSha1 { .. } => exit::SECURITY,
            _ => exit::GENERIC,
        }
    }

    if e.is::<prompt::Missing>() {
        return exit::USAGE;
    }
    if e.is::<prompt::Declined>() {
        return exit::DECLINED;
    }
    // The wrapper enums are matched explicitly: `#[error(transparent)]` forwards `source()`
    // past the wrapped error, so it never appears in the chain on its own.
    for cause in e.chain() {
        if let Some(h) = cause.downcast_ref::<HttpError>() {
            return http(h);
        }
        if let Some(RegistryError::Http(h)) = cause.downcast_ref::<RegistryError>() {
            return http(h);
        }
        if let Some(c) = cause.downcast_ref::<CurseForgeError>() {
            return curseforge(c);
        }
        if let Some(p) = cause.downcast_ref::<PackError>() {
            return match p {
                PackError::Archive(m) => pack(m),
                PackError::Manifest(c) => curseforge(c),
                PackError::MissingManifest => exit::GENERIC,
            };
        }
        if cause
            .downcast_ref::<hopper_core::cache::BlobError>()
            .is_some()
        {
            // A hash mismatch is a verification failure, not a flaky network.
            return exit::SECURITY;
        }
        if let Some(m) = cause.downcast_ref::<MrpackError>() {
            return pack(m);
        }
    }
    exit::GENERIC
}

#[tokio::main(flavor = "current_thread")]
#[cfg(test)]
async fn run(cli: &Cli) -> Result<i32> {
    let root = &cli.global.dir;

    match &cli.command {
        Some(Command::Status) => status(root),
        Some(Command::Disable { name }) => toggle(root, name, true),
        Some(Command::Enable { name }) => toggle(root, name, false),
        Some(Command::Repair) => repair(root),
        Some(Command::SelfUpdate { check, force }) => {
            selfupdate::run(&selfupdate::Options {
                check: *check || cli.global.dry_run,
                force: *force,
                yes: cli.global.yes,
                quiet: cli.global.quiet,
                user_agent: &user_agent(),
                cache: &cache_dir()?,
            })
            .await
        }
        Some(Command::Service(_)) => bail!("service orchestration belongs to the instance CLI"),
        Some(Command::Console { command }) => service::console(root, command),
        Some(Command::Show { plain }) => show::run(root, *plain),
        Some(Command::Chat { name }) => chat::run(root, name.as_deref()),
        Some(Command::Logs { .. }) => bail!("logs belong to the instance CLI"),
        Some(Command::Completions { shell }) => {
            let mut cmd = <Cli as clap::CommandFactory>::command();
            let mut script = Vec::new();
            clap_complete::generate(*shell, &mut cmd, "hopper", &mut script);
            // A reader that stops early (`| head`) is not an error worth a panic.
            let _ = std::io::Write::write_all(&mut std::io::stdout(), &script);
            Ok(exit::OK)
        }
        None => install(cli).await,
    }
}

// ---------------------------------------------------------------- status

fn status(root: &Path) -> Result<i32> {
    let Some(lock) = read_lockfile(root).context("reading the lockfile")? else {
        println!("No pack is installed in {}.", root.display());
        println!("\n  Install one:  hopper <pack>");
        return Ok(exit::OK);
    };

    let managed = lock.files.len();
    let disabled = lock.files.iter().filter(|f| f.disabled).count();
    let edited = lock
        .files
        .iter()
        .filter(|f| f.user_modified.is_some())
        .count();

    println!(
        "{}{}",
        lock.pack.name,
        lock.pack
            .version_label
            .as_ref()
            .map(|v| format!(" {v}"))
            .unwrap_or_default()
    );
    println!();
    println!("  Minecraft   {}", lock.server.minecraft);
    println!(
        "  Loader      {} {}",
        lock.server.loader, lock.server.loader_version
    );
    println!("  Java        {}", lock.server.java_major);
    println!("  Files       {managed} managed by hopper");
    if edited > 0 {
        println!("              {edited} edited by you and preserved");
    }
    if disabled > 0 {
        println!("              {disabled} disabled");
    }
    println!("  Updated     {}", lock.updated_at);
    println!("  Source      {}", lock.pack.source_arg);
    Ok(exit::OK)
}

// ---------------------------------------------------------------- disable / enable

/// Turn a mod off by renaming it, which reconcile honours permanently.
///
/// Deliberately not deletion: a deleted file is simply reinstalled by the next update, so
/// "remove this mod" has to be expressed as something the reconciler respects.
fn toggle(root: &Path, name: &str, disable: bool) -> Result<i32> {
    let lock = read_lockfile(root)
        .context("reading the lockfile")?
        .context("no pack is installed here")?;

    let suffix = ".disabled";
    let matches: Vec<&LockedFile> = lock
        .files
        .iter()
        .filter(|f| f.path.starts_with_dir("mods") && f.path.file_name().contains(name))
        .collect();

    let file = match matches.as_slice() {
        [] => bail!("no installed mod matches {name:?}"),
        [one] => *one,
        many => {
            eprintln!("error: {name:?} matches several mods:");
            for m in many {
                eprintln!("  {}", m.path.file_name());
            }
            eprintln!("\nhelp: use more of the filename to pick one");
            return Ok(exit::GENERIC);
        }
    };

    let live = file.path.resolve_under(root);
    let off = file
        .path
        .with_suffix(suffix)
        .context("building the disabled path")?
        .resolve_under(root);

    if disable {
        if !live.exists() {
            bail!("{} is already disabled", file.path.file_name());
        }
        hfs::rename(&live, &off).context("disabling the mod")?;
        println!("Disabled {}.", file.path.file_name());
        println!("  hopper will leave it off until you run: hopper enable {name}");
    } else {
        if !off.exists() {
            bail!("{} is not disabled", file.path.file_name());
        }
        hfs::rename(&off, &live).context("enabling the mod")?;
        println!("Enabled {}.", file.path.file_name());
    }
    println!("  Restart the server for this to take effect.");
    Ok(exit::OK)
}

// ---------------------------------------------------------------- repair

#[cfg(test)]
fn repair(root: &Path) -> Result<i32> {
    let journal = hopper_core::apply::execute::journal_path(root);
    if !journal.exists() {
        println!("Nothing to repair: the last run completed cleanly.");
        return Ok(exit::OK);
    }
    println!("A previous run was interrupted.");
    println!("  Re-running the install reconciles it automatically:");
    println!("    hopper");
    Ok(exit::OK)
}

// ---------------------------------------------------------------- install / update

async fn install(cli: &Cli) -> Result<i32> {
    let root = &cli.global.dir;
    let existing = read_lockfile(root).context("reading the lockfile")?;

    // A bare `hopper` replays whatever this directory was installed from.
    let source_arg = match (&cli.install.pack, &existing) {
        (Some(p), _) => p.clone(),
        (None, Some(lock)) => lock.pack.source_arg.clone(),
        (None, None) => {
            eprintln!("error: no pack given, and nothing is installed in this directory");
            eprintln!("\nhelp: install one with");
            eprintln!("        hopper <pack>");
            eprintln!("      for example");
            eprintln!("        hopper adrenaserver");
            return Ok(exit::GENERIC);
        }
    };

    let spec = match cli
        .install
        .source_override
        .clone()
        .map(Ok)
        .unwrap_or_else(|| SourceSpec::parse(&source_arg))
    {
        Ok(s) => s,
        Err(e @ SpecError::WrongProjectType(..)) => {
            eprintln!("error: {e}");
            eprintln!("\nhelp: hopper installs modpacks. Look for the pack that contains this,");
            eprintln!("      or point at a .mrpack file.");
            return Ok(exit::GENERIC);
        }
        Err(e) => bail!(e),
    };

    // A positional VERSION means the same as `pack@version`. Folded in here, once, so every
    // source honours it and a bare `hopper` later replays the same pin.
    let (spec, source_arg) = match (&cli.install.version, spec) {
        (
            Some(v),
            SourceSpec::Ambiguous {
                token,
                version: None,
            },
        ) => (
            SourceSpec::Ambiguous {
                token: token.clone(),
                version: Some(v.clone()),
            },
            format!("{token}@{v}"),
        ),
        (
            Some(v),
            SourceSpec::Pack {
                slug,
                version: None,
            },
        ) => (
            SourceSpec::Pack {
                slug: slug.clone(),
                version: Some(v.clone()),
            },
            format!("{slug}@{v}"),
        ),
        (
            Some(v),
            SourceSpec::CurseForge {
                slug,
                version: None,
            },
        ) => (
            SourceSpec::CurseForge {
                slug: slug.clone(),
                version: Some(v.clone()),
            },
            format!("cf:{slug}@{v}"),
        ),
        (
            Some(_),
            SourceSpec::Ambiguous { .. } | SourceSpec::Pack { .. } | SourceSpec::CurseForge { .. },
        ) => {
            eprintln!("error: the version is given twice, as pack@version and as an argument");
            return Ok(exit::GENERIC);
        }
        (Some(_), _) => {
            eprintln!("error: a version can only be given for a pack named by its slug");
            return Ok(exit::GENERIC);
        }
        (None, spec) => (spec, source_arg),
    };

    let cache = cache_dir()?;
    let store = BlobStore::new(&cache);
    // One client for both the API and the CDN it hands out URLs for; pack downloads are
    // still checked against the pack allowlist when they happen.
    let client =
        HttpClient::new(&user_agent(), HostAllowlist::api()).context("building the HTTP client")?;

    // 1. Resolve the source into a pack, keeping the archive bytes so overrides can be
    //    staged out of it.
    let wanted_mc = cli.install.mc.as_deref().map(MinecraftVersion::new);

    // A collection has no archive to download: it is a list of projects, and hopper picks
    // which versions of them to install. Everything downstream is identical, so it is turned
    // into a pack here rather than becoming a second code path.
    let cf_key = curseforge::api_key(cli.install.cf_api_key.as_deref());
    let cf_opts = curseforge::Options {
        api_key: cf_key.as_deref(),
        user_agent: &user_agent(),
        mc: wanted_mc.as_ref(),
        installed_project: existing
            .as_ref()
            .filter(|l| l.pack.registry == Some(hopper_core::model::RegistryId::CurseForge))
            .and_then(|l| l.pack.project_id.as_deref()),
        fallback_accepted: existing
            .as_ref()
            .is_some_and(|l| l.policy.client_pack_fallback),
        allow_client_pack: cli.install.allow_client_pack,
        skip_blocked: cli.install.skip_blocked,
        skip_blocked_remembered: existing.as_ref().is_some_and(|l| l.policy.skip_blocked),
        yes: cli.global.yes,
        dry_run: cli.global.dry_run,
        quiet: cli.global.quiet,
    };
    // Recorded so an update replays the registry that actually resolved the pack: a bare
    // token that fell through to CurseForge must not switch to Modrinth if the same slug
    // appears there later.
    let mut source_arg = source_arg;
    let mut cf_source: Option<curseforge::Resolved> = None;

    if let SourceSpec::Gtnh {
        channel,
        version,
        java_major,
    } = &spec
    {
        let native = gtnh::resolve(channel, version.as_deref(), *java_major, &store).await?;
        let mut native_cli = cli.clone();
        native_cli.install.native_java = Some(native.java);
        native_cli.install.native_launch = Some(native.launch);
        return finish_install(
            &native_cli,
            existing,
            source_arg,
            (native.pack, native.content),
            None,
            &store,
            &client,
        )
        .await;
    }

    // A collection has no archive to download: it is a list of projects, and hopper picks
    // which versions of them to install. Everything downstream is identical, so it is turned
    // into a pack here rather than becoming a second code path. CurseForge packs likewise.
    let (pack, override_content) = match &spec {
        SourceSpec::Collection { id } => {
            let pack = resolve_collection(
                &client,
                id,
                wanted_mc.as_ref(),
                cli.install.loader.map(Into::into),
                cli.global.quiet,
                cli.install.collection_channel.as_deref(),
            )
            .await?;
            (pack, Default::default())
        }
        SourceSpec::CurseForge { slug, version } => {
            let version = version.as_deref();
            match curseforge::resolve(slug, version, &store, &cf_opts, prompt::consent).await? {
                curseforge::Outcome::Declined => {
                    println!("Aborted. Nothing changed.");
                    return Ok(exit::DECLINED);
                }
                curseforge::Outcome::Resolved(r) => {
                    let r = *r;
                    let out = (r.pack.clone(), r.overrides.clone());
                    cf_source = Some(r);
                    out
                }
            }
        }
        _ => {
            let archive =
                match load_archive(&spec, &client, cli.global.quiet, wanted_mc.as_ref()).await {
                    Ok(a) => a,
                    // A bare slug Modrinth does not know may be a CurseForge pack.
                    Err(e) if is_not_found(&e) => {
                        let SourceSpec::Ambiguous { token, version } = &spec else {
                            return Err(e);
                        };
                        if cf_key.is_none() {
                            eprintln!("error: {e:#}\n");
                            eprintln!(
                                "help: if {token:?} is a CurseForge pack, set a CurseForge API key:"
                            );
                            curseforge::missing_key_help();
                            return Ok(exit::GENERIC);
                        }
                        if !cli.global.quiet {
                            println!("note: {token:?} is not on Modrinth; looking on CurseForge");
                        }
                        let version = version.as_deref();
                        match curseforge::resolve(token, version, &store, &cf_opts, prompt::consent)
                            .await?
                        {
                            curseforge::Outcome::Declined => {
                                println!("Aborted. Nothing changed.");
                                return Ok(exit::DECLINED);
                            }
                            curseforge::Outcome::Resolved(r) => {
                                let r = *r;
                                source_arg = match version {
                                    Some(v) => format!("cf:{token}@{v}"),
                                    None => format!("cf:{token}"),
                                };
                                let out = (r.pack.clone(), r.overrides.clone());
                                cf_source = Some(r);
                                return finish_install(
                                    cli, existing, source_arg, out, cf_source, &store, &client,
                                )
                                .await;
                            }
                        }
                    }
                    Err(e) => return Err(e),
                };
            let pack = mrpack::read(std::io::Cursor::new(&archive), &HostAllowlist::packs())
                .context("reading the modpack")?;
            let overrides =
                mrpack::stage_overrides(std::io::Cursor::new(&archive), &pack.overrides, &store)
                    .context("extracting the pack's config files")?;
            (pack, overrides)
        }
    };

    finish_install(
        cli,
        existing,
        source_arg,
        (pack, override_content),
        cf_source,
        &store,
        &client,
    )
    .await
}

/// Whether an error chain bottoms out in Modrinth saying the slug does not exist.
fn is_not_found(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<hopper_core::api::client::RegistryError>(),
            Some(hopper_core::api::client::RegistryError::NotFound { .. })
        )
    })
}

/// Everything after the source is resolved into a pack: classify, plan, confirm, apply.
async fn finish_install(
    cli: &Cli,
    existing: Option<Lockfile>,
    source_arg: String,
    (pack, override_content): (Mrpack, BTreeMap<RelPath, (hopper_core::model::Digest, u64)>),
    cf_source: Option<curseforge::Resolved>,
    store: &BlobStore,
    client: &HttpClient,
) -> Result<i32> {
    let root = &cli.global.dir;
    let store = store.clone();
    if !cli.global.quiet {
        println!(
            "{}{}",
            pack.index.name,
            pack.index
                .version_id
                .is_empty()
                .then(String::new)
                .unwrap_or_else(|| format!(" {}", pack.index.version_id))
        );
        println!(
            "Minecraft {}, {}, into {}\n",
            pack.index.minecraft,
            pack.index.loader,
            root.display()
        );
    }

    // 2. Classify for a server.
    //
    // The operator's rules are remembered, or a bare `hopper` from cron would reinstall the
    // client-only mod they excluded to make the server start. Passing a flag again replaces
    // the remembered list for that flag.
    let stored = existing.as_ref().map(|l| &l.policy);
    let pick = |given: &Vec<String>, kept: Option<&Vec<String>>| -> Vec<String> {
        if given.is_empty() {
            kept.cloned().unwrap_or_default()
        } else {
            given.clone()
        }
    };
    let includes = pick(&cli.install.force_include, stored.map(|p| &p.force_include));
    let excludes = pick(&cli.install.force_exclude, stored.map(|p| &p.force_exclude));
    let include_optional = !cli.install.no_optional && stored.is_none_or(|p| p.include_optional);
    if !cli.global.quiet {
        let remembered = |flag: &str, given: &Vec<String>, used: &Vec<String>| {
            if given.is_empty() && !used.is_empty() {
                println!("Remembered: {flag} {}", used.join(", "));
            }
        };
        remembered("--force-include", &cli.install.force_include, &includes);
        remembered("--force-exclude", &cli.install.force_exclude, &excludes);
        if !cli.install.no_optional && !include_optional {
            println!("Remembered: --no-optional");
        }
    }
    let (rule_includes, rule_excludes) = (includes.clone(), excludes.clone());
    let overrides = move |path: &RelPath| -> Option<Override> {
        let name = path.as_str();
        if rule_excludes.iter().any(|e| name.contains(e.as_str())) {
            Some(Override::Exclude)
        } else if rule_includes.iter().any(|i| name.contains(i.as_str())) {
            Some(Override::Include)
        } else {
            None
        }
    };
    let resolved = resolve_for_server(&pack, include_optional, &overrides, &override_content);

    if !cli.global.quiet && !resolved.access_lists.is_empty() {
        let names: Vec<&str> = resolved.access_lists.iter().map(RelPath::as_str).collect();
        println!(
            "note: the pack ships {}; access lists are yours alone and were not installed\n",
            names.join(", ")
        );
    }

    if !cli.global.quiet && resolved.skipped_count() > 0 {
        println!(
            "Environment: {} server files, {} skipped as client-only",
            resolved.desired.len(),
            resolved.skipped_count()
        );
        if cli.global.verbose {
            for s in &resolved.skipped {
                println!(
                    "  - {}  ({}, {:?})",
                    s.path, s.outcome.winner.note, s.outcome.confidence
                );
            }
        } else {
            for s in resolved.skipped.iter().take(3) {
                println!("  - {}  ({})", s.path, s.outcome.winner.note);
            }
            if resolved.skipped_count() > 3 {
                println!(
                    "  ... and {} more; run with -v to see them all",
                    resolved.skipped_count() - 3
                );
            }
        }
        println!();
    }

    // 3. Resolve the runtime: the vanilla jar and the loader's libraries. These become
    //    ordinary managed files, so a loader upgrade cleans up its own stale libraries/ tree
    //    through the same path that removes a dropped mod.
    //
    //    --mods-only skips all of it, for a directory that already has a working server.
    // Remembered across runs: someone who installed with --mods-only must not get a loader
    // and a JVM appear underneath them on the next bare `hopper`.
    let mods_only = cli.install.mods_only
        || (!cli.install.managed_launcher && existing.as_ref().is_some_and(|l| l.policy.mods_only));
    let runtime = if mods_only {
        None
    } else {
        Some(resolve_runtime(cli, &pack, &store).await?)
    };

    let mut desired = resolved.desired.clone();
    // The pack's default-server.properties seeds server.properties below rather than being
    // installed, so no mod can replace the operator's settings with it on first start.
    let defaults_path =
        RelPath::parse(hopper_core::server::PACK_DEFAULT_PROPERTIES).expect("a constant path");
    let pack_defaults = desired.remove(&defaults_path).and_then(|f| {
        let blob = store.get(&f.content).ok().flatten()?;
        std::fs::read_to_string(blob.path).ok()
    });
    let mut installer_pending = false;
    if let Some((rt, _)) = &runtime {
        for f in &rt.files {
            desired.insert(f.clone());
        }
        // A dry run does not run the installer. Hold the installed loader files where they
        // are, so the plan does not show them as removed.
        if rt.installer_pending {
            installer_pending = true;
            for f in existing.iter().flat_map(|l| &l.files) {
                if matches!(f.provenance, hopper_core::model::Provenance::Loader { .. }) {
                    desired.insert(hopper_core::plan::DesiredFile {
                        path: f.path.clone(),
                        content: f.digest.clone(),
                        size: Some(f.size),
                        executable: f.executable,
                        provenance: f.provenance.clone(),
                        managed: f.managed,
                    });
                }
            }
        }
    }

    // 4. Plan against what is on disk.
    let mut interesting: Vec<RelPath> = desired.paths().cloned().collect();
    if let Some(l) = &existing {
        interesting.extend(l.files.iter().map(|f| f.path.clone()));
    }
    let disk = scan(root, &interesting);
    let decisions = reconcile(existing.as_ref(), &disk, &desired, &world_policy(root)?);
    let summary = Summary::of(&decisions);
    let user_files = count_user_files(root, &existing);

    if !cli.global.quiet {
        print!("{}", render::plan(&decisions, &summary, user_files));
        if let Some(w) = render::removal_warning(&summary) {
            print!("{w}");
        }
    }

    // Accepting the EULA is independent of whether the pack needs changes: `hopper --eula`
    // on an already-current directory has to work, since that is how someone accepts it after
    // an unattended install.
    if cli.install.eula && !cli.global.dry_run && !cli.install.stage_only {
        accept_eula(root)?;
        if !cli.global.quiet {
            println!("  accepted the Minecraft EULA");
        }
    }

    let target_changed = existing.as_ref().is_some_and(|lock| {
        lock.pack.version_label.as_deref() != Some(pack.index.version_id.as_str())
            || cli
                .install
                .native_java
                .is_some_and(|major| major != lock.server.java_major)
    });
    if cli.install.pack_changes_only {
        // Putting back a missing file with the content it had is drift, not an update. A
        // restore with different content means the pack changed it too; so does everything
        // else that writes or deletes.
        let drift = |path: &RelPath, d: &hopper_core::plan::Decision| match d {
            hopper_core::plan::Decision::Add {
                restore: true,
                content,
            } => existing
                .as_ref()
                .and_then(|l| l.file(path))
                .is_some_and(|f| f.digest == *content),
            _ => false,
        };
        let pack_changed = decisions
            .iter()
            .any(|(path, d)| d.mutates() && !drift(path, d));
        return Ok(if pack_changed || installer_pending || target_changed {
            exit::CHANGES_PENDING
        } else {
            exit::OK
        });
    }
    if installer_pending {
        if !cli.global.quiet {
            println!(
                "\n  {} {} is not installed yet; a real run will run its installer",
                pack.index.loader,
                runtime
                    .as_ref()
                    .map_or("", |(rt, _)| rt.loader_version.as_str())
            );
        }
        return Ok(exit::CHANGES_PENDING);
    }
    if !summary.changes_anything() && !target_changed {
        return Ok(exit::OK);
    }
    if cli.global.dry_run {
        return Ok(exit::CHANGES_PENDING);
    }

    // 4. Confirm.
    if !cli.global.yes && !confirm("\nApply?")? {
        println!("Aborted. Nothing changed.");
        return Ok(exit::DECLINED);
    }

    // 5. Make sure every desired file is in the content store before touching the directory,
    //    so a failure here really does leave it unchanged.
    //
    //    Two kinds of file arrive differently: index entries are downloaded, while overrides
    //    were already staged out of the archive. Both must end up in the lockfile -- an
    //    installed file that is not recorded can never be updated or cleaned up afterwards.
    let mut entries: BTreeMap<RelPath, LockedFile> = BTreeMap::new();
    for (path, file) in desired.iter() {
        // Matched on content as well as path: an override can replace an index file at the same
        // path, and must not send us downloading the index file's URL for the override's bytes.
        let index_entry = pack.index.files.iter().find(|f| {
            f.path == *path && f.hashes.get(hopper_core::HashAlgo::Sha512) == Some(&file.content)
        });
        let (digest, size) = match index_entry {
            Some(index_file) => {
                let urls: Vec<String> = index_file
                    .downloads
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                let blob = client
                    .fetch_to_store(&urls, Some(&file.content), index_file.size, &store)
                    .await
                    .with_context(|| format!("downloading {path}"))?;
                (blob.digest, blob.size)
            }
            // An override or a runtime file: already staged into the store.
            None => (file.content.clone(), file.size.unwrap_or(0)),
        };

        entries.insert(
            path.clone(),
            LockedFile {
                path: path.clone(),
                digest,
                size,
                state: FileState::Managed,
                provenance: file.provenance.clone(),
                managed: Managed::Full,
                executable: false,
                user_modified: None,
                disabled: false,
                mtime_ns: None,
            },
        );
    }

    // 6. Apply, or save an immutable fully-downloaded plan for maintenance.
    let mut template = lockfile_template(&pack, &source_arg, existing.as_ref());
    template.policy.mods_only = mods_only;
    if let Some(r) = &cf_source {
        template.pack.registry = Some(r.registry);
        template.pack.project_id = Some(r.project_id.clone());
        template.pack.file_id = Some(r.file_id.clone());
        template.policy.client_pack_fallback = r.client_pack_fallback;
        template.policy.skip_blocked = r.skipped_blocked;
    }
    if let Some(SourceSpec::Pack { slug, version }) = &cli.install.source_override {
        template.pack.registry = Some(hopper_core::model::RegistryId::Modrinth);
        template.pack.project_id = Some(slug.clone());
        template.pack.file_id = version.clone();
    }
    if let Some(source) = &cli.install.source_override {
        template
            .unknown
            .insert("resolved_source".into(), serde_json::to_value(source)?);
    }
    template.policy.include_optional = include_optional;
    template.policy.force_include = includes;
    template.policy.force_exclude = excludes;
    if let Some((rt, _)) = &runtime {
        template.server.loader_version = rt.loader_version.clone();
        template.server.java_major = rt.java_major;
    }
    let start = match &runtime {
        Some((rt, java)) => Some(plan_start_script_at(
            root,
            rt,
            java,
            existing.as_ref(),
            if cli.install.managed_launcher {
                ".hopper-launch.sh"
            } else {
                "start.sh"
            },
        )?),
        None => None,
    };
    template.server.start_script = match &start {
        Some(StartScript::Write { digest, .. }) => Some(digest.clone()),
        Some(StartScript::KeepEdited { .. }) | None => existing
            .as_ref()
            .and_then(|l| l.server.start_script.clone()),
    };
    let next = next_lockfile(existing.as_ref(), &decisions, &entries, template.clone());
    if cli.install.stage_only {
        hfs::create_dir_all(&root.join(".hopper"))?;
        let prepared = PreparedInstall {
            template,
            entries,
            start,
            managed: cli.install.managed_launcher,
            gtnh: cli.install.native_java.is_some(),
            previous: existing.map(|l| l.to_json()).transpose()?,
            committed: None,
        };
        hfs::write_atomic(
            &root.join(".hopper/prepared.json"),
            &serde_json::to_vec(&prepared)?,
            false,
        )?;
        return Ok(exit::CHANGES_PENDING);
    }
    let txn = format!("{}", std::process::id());
    let applied = apply(
        root,
        &decisions,
        &next,
        &store,
        &txn,
        &format!("hopper {}", env!("CARGO_PKG_VERSION")),
        &now_rfc3339(),
    )
    .context("applying the plan")?;

    // 7. Bootstrap: one start command regardless of loader.
    if let Some(start) = &start {
        write_start_script(
            root,
            start,
            cli.global.quiet,
            cli.install.managed_launcher,
            cli.install.native_java.is_some(),
        )?;
    }

    // Recorded before choosing a port, so the next install on this machine steers clear of
    // this one's even while it is stopped.
    let _ = instances::register(root);
    let props = root.join("server.properties");
    if !props.exists() {
        // The pack's own port preference, if it has one, is where the search starts.
        let wanted = pack_defaults
            .as_deref()
            .and_then(|d| hopper_core::server::properties::get(d, "server-port"))
            .and_then(|p| p.parse().ok())
            .unwrap_or(hopper_core::server::ports::DEFAULT_SERVER_PORT);
        let port = instances::pick_server_port_from(root, wanted);
        hfs::write_atomic(
            &props,
            hopper_core::server::seed_server_properties(pack_defaults.as_deref(), port).as_bytes(),
            false,
        )
        .context("seeding server.properties")?;
        if port != wanted && !cli.global.quiet {
            println!(
                "\n  Port {wanted} is in use or belongs to another server here, so this one uses {port}."
            );
        }
    }

    if !cli.global.quiet {
        println!(
            "\n  wrote {} · removed {} · lockfile committed",
            applied.written.len(),
            applied.removed.len()
        );
        if !applied.new_files.is_empty() {
            println!(
                "\n  {} config(s) need your attention:",
                applied.new_files.len()
            );
            for f in &applied.new_files {
                println!("    {f} — compare with your version, then delete it");
            }
        }
        println!("\n{}: pack files are ready.", pack.index.name);
        if !cli.install.managed_launcher && !eula_accepted(root) {
            offer_eula(root, cli.global.yes)?;
        }
    }

    Ok(exit::OK)
}

// ---------------------------------------------------------------- helpers

#[derive(serde::Serialize, serde::Deserialize)]
struct PreparedInstall {
    template: Lockfile,
    entries: BTreeMap<RelPath, LockedFile>,
    start: Option<StartScript>,
    managed: bool,
    gtnh: bool,
    previous: Option<String>,
    #[serde(default)]
    committed: Option<String>,
}

/// Apply the staged snapshot without a second network resolution. Reconcile after
/// shutdown so edits during the warning period still receive normal protection.
fn apply_prepared(root: &Path, cache: &Path) -> Result<i32> {
    let mut prepared: PreparedInstall =
        serde_json::from_slice(&std::fs::read(root.join(".hopper/prepared.json"))?)?;
    let mut previous = read_lockfile(root)?;
    let current = previous.as_ref().map(|l| l.to_json()).transpose()?;
    let already_committed = prepared.committed.is_some() && current == prepared.committed;
    anyhow::ensure!(
        current == prepared.previous || already_committed,
        "lockfile changed after staging; recheck instead of applying a stale snapshot"
    );
    if !already_committed {
        let journal = hopper_core::apply::execute::journal_path(root);
        if let Some(bytes) = hfs::read_optional(&journal)? {
            let journal = hopper_core::apply::Journal::load(&bytes)?;
            if prepared.committed.as_deref() == Some(journal.next_lock.to_json()?.as_str()) {
                if let Some(previous) = &mut previous {
                    for intent in &journal.intended {
                        let path = intent.path.resolve_under(root);
                        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file())
                            && hfs::hash_file(&path).is_ok_and(|(_, d)| d == intent.digest)
                        {
                            if let Some(written) = journal.next_lock.file(&intent.path) {
                                previous.files.retain(|f| f.path != intent.path);
                                previous.files.push(written.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    let launcher = root.join(if prepared.managed {
        ".hopper-launch.sh"
    } else {
        "start.sh"
    });
    let original = prepared
        .previous
        .as_ref()
        .map(|s| Lockfile::load(s.as_bytes()))
        .transpose()?;
    let edited = original
        .as_ref()
        .and_then(|l| l.server.start_script.as_ref())
        .is_some_and(|digest| {
            hfs::hash_file(&launcher).is_ok_and(|(_, current)| current != *digest)
        });
    if edited {
        prepared.template.server.start_script = previous
            .as_ref()
            .and_then(|l| l.server.start_script.clone());
    }
    let mut desired = hopper_core::plan::DesiredSet::default();
    for file in prepared.entries.values() {
        desired.insert(hopper_core::plan::DesiredFile {
            path: file.path.clone(),
            content: file.digest.clone(),
            size: Some(file.size),
            executable: file.executable,
            provenance: file.provenance.clone(),
            managed: file.managed,
        });
    }
    let mut paths: Vec<_> = desired.paths().cloned().collect();
    if let Some(lock) = &previous {
        paths.extend(lock.files.iter().map(|f| f.path.clone()));
    }
    let decisions = reconcile(
        previous.as_ref(),
        &scan(root, &paths),
        &desired,
        &world_policy(root)?,
    );
    let next = next_lockfile(
        previous.as_ref(),
        &decisions,
        &prepared.entries,
        prepared.template.clone(),
    );
    if !already_committed {
        prepared.committed = Some(next.to_json()?);
        hfs::write_atomic(
            &root.join(".hopper/prepared.json"),
            &serde_json::to_vec(&prepared)?,
            false,
        )?;
        let store = BlobStore::new(cache);
        apply(
            root,
            &decisions,
            &next,
            &store,
            &std::process::id().to_string(),
            &format!("hopper {}", env!("CARGO_PKG_VERSION")),
            &now_rfc3339(),
        )?;
    }
    if let Some(start) = prepared.start {
        let start = if edited {
            match start {
                StartScript::Write { content, .. } | StartScript::KeepEdited { content } => {
                    StartScript::KeepEdited { content }
                }
            }
        } else {
            start
        };
        write_start_script(root, &start, false, prepared.managed, prepared.gtnh)?;
    }
    std::fs::remove_file(root.join(".hopper/prepared.json"))?;
    Ok(exit::OK)
}

fn world_policy(root: &Path) -> Result<ConflictPolicy> {
    let mut policy = ConflictPolicy::default();
    if let Ok(text) = std::fs::read_to_string(root.join("server.properties")) {
        if let Some(world) = hopper_core::server::properties::get(&text, "level-name") {
            RelPath::parse(world)
                .context("level-name must be a relative path within this instance")?;
            policy.protected_dirs.push(world.to_owned());
        }
    }
    Ok(policy)
}

/// Fetch the raw `.mrpack` bytes for whatever the operator named.
/// Fetch the raw `.mrpack` bytes for whatever the operator named.
async fn load_archive(
    spec: &SourceSpec,
    client: &HttpClient,
    quiet: bool,
    mc: Option<&MinecraftVersion>,
) -> Result<Vec<u8>> {
    let bytes = match spec {
        SourceSpec::File { path } => {
            hfs::read(Path::new(path)).with_context(|| format!("reading {path}"))?
        }
        SourceSpec::Url { url } => client
            .get_bytes(url.as_str())
            .await
            .with_context(|| format!("downloading {url}"))?,
        SourceSpec::Pack { slug, version } => {
            fetch_from_registry(client, slug, version.as_deref(), mc, quiet).await?
        }
        // A bare token is resolved as a pack first; collections come later.
        SourceSpec::Ambiguous { token, version } => {
            fetch_from_registry(client, token, version.as_deref(), mc, quiet).await?
        }
        SourceSpec::Collection { .. } | SourceSpec::CurseForge { .. } | SourceSpec::Gtnh { .. } => {
            unreachable!("handled before load_archive")
        }
        SourceSpec::SharedInstance { .. } => {
            bail!("shared instances are experimental and not wired up yet")
        }
    };
    Ok(bytes)
}

/// Look a pack up on Modrinth and download its `.mrpack`.
async fn fetch_from_registry(
    client: &HttpClient,
    slug: &str,
    version: Option<&str>,
    mc: Option<&MinecraftVersion>,
    quiet: bool,
) -> Result<Vec<u8>> {
    use hopper_core::api::client as registry;

    let project = registry::fetch_project(client, slug)
        .await
        .with_context(|| format!("looking up {slug:?} on Modrinth"))?;
    registry::require_modpack(&project, slug)?;

    let versions = registry::fetch_versions(client, project.id.as_str())
        .await
        .with_context(|| format!("listing versions of {slug:?}"))?;
    let chosen = registry::choose_version(&versions, slug, version, mc)?;
    let pack = registry::pack_file(&chosen, slug)?;

    if !quiet
        && !matches!(
            pack.version_type,
            Some(hopper_core::api::modrinth::VersionType::Release) | None
        )
    {
        // Worth saying out loud: the operator asked for a pack and is getting a prerelease
        // because no stable version exists.
        println!(
            "note: {} has no stable release; installing {} ({:?})",
            project.title, pack.version_number, pack.version_type
        );
    }

    let bytes = client
        .get_bytes(&pack.file_url)
        .await
        .with_context(|| format!("downloading {}", pack.file_name))?;
    verify_pack(&bytes, &pack).with_context(|| format!("verifying {}", pack.file_name))?;
    Ok(bytes)
}

/// Check a downloaded `.mrpack` against the hashes Modrinth published for it.
///
/// Every file the pack lists is verified, so the pack itself must be too: it decides which
/// files and hashes those are.
fn verify_pack(
    bytes: &[u8],
    pack: &hopper_core::api::client::PackVersion,
) -> Result<(), hopper_core::cache::BlobError> {
    use hopper_core::cache::BlobError;

    let mut hasher = hopper_core::MultiHasher::new();
    hasher.update(bytes);
    let (observed, _) = hasher.finish();
    if let Some(expected) = pack.hashes.strongest()
        && pack.hashes.verify(&observed) == Some(false)
    {
        return Err(BlobError::HashMismatch {
            expected: expected.clone(),
            actual: observed
                .get(expected.algo())
                .cloned()
                .unwrap_or_else(|| expected.clone()),
        });
    }
    if pack.size != 0 && pack.size != bytes.len() as u64 {
        return Err(BlobError::SizeMismatch {
            expected: pack.size,
            actual: bytes.len() as u64,
        });
    }
    Ok(())
}

fn scan(root: &Path, interesting: &[RelPath]) -> DiskState {
    use hopper_core::fs::FileKind;
    use hopper_core::plan::EntryKind;

    let mut disk = DiskState::new();

    // `<path>.disabled` has to be in the scan or reconcile cannot see the marker, and an
    // update would silently re-install a mod the operator deliberately switched off.
    let mut paths: Vec<RelPath> = interesting.to_vec();
    paths.extend(
        interesting
            .iter()
            .filter_map(|p| p.with_suffix(".disabled").ok()),
    );

    // Whatever is not a plain file is reported as what it is, so reconcile refuses it rather
    // than writing over or through it. A symlinked ancestor counts as a symlink: writing
    // `mods/x.jar` through `mods -> /srv/shared` would escape the directory being managed.
    let mut ancestor_links: std::collections::HashMap<String, bool> = Default::default();
    let mut through_link = |path: &RelPath| -> bool {
        let segments: Vec<&str> = path.as_str().split('/').collect();
        let mut prefix = String::new();
        for segment in &segments[..segments.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            let linked = *ancestor_links.entry(prefix.clone()).or_insert_with(|| {
                matches!(hfs::kind_of(&root.join(&prefix)), Ok(FileKind::Symlink))
            });
            if linked {
                return true;
            }
        }
        false
    };

    let other = |kind| DiskEntry {
        kind,
        digest: None,
        size: 0,
        executable: false,
        mtime_ns: None,
    };
    for path in &paths {
        let full = path.resolve_under(root);
        let kind = if through_link(path) {
            Ok(FileKind::Symlink)
        } else {
            hfs::kind_of(&full)
        };
        match kind {
            Ok(FileKind::File) => {
                if let Ok((_, digest)) = hfs::hash_file(&full) {
                    let size = std::fs::metadata(&full).map(|m| m.len()).unwrap_or(0);
                    disk.insert(path.clone(), DiskEntry::file(digest, size));
                }
            }
            Ok(FileKind::Symlink) => disk.insert(path.clone(), other(EntryKind::Symlink)),
            Ok(FileKind::Dir) => disk.insert(path.clone(), other(EntryKind::Dir)),
            Ok(FileKind::Other) => disk.insert(path.clone(), other(EntryKind::Other)),
            Ok(FileKind::Missing) | Err(_) => {}
        }
    }
    disk
}

/// Files in `mods/` that hopper does not manage.
fn count_user_files(root: &Path, lock: &Option<Lockfile>) -> usize {
    let Ok(entries) = std::fs::read_dir(root.join("mods")) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(path) = RelPath::parse(&format!("mods/{name}")) else {
                return false;
            };
            lock.as_ref().is_none_or(|l| l.file(&path).is_none())
        })
        .count()
}

/// Turn a collection into a pack.
///
/// Collections pin no versions and declare no Minecraft version or loader, so hopper has to
/// choose. Rather than demanding both up front, it solves for the pair covering the most
/// projects and shows the trade-off, including what would be left out.
async fn resolve_collection(
    client: &HttpClient,
    id: &str,
    wanted_mc: Option<&MinecraftVersion>,
    wanted_loader: Option<hopper_core::model::LoaderKind>,
    quiet: bool,
    channel: Option<&str>,
) -> Result<Mrpack> {
    use hopper_core::api::modrinth::{API_V3, WireCollection, WireVersion};
    use hopper_core::model::LoaderKind;
    use hopper_core::source::collection;
    use hopper_core::source::mrpack::{IndexFile, MrpackIndex};

    let collection: WireCollection = client
        .get_json(&format!("{API_V3}/collection/{id}"))
        .await
        .with_context(|| format!("fetching collection {id}"))?;

    if collection.projects.is_empty() {
        bail!("collection {:?} is empty", collection.name);
    }
    if !quiet {
        println!(
            "Collection \"{}\" ({} projects)",
            collection.name,
            collection.projects.len()
        );
    }

    // One request per project: collections have no batch version endpoint.
    let mut by_project = std::collections::BTreeMap::new();
    for project in &collection.projects {
        let mut versions: Vec<WireVersion> =
            hopper_core::api::client::fetch_versions(client, project.as_str())
                .await
                .with_context(|| format!("listing versions of {project}"))?;
        if let Some(channel) = channel {
            versions.retain(|v| {
                matches!(
                    (channel, v.version_type),
                    (
                        "stable",
                        Some(hopper_core::api::modrinth::VersionType::Release)
                    ) | (
                        "testing",
                        Some(
                            hopper_core::api::modrinth::VersionType::Beta
                                | hopper_core::api::modrinth::VersionType::Alpha
                        )
                    )
                )
            });
        }
        by_project.insert(project.clone(), versions);
    }

    let mc_candidates = match wanted_mc {
        Some(mc) => vec![mc.clone()],
        None => collection::candidate_minecraft_versions(&by_project),
    };
    let loaders = match wanted_loader {
        Some(l) => vec![l],
        None => vec![LoaderKind::Fabric, LoaderKind::NeoForge, LoaderKind::Quilt],
    };
    let ranked = collection::rank_targets(&by_project, &mc_candidates, &loaders);
    let best = ranked
        .first()
        .context("no usable target for this collection")?;

    if !quiet {
        println!(
            "  picked Minecraft {} with {}: {} of {} projects",
            best.minecraft, best.loader, best.covered, best.total
        );
        if !best.missing.is_empty() {
            // Project ids alone are unreadable, and resolving them to names would cost a
            // request each, so report the count and point at the collection.
            println!(
                "    {} project(s) have nothing for this combination",
                best.missing.len()
            );
        }
        // Say what the alternatives were, so the choice is reviewable rather than magic.
        for other in ranked.iter().skip(1).take(2) {
            println!(
                "  alternative: Minecraft {} with {} covers {} of {}",
                other.minecraft, other.loader, other.covered, other.total
            );
        }
        println!();
    }

    let chosen = collection::select_versions(&by_project, &best.minecraft, best.loader);
    let mut files = Vec::new();
    for version in chosen.into_values() {
        let resolved = match version.into_version() {
            Ok(v) => v,
            // A project with no usable file is skipped rather than failing the whole install.
            Err(_) => continue,
        };
        let filename = resolved.file.filename.clone();
        let path = hopper_core::model::RelPath::parse(&format!("mods/{filename}"))
            .with_context(|| format!("{filename:?} is not a usable filename"))?;
        files.push(IndexFile {
            path,
            hashes: resolved.file.hashes.clone(),
            downloads: vec![resolved.file.url.clone()],
            // Collections carry no environment data, so classification falls to the registry
            // metadata and the curated rules.
            env: None,
            size: Some(resolved.file.size),
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(Mrpack {
        index: MrpackIndex {
            name: collection.name.clone(),
            version_id: String::new(),
            summary: collection.description.clone(),
            minecraft: best.minecraft.clone(),
            loader: best.loader,
            loader_version: None,
            files,
        },
        overrides: vec![],
    })
}

/// Resolve the server jar, the loader and a JVM.
async fn resolve_runtime(
    cli: &Cli,
    pack: &Mrpack,
    store: &BlobStore,
) -> Result<(runtime::Runtime, hopper_core::java::JavaPlan)> {
    let client = HttpClient::new(&user_agent(), HostAllowlist::runtimes())
        .context("building the runtime HTTP client")?;
    let release = if cli.install.native_java.is_some() {
        // GTNH's patched bootstrap declares its own JVM compatibility, not Mojang 1.7.10.
        None
    } else {
        Some(runtime::resolve_minecraft(&client, &pack.index.minecraft).await?)
    };
    let major = cli
        .install
        .native_java
        .or_else(|| release.as_ref().map(|r| r.java_major))
        .context("missing runtime requirement")?;

    let java = runtime::plan_runtime_java(
        major,
        &pack.index.minecraft,
        cli.install.java.as_deref(),
        cli.install.java_vendor.map(Into::into),
    )?;
    describe_java(&java, major, cli.global.quiet);

    // Provision before anything is written to the server directory, so a failure here leaves
    // it untouched.
    let java = match java {
        // A dry run downloads no JVM; the plan above already said which one would be.
        plan @ hopper_core::java::JavaPlan::Provision { .. } if cli.global.dry_run => plan,
        hopper_core::java::JavaPlan::Provision { candidate } => {
            let data = runtime::data_dir()?;
            let installed = runtime::provision_java(&client, store, &candidate, &data).await?;
            if !cli.global.quiet {
                println!("      installed to {}", installed.path.display());
            }
            hopper_core::java::JavaPlan::UseExisting { java: installed }
        }
        other => other,
    };

    let java_path = match &java {
        hopper_core::java::JavaPlan::UseExisting { java } => Some(java.path.clone()),
        _ => None,
    };
    let cache_root = cache_dir()?;
    let rt = if let Some(launch) = &cli.install.native_launch {
        runtime::Runtime {
            files: vec![],
            launch: launch.clone(),
            java_major: major,
            loader_version: pack.index.loader_version.clone().unwrap_or_default(),
            installer_pending: false,
        }
    } else {
        runtime::resolve_loader(
            &client,
            store,
            pack.index.loader,
            pack.index.loader_version.as_deref(),
            &pack.index.minecraft,
            release.as_ref().context("missing Minecraft metadata")?,
            &runtime::LoaderEnv {
                java: java_path.as_deref(),
                cache_root: &cache_root,
                quiet: cli.global.quiet,
                run_installer: !cli.global.dry_run,
            },
        )
        .await?
    };
    Ok((rt, java))
}

/// Say what will happen about Java before anything is downloaded.
///
/// A surprise JDK download after someone typed "yes" to installing a modpack is the kind of
/// thing that makes a tool feel untrustworthy.
fn describe_java(plan: &hopper_core::java::JavaPlan, major: u32, quiet: bool) {
    if quiet {
        return;
    }
    match plan {
        hopper_core::java::JavaPlan::UseExisting { java } => {
            println!(
                "Java: using {} ({} {})",
                java.path.display(),
                java.vendor,
                java.version
            );
        }
        hopper_core::java::JavaPlan::Provision { candidate } => {
            println!(
                "Java: no suitable JVM found; this pack needs Java {major}.\n      \
                 hopper would install {} {}.",
                candidate.vendor, candidate.major
            );
        }
        hopper_core::java::JavaPlan::Unavailable { major, platform } => {
            println!(
                "Java: no Java {major} build is published for {:?} {:?}.",
                platform.os, platform.arch
            );
        }
    }
}

/// Emit `start.sh` and, on first install, `jvm.args`.
/// What to do about `start.sh`, decided before apply so the lockfile can record it.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
enum StartScript {
    Write {
        content: String,
        digest: hopper_core::model::Digest,
    },
    /// The operator edited the script. Theirs stays; ours goes beside it.
    KeepEdited { content: String },
}

/// Regenerate `start.sh` unless the operator has edited it since hopper last wrote it.
///
/// Its header promises exactly this, so an edit is detected by comparing the file with what
/// the lockfile says hopper wrote.
#[cfg(test)]
fn plan_start_script(
    root: &Path,
    runtime: &runtime::Runtime,
    java: &hopper_core::java::JavaPlan,
    previous: Option<&Lockfile>,
) -> Result<StartScript> {
    plan_start_script_at(root, runtime, java, previous, "start.sh")
}

fn plan_start_script_at(
    root: &Path,
    runtime: &runtime::Runtime,
    java: &hopper_core::java::JavaPlan,
    previous: Option<&Lockfile>,
    launcher: &str,
) -> Result<StartScript> {
    use hopper_core::java::JavaPlan;
    use hopper_core::server::{JavaLocation, start_script};

    let location = match java {
        JavaPlan::UseExisting { java } => JavaLocation::System {
            path: java.path.display().to_string(),
        },
        // Nothing has been provisioned yet, so fall back to whatever `java` the environment
        // provides; JAVA= in the script lets the operator point at a specific one.
        _ => JavaLocation::System {
            path: "java".into(),
        },
    };
    let args_path = RelPath::parse("jvm.args").expect("a constant path");
    let content = start_script(
        &runtime.launch,
        &location,
        Some(&args_path),
        env!("CARGO_PKG_VERSION"),
    );
    let mut hasher = hopper_core::MultiHasher::new();
    hasher.update(content.as_bytes());
    let (_, digest) = hasher.finish();

    let path = root.join(launcher);
    let on_disk = match hfs::kind_of(&path)? {
        hopper_core::fs::FileKind::Missing => None,
        hopper_core::fs::FileKind::File => Some(hfs::hash_file(&path)?.1),
        // A link or a directory is the operator's arrangement, never written through.
        _ => return Ok(StartScript::KeepEdited { content }),
    };
    let last_written = previous.and_then(|l| l.server.start_script.as_ref());
    Ok(match on_disk {
        None => StartScript::Write { content, digest },
        Some(d) if d == digest || Some(&d) == last_written => {
            StartScript::Write { content, digest }
        }
        // Either edited since we wrote it, or not ours at all (no record, different content).
        Some(_) => StartScript::KeepEdited { content },
    })
}

fn write_start_script(
    root: &Path,
    plan: &StartScript,
    quiet: bool,
    managed: bool,
    gtnh: bool,
) -> Result<()> {
    use hopper_core::server::{jvm_args_file, suggested_heap_gb};

    let jvm_args = root.join("jvm.args");
    if !jvm_args.exists() {
        // Seeded once, then the operator's file.
        let total_gb = total_memory_gb();
        hfs::write_atomic(
            &jvm_args,
            jvm_args_file(
                if gtnh {
                    suggested_heap_gb(total_gb).min(6)
                } else {
                    suggested_heap_gb(total_gb)
                },
                Some(total_gb),
            )
            .as_bytes(),
            false,
        )
        .context("writing jvm.args")?;
    }

    match plan {
        StartScript::Write { content, .. } => {
            hfs::write_atomic(
                &root.join(if managed {
                    ".hopper-launch.sh"
                } else {
                    "start.sh"
                }),
                content.as_bytes(),
                true,
            )
            .context("writing start.sh")?;
        }
        StartScript::KeepEdited { content } => {
            let beside = root.join(if managed {
                ".hopper-launch.sh.new"
            } else {
                "start.sh.new"
            });
            if hfs::kind_of(&beside)? == hopper_core::fs::FileKind::Missing
                || hfs::kind_of(&beside)? == hopper_core::fs::FileKind::File
            {
                hfs::write_atomic(&beside, content.as_bytes(), true)
                    .context("writing start.sh.new")?;
            }
            if !quiet {
                println!("\n  start.sh was edited, so it was left alone; the regenerated one");
                println!("  is in start.sh.new to compare against.");
            }
        }
    }
    Ok(())
}

/// Total system memory in gigabytes, for the default heap size.
fn total_memory_gb() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("MemTotal:"))
                .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        })
        .map(|kb| kb / 1024 / 1024)
        // A conservative default beats guessing high on an unknown platform.
        .unwrap_or(4)
}

fn accept_eula(root: &Path) -> Result<()> {
    hfs::write_atomic(
        &root.join("eula.txt"),
        hopper_core::server::eula_file(true).as_bytes(),
        false,
    )
    .context("writing eula.txt")?;
    Ok(())
}

/// Ask the operator to accept the EULA, when there is an operator to ask.
///
/// Never assumed: `--yes` and non-interactive runs only explain how, since agreeing to a
/// licence on someone's behalf because they skipped prompts is not defensible. Only an explicit
/// answer of yes accepts.
fn offer_eula(root: &Path, _yes: bool) -> Result<bool> {
    if !prompt::enabled() {
        println!("\n  note: the Minecraft EULA has not been accepted, so the server");
        println!("        will not start. Accept it with:  hopper --eula");
        println!("        {}", hopper_core::server::EULA_URL);
        return Ok(false);
    }
    if prompt::consent(&format!(
        "Accept the Minecraft EULA? {}",
        hopper_core::server::EULA_URL
    ))? {
        accept_eula(root)?;
        Ok(true)
    } else {
        println!("  Not accepted. When you are ready:  hopper --eula");
        Ok(false)
    }
}

fn eula_accepted(root: &Path) -> bool {
    hfs::read_optional(&root.join("eula.txt"))
        .ok()
        .flatten()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .as_deref()
        .is_some_and(hopper_core::server::eula_accepted)
}

fn lockfile_template(pack: &Mrpack, source_arg: &str, previous: Option<&Lockfile>) -> Lockfile {
    Lockfile {
        lock_version: LOCK_VERSION,
        generator: format!("hopper {}", env!("CARGO_PKG_VERSION")),
        updated_at: now_rfc3339(),
        server: ServerRecord {
            minecraft: MinecraftVersion::new(pack.index.minecraft.as_str()),
            loader: pack.index.loader,
            loader_version: pack.index.loader_version.clone().unwrap_or_default(),
            java_major: previous.map(|p| p.server.java_major).unwrap_or(21),
            start_script: None,
        },
        pack: PackRecord {
            name: pack.index.name.clone(),
            version_label: Some(pack.index.version_id.clone()),
            source_arg: source_arg.to_owned(),
            registry: None,
            project_id: None,
            file_id: None,
        },
        policy: PolicyRecord::default(),
        files: vec![],
        skipped: vec![],
        unknown: Default::default(),
    }
}

fn confirm(question: &str) -> Result<bool> {
    // Nobody to answer, and assuming yes would be the dangerous default.
    if !prompt::enabled() {
        println!(
            "{} not a terminal, so nothing was applied. Re-run with --yes.",
            question.trim()
        );
        return Ok(false);
    }
    prompt::ask(question.trim(), true)
}

fn cache_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("HOPPER_CACHE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context("could not determine a cache directory; set HOPPER_CACHE_DIR")?;
    Ok(base.join("hopper"))
}

fn user_agent() -> String {
    hopper_core::api::modrinth::user_agent(
        "ptMuta",
        env!("CARGO_PKG_VERSION"),
        "https://github.com/ptMuta/hopper",
    )
}

/// RFC3339 timestamp without pulling in a date library for one field.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Howard's civil-from-days algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    #[test]
    fn logs_arguments_pass_through_untouched() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (dir, args) = super::logs_invocation(v(&["logs", "-n", "3", "-f"])).unwrap();
        assert_eq!(dir, std::path::PathBuf::from("."));
        assert_eq!(args, ["-n", "3", "-f"]);
        let (dir, args) = super::logs_invocation(v(&["-d", "/srv/mc", "logs", "-q"])).unwrap();
        assert_eq!(dir, std::path::PathBuf::from("/srv/mc"));
        assert_eq!(args, ["-q"]);
        assert!(super::logs_invocation(v(&["--dir=/x", "logs"])).is_some());
        // Anything else is hopper's to parse.
        assert!(super::logs_invocation(v(&["-n", "logs"])).is_none());
        assert!(super::logs_invocation(v(&["adrenaserver"])).is_none());
    }

    fn runtime() -> super::runtime::Runtime {
        super::runtime::Runtime {
            files: vec![],
            launch: hopper_core::loader::LaunchProfile::ExecutableJar {
                jar: hopper_core::RelPath::parse("server.jar").unwrap(),
                jvm_args: vec![],
                program_args: vec!["nogui".into()],
            },
            java_major: 21,
            loader_version: String::new(),
            installer_pending: false,
        }
    }

    #[test]
    fn an_edited_start_script_is_left_alone() {
        use super::StartScript;
        let dir = tempfile::tempdir().unwrap();
        let java = hopper_core::java::JavaPlan::Unavailable {
            major: 21,
            platform: hopper_core::java::Platform {
                os: hopper_core::java::Os::Linux,
                arch: hopper_core::java::Arch::X64,
            },
        };
        let plan = || super::plan_start_script(dir.path(), &runtime(), &java, None).unwrap();

        // Absent: written.
        let StartScript::Write { content, .. } = plan() else {
            panic!("a missing start.sh is written");
        };
        // Unchanged since written: rewritten freely.
        std::fs::write(dir.path().join("start.sh"), &content).unwrap();
        assert!(matches!(plan(), StartScript::Write { .. }));
        // Edited, and no record says hopper wrote that: kept.
        std::fs::write(dir.path().join("start.sh"), format!("{content}\n# mine\n")).unwrap();
        assert!(matches!(plan(), StartScript::KeepEdited { .. }));
    }

    #[test]
    fn wrapped_failures_keep_their_exit_codes() {
        use hopper_core::api::curseforge::CurseForgeError;
        use hopper_core::net::{HostError, HttpError};
        let refused = || HttpError::Host {
            url: "https://evil.test/".into(),
            source: HostError::NoHost,
        };

        let e = anyhow::Error::from(CurseForgeError::Http(refused())).context("looking up x");
        assert_eq!(super::exit_code_for(&e), super::exit::SECURITY);

        let e = anyhow::Error::from(hopper_core::api::client::RegistryError::Http(refused()));
        assert_eq!(super::exit_code_for(&e), super::exit::SECURITY);

        let e = anyhow::Error::from(hopper_core::source::curseforge::PackError::Archive(
            hopper_core::source::MrpackError::SymlinkEntry("run.sh".into()),
        ));
        assert_eq!(super::exit_code_for(&e), super::exit::SECURITY);

        let e = anyhow::Error::from(CurseForgeError::NoSha1 { file: 1 });
        assert_eq!(super::exit_code_for(&e), super::exit::SECURITY);
    }

    use super::*;

    #[test]
    fn staged_recovery_finishes_launcher_after_lock_commit() {
        staged_launcher_recovery(true, false);
    }

    #[test]
    fn staged_apply_preserves_launcher_edited_during_warning() {
        staged_launcher_recovery(false, true);
    }

    fn staged_launcher_recovery(committed: bool, edited: bool) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("server");
        std::fs::create_dir_all(root.join(".hopper")).unwrap();
        let cache = temp.path().join("cache");
        let store = BlobStore::new(&cache);
        let old_script = "#!/bin/sh\nexec true\n";
        let new_script = "#!/bin/sh\n# updated launcher\nexec true\n";
        let old_digest = store
            .insert_bytes(old_script.as_bytes(), None)
            .unwrap()
            .digest;
        let new_digest = store
            .insert_bytes(new_script.as_bytes(), None)
            .unwrap()
            .digest;
        let pack = Mrpack {
            index: mrpack::MrpackIndex {
                name: "test".into(),
                version_id: "old".into(),
                summary: None,
                minecraft: MinecraftVersion::new("1.7.10"),
                loader: hopper_core::model::LoaderKind::Forge,
                loader_version: None,
                files: vec![],
            },
            overrides: vec![],
        };
        let mut old = lockfile_template(&pack, "test", None);
        old.server.start_script = Some(old_digest.clone());
        let mut next = old.clone();
        next.pack.version_label = Some("new".into());
        next.server.start_script = Some(new_digest.clone());
        let prepared = PreparedInstall {
            template: next.clone(),
            entries: BTreeMap::new(),
            start: Some(StartScript::Write {
                content: new_script.into(),
                digest: new_digest,
            }),
            managed: true,
            gtnh: false,
            previous: Some(old.to_json().unwrap()),
            committed: committed.then(|| next.to_json().unwrap()),
        };
        let current_script = if edited {
            "#!/bin/sh\n# operator edit\nexec true\n"
        } else {
            old_script
        };
        std::fs::write(root.join(".hopper-launch.sh"), current_script).unwrap();
        std::fs::write(
            root.join(".hopper/lock.json"),
            if committed {
                next.to_json().unwrap()
            } else {
                old.to_json().unwrap()
            },
        )
        .unwrap();
        std::fs::write(
            root.join(".hopper/prepared.json"),
            serde_json::to_vec(&prepared).unwrap(),
        )
        .unwrap();
        assert_eq!(apply_prepared(&root, &cache).unwrap(), 0);
        assert_eq!(
            std::fs::read_to_string(root.join(".hopper-launch.sh")).unwrap(),
            if edited { current_script } else { new_script }
        );
        if edited {
            assert_eq!(
                read_lockfile(&root).unwrap().unwrap().server.start_script,
                Some(old_digest)
            );
            assert!(root.join(".hopper-launch.sh.new").is_file());
        }
        assert!(!root.join(".hopper/prepared.json").exists());
    }

    #[test]
    fn timestamps_are_rfc3339() {
        let t = now_rfc3339();
        assert_eq!(t.len(), 20, "got {t}");
        assert!(t.ends_with('Z'));
        assert_eq!(t.as_bytes()[4], b'-');
        assert_eq!(t.as_bytes()[10], b'T');
    }

    #[test]
    fn civil_dates_match_known_values() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        // A leap day, which off-by-one errors in this algorithm tend to miss.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn security_refusals_get_their_own_exit_code() {
        // A caller must be able to tell "retry later" from "this pack is hostile".
        let host = anyhow::Error::new(hopper_core::net::HttpError::Host {
            url: "https://evil.test/x".into(),
            source: hopper_core::net::HostError::NoHost,
        });
        assert_eq!(exit_code_for(&host), exit::SECURITY);

        let transient = anyhow::Error::new(hopper_core::net::HttpError::Status {
            url: "https://cdn.modrinth.com/x".into(),
            status: 503,
        });
        assert_eq!(exit_code_for(&transient), exit::NETWORK);

        let missing = anyhow::Error::new(hopper_core::net::HttpError::Status {
            url: "https://cdn.modrinth.com/x".into(),
            status: 404,
        });
        assert_eq!(exit_code_for(&missing), exit::GENERIC);
    }

    #[test]
    fn a_traversing_pack_entry_is_a_security_exit() {
        let err = anyhow::Error::new(hopper_core::source::MrpackError::SymlinkEntry(
            "overrides/evil".into(),
        ));
        assert_eq!(exit_code_for(&err), exit::SECURITY);
    }

    #[test]
    fn the_user_agent_identifies_hopper_and_a_contact() {
        let ua = user_agent();
        assert!(ua.contains("hopper/"), "got {ua}");
        assert!(ua.contains("github.com"), "Modrinth ask for contact info");
    }
}
