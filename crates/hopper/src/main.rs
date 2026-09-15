//! hopper — install and update Minecraft server modpacks from Modrinth.

use std::collections::BTreeMap;
use std::io::Write;
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

mod cli;
mod render;

use cli::{Cli, Command, exit};

fn main() {
    let cli = Cli::parse();
    let code = match run(&cli) {
        Ok(code) => code,
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

/// Map a failure to an exit code, so scripts can tell "try again later" from "this is
/// broken" and from "this was refused on security grounds".
fn exit_code_for(e: &anyhow::Error) -> i32 {
    for cause in e.chain() {
        if let Some(http) = cause.downcast_ref::<hopper_core::net::HttpError>() {
            return match http {
                hopper_core::net::HttpError::Host { .. } => exit::SECURITY,
                _ if http.is_transient() => exit::NETWORK,
                _ => exit::GENERIC,
            };
        }
        if cause
            .downcast_ref::<hopper_core::cache::BlobError>()
            .is_some()
        {
            // A hash mismatch is a verification failure, not a flaky network.
            return exit::SECURITY;
        }
        if let Some(pack) = cause.downcast_ref::<hopper_core::source::MrpackError>() {
            return match pack {
                hopper_core::source::MrpackError::BadPath { .. }
                | hopper_core::source::MrpackError::BadUrl { .. }
                | hopper_core::source::MrpackError::SymlinkEntry(_)
                | hopper_core::source::MrpackError::EntryTooLarge { .. }
                | hopper_core::source::MrpackError::TotalTooLarge(_) => exit::SECURITY,
                _ => exit::GENERIC,
            };
        }
    }
    exit::GENERIC
}

#[tokio::main(flavor = "current_thread")]
async fn run(cli: &Cli) -> Result<i32> {
    let root = &cli.global.dir;

    match &cli.command {
        Some(Command::Status) => status(root),
        Some(Command::Disable { name }) => toggle(root, name, true),
        Some(Command::Enable { name }) => toggle(root, name, false),
        Some(Command::Repair) => repair(root),
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
    println!("\n  Check for updates:  hopper -n");
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
            eprintln!("        hopper simply-optimized");
            return Ok(exit::GENERIC);
        }
    };

    let spec = match SourceSpec::parse(&source_arg) {
        Ok(s) => s,
        Err(e @ SpecError::WrongProjectType(..)) => {
            eprintln!("error: {e}");
            eprintln!("\nhelp: hopper installs modpacks. Look for the pack that contains this,");
            eprintln!("      or point at a .mrpack file.");
            return Ok(exit::GENERIC);
        }
        Err(e) => bail!(e),
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
    let archive = load_archive(&spec, &client, &source_arg, wanted_mc.as_ref()).await?;
    let pack = mrpack::read(std::io::Cursor::new(&archive), &HostAllowlist::packs())
        .context("reading the modpack")?;
    let override_content =
        mrpack::stage_overrides(std::io::Cursor::new(&archive), &pack.overrides, &store)
            .context("extracting the pack's config files")?;

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
    let includes = cli.install.force_include.clone();
    let excludes = cli.install.force_exclude.clone();
    let overrides = move |path: &RelPath| -> Option<Override> {
        let name = path.as_str();
        if excludes.iter().any(|e| name.contains(e.as_str())) {
            Some(Override::Exclude)
        } else if includes.iter().any(|i| name.contains(i.as_str())) {
            Some(Override::Include)
        } else {
            None
        }
    };
    let resolved = resolve_for_server(
        &pack,
        !cli.install.no_optional,
        &overrides,
        &override_content,
    );

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

    // 3. Plan against what is on disk.
    let mut interesting: Vec<RelPath> = resolved.desired.paths().cloned().collect();
    if let Some(l) = &existing {
        interesting.extend(l.files.iter().map(|f| f.path.clone()));
    }
    let disk = scan(root, &interesting);
    let decisions = reconcile(
        existing.as_ref(),
        &disk,
        &resolved.desired,
        &ConflictPolicy::default(),
    );
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
    if cli.install.eula {
        accept_eula(root)?;
        if !cli.global.quiet {
            println!("  accepted the Minecraft EULA");
        }
    }

    if !summary.changes_anything() {
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
    for (path, file) in resolved.desired.iter() {
        let (digest, size) = match pack.index.files.iter().find(|f| f.path == *path) {
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
            // An override: already in the store from stage_overrides.
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

    // 6. Apply.
    let template = lockfile_template(&pack, &source_arg, existing.as_ref());
    let next = next_lockfile(existing.as_ref(), &decisions, &entries, template);
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

    // 7. Bootstrap.
    let props = root.join("server.properties");
    if !props.exists() {
        hfs::write_atomic(
            &props,
            hopper_core::server::default_server_properties().as_bytes(),
            false,
        )
        .context("seeding server.properties")?;
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
        println!("\n{} is installed.", pack.index.name);
        if !eula_accepted(root) {
            println!("\n  note: the Minecraft EULA has not been accepted, so the server");
            println!("        will not start. Accept it with:  hopper --eula");
            println!("        {}", hopper_core::server::EULA_URL);
        }
    }

    Ok(exit::OK)
}

// ---------------------------------------------------------------- helpers

/// Fetch the raw `.mrpack` bytes for whatever the operator named.
/// Fetch the raw `.mrpack` bytes for whatever the operator named.
async fn load_archive(
    spec: &SourceSpec,
    client: &HttpClient,
    source_arg: &str,
    mc: Option<&MinecraftVersion>,
) -> Result<Vec<u8>> {
    let _ = source_arg;
    let bytes = match spec {
        SourceSpec::File { path } => {
            hfs::read(Path::new(path)).with_context(|| format!("reading {path}"))?
        }
        SourceSpec::Url { url } => client
            .get_bytes(url.as_str())
            .await
            .with_context(|| format!("downloading {url}"))?,
        SourceSpec::Pack { slug, version } => {
            fetch_from_registry(client, slug, version.as_deref(), mc).await?
        }
        // A bare token is resolved as a pack first; collections come later.
        SourceSpec::Ambiguous { token, version } => {
            fetch_from_registry(client, token, version.as_deref(), mc).await?
        }
        SourceSpec::Collection { .. } => bail!("collections are not wired up yet"),
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

    if !matches!(
        pack.version_type,
        Some(hopper_core::api::modrinth::VersionType::Release) | None
    ) {
        // Worth saying out loud: the operator asked for a pack and is getting a prerelease
        // because no stable version exists.
        println!(
            "note: {} has no stable release; installing {} ({:?})",
            project.title, pack.version_number, pack.version_type
        );
    }

    client
        .get_bytes(&pack.file_url)
        .await
        .with_context(|| format!("downloading {}", pack.file_name))
}

fn scan(root: &Path, interesting: &[RelPath]) -> DiskState {
    let mut disk = DiskState::new();
    for path in interesting {
        let full = path.resolve_under(root);
        if let Ok(hopper_core::fs::FileKind::File) = hfs::kind_of(&full)
            && let Ok((_, digest)) = hfs::hash_file(&full)
        {
            let size = std::fs::metadata(&full).map(|m| m.len()).unwrap_or(0);
            disk.insert(path.clone(), DiskEntry::file(digest, size));
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

fn accept_eula(root: &Path) -> Result<()> {
    hfs::write_atomic(
        &root.join("eula.txt"),
        hopper_core::server::eula_file(true).as_bytes(),
        false,
    )
    .context("writing eula.txt")?;
    Ok(())
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
        },
        pack: PackRecord {
            name: pack.index.name.clone(),
            version_label: Some(pack.index.version_id.clone()),
            source_arg: source_arg.to_owned(),
        },
        policy: PolicyRecord::default(),
        files: vec![],
        skipped: vec![],
        unknown: Default::default(),
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    // Non-interactive input cannot answer, and assuming yes would be the dangerous default.
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        println!("{prompt} not a terminal, so nothing was applied. Re-run with --yes.");
        return Ok(false);
    }
    print!("{prompt} [Y/n] ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let a = line.trim().to_ascii_lowercase();
    Ok(a.is_empty() || a == "y" || a == "yes")
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
    use super::*;

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
