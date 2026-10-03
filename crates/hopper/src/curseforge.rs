//! Turning a CurseForge modpack into a pack the rest of hopper installs like any other.
//!
//! The result is the same `Mrpack` plus staged overrides that a `.mrpack` produces, so planning,
//! applying and updating need no CurseForge-specific path. Two routes lead there:
//!
//! - **Server pack** (preferred): the author's own server directory, staged as server-layer
//!   overrides. No client-only guessing is needed, because the author already decided.
//! - **Client pack**, only when no server pack exists and the operator agreed: the manifest's
//!   files are resolved through the API and classified like a `.mrpack`'s, with the same
//!   heuristics and the same `--force-include`/`--force-exclude` escape hatches.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};

use hopper_core::api::curseforge::{self as cf, ReleaseType, WireFile, WireMod};
use hopper_core::cache::{Blob, BlobStore};
use hopper_core::model::{Digest, Hashes, MinecraftVersion, RegistryId, RelPath};
use hopper_core::net::{HostAllowlist, HttpClient};
use hopper_core::source::curseforge::{ClientPack, read_client_pack, read_server_pack};
use hopper_core::source::mrpack::{
    EnvSupport, FileEnv, IndexFile, Mrpack, MrpackIndex, stage_overrides,
};

/// The environment variable the API key is read from.
pub const KEY_ENV: &str = "CURSEFORGE_API_KEY";

pub struct Options<'a> {
    pub api_key: Option<&'a str>,
    pub user_agent: &'a str,
    pub mc: Option<&'a MinecraftVersion>,
    /// The CurseForge project this directory already has installed, if any. Choices remembered
    /// from earlier runs apply only to that project, never to a different pack.
    pub installed_project: Option<&'a str>,
    /// Remembered: the operator accepted the client-pack route for the installed project.
    pub fallback_accepted: bool,
    pub allow_client_pack: bool,
    pub skip_blocked: bool,
    /// Remembered: the operator chose to go without blocked files for the installed project.
    pub skip_blocked_remembered: bool,
    pub yes: bool,
    pub dry_run: bool,
    pub quiet: bool,
}

pub struct Resolved {
    pub pack: Mrpack,
    pub overrides: BTreeMap<RelPath, (Digest, u64)>,
    pub registry: RegistryId,
    pub project_id: String,
    pub file_id: String,
    /// Built from the client pack because no server pack exists.
    pub client_pack_fallback: bool,
    /// Blocked files were left out with the operator's consent.
    pub skipped_blocked: bool,
}

pub enum Outcome {
    Resolved(Box<Resolved>),
    /// The operator declined the client-pack route. Nothing was changed.
    Declined,
}

/// The API key from the flag or the environment.
pub fn api_key(flag: Option<&str>) -> Option<String> {
    flag.map(str::to_owned)
        .or_else(|| std::env::var(KEY_ENV).ok())
        .or_else(|| {
            std::env::var_os("CREDENTIALS_DIRECTORY").and_then(|dir| {
                std::fs::read_to_string(std::path::PathBuf::from(dir).join("curseforge")).ok()
            })
        })
        .or_else(|| {
            std::env::var_os("HOPPER_CREDENTIAL_FILE")
                .and_then(|path| std::fs::read_to_string(path).ok())
        })
        .filter(|k| !k.trim().is_empty())
}

pub fn missing_key_help() {
    eprintln!("help: CurseForge requires an API key for every request, and hopper does not ship");
    eprintln!("      one. Create one at https://console.curseforge.com/ and either export it:");
    eprintln!("        export {KEY_ENV}='...'");
    eprintln!("      Scheduled updates store a private per-instance credential; it is never");
    eprintln!("      passed to the Minecraft service or placed in command-line arguments.");
}

pub async fn resolve(
    slug: &str,
    version: Option<&str>,
    store: &BlobStore,
    opts: &Options<'_>,
    confirm: impl Fn(&str) -> Result<bool>,
) -> Result<Outcome> {
    let Some(key) = opts.api_key else {
        eprintln!("error: {slug:?} is a CurseForge pack, and no CurseForge API key is set\n");
        missing_key_help();
        bail!("no CurseForge API key");
    };

    // The keyed client can reach only the API host, so a redirect cannot carry the key off
    // anywhere else. Files come from the CDN through a client that never sees the key.
    let api = HttpClient::with_secret_header(
        opts.user_agent,
        HostAllowlist::curseforge_api(),
        cf::KEY_HEADER,
        key,
    )
    .context("building the CurseForge API client")?;
    let cdn = HttpClient::new(opts.user_agent, HostAllowlist::curseforge_files())
        .context("building the CurseForge download client")?;

    let project = cf::find_modpack(&api, slug)
        .await
        .with_context(|| format!("looking up {slug:?} on CurseForge"))?;
    let files = cf::list_files(&api, project.id)
        .await
        .with_context(|| format!("listing files of {slug:?}"))?;
    let chosen = cf::choose_file(&files, slug, version, opts.mc)?;
    let same_project = opts.installed_project == Some(project.id.to_string().as_str());
    let fallback_accepted = same_project && opts.fallback_accepted;
    let skip_blocked = opts.skip_blocked || (same_project && opts.skip_blocked_remembered);

    if !opts.quiet && chosen.release().is_some_and(|r| r != ReleaseType::Release) {
        println!(
            "note: {} has no stable release; installing {} ({})",
            project.name,
            chosen.label(),
            chosen.release().map_or("unknown".into(), |r| r.to_string())
        );
    }

    // The client pack is needed either way: its manifest is the authoritative statement of
    // the Minecraft version and loader, which a server pack does not declare.
    let client_blob = download(&cdn, store, &project, chosen)
        .await
        .with_context(|| format!("downloading {}", chosen.file_name))?;
    let client_pack = read_client_pack(open(&client_blob)?)
        .with_context(|| format!("reading {}", chosen.file_name))?;
    let manifest = &client_pack.manifest;
    let (loader, loader_version) = manifest.loader()?;
    let minecraft = manifest.minecraft();
    let name = if manifest.name.is_empty() {
        project.name.clone()
    } else {
        manifest.name.clone()
    };

    let index = |files: Vec<IndexFile>| MrpackIndex {
        name: name.clone(),
        version_id: chosen.label().to_owned(),
        summary: None,
        minecraft: minecraft.clone(),
        loader,
        loader_version: loader_version.clone(),
        files,
    };

    if let Some(server_pack_id) = chosen.server_pack() {
        if fallback_accepted && !opts.quiet {
            println!(
                "note: {} now publishes a server pack; switching to it",
                project.name
            );
        }
        let server_file = cf::get_file(&api, project.id, server_pack_id)
            .await
            .context("looking up the server pack")?;
        let blob = download(&cdn, store, &project, &server_file)
            .await
            .with_context(|| format!("downloading {}", server_file.file_name))?;
        let server_pack = read_server_pack(open(&blob)?)
            .with_context(|| format!("reading {}", server_file.file_name))?;

        if let Some((bundled_kind, bundled)) = &server_pack.bundled_loader
            && (Some(bundled) != loader_version.as_ref() || *bundled_kind != loader)
            && !opts.quiet
        {
            println!(
                "note: the server pack bundles {bundled_kind} {bundled}, the manifest names {loader} {}; \
                 using the manifest's",
                loader_version.as_deref().unwrap_or("(unspecified)")
            );
        }

        let overrides = stage_overrides(open(&blob)?, &server_pack.entries, store)
            .context("extracting the server pack")?;
        return Ok(Outcome::Resolved(Box::new(Resolved {
            pack: Mrpack {
                index: index(vec![]),
                overrides: server_pack.entries,
            },
            overrides,
            registry: RegistryId::CurseForge,
            project_id: project.id.to_string(),
            file_id: chosen.id.to_string(),
            client_pack_fallback: false,
            skipped_blocked: false,
        })));
    }

    // No server pack. This changes what the operator is getting, so it is said before
    // anything else happens, whatever the flags.
    eprintln!(
        "warning: {} {} has no server pack published.",
        project.name,
        chosen.label()
    );
    eprintln!("         hopper can build the server from the client pack instead, leaving out");
    eprintln!("         mods it judges client-only. That judgement is heuristic: if the server");
    eprintln!("         fails to start, --force-exclude <name> removes a mod and -v shows why");
    eprintln!("         each one was kept or skipped.\n");

    let proceed = if fallback_accepted || opts.allow_client_pack {
        true
    } else if opts.dry_run {
        // Shown, so the plan is visible, but a real run will not apply it on its own.
        eprintln!("note: a real run will ask before building from the client pack; unattended,");
        eprintln!("      pass --allow-client-pack.\n");
        true
    } else if opts.yes || !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        // --yes means "no prompts", not "agree to a different kind of install".
        eprintln!("help: this needs an explicit yes. Re-run with --allow-client-pack to build");
        eprintln!("      from the client pack unattended.");
        false
    } else {
        confirm("Build the server from the client pack?")?
    };
    if !proceed {
        return Ok(Outcome::Declined);
    }

    let (built, skipped_blocked) =
        build_from_manifest(&api, &cdn, store, &client_pack, skip_blocked, opts.quiet).await?;
    let overrides = stage_overrides(open(&client_blob)?, &client_pack.overrides, store)
        .context("extracting the pack's config files")?;
    Ok(Outcome::Resolved(Box::new(Resolved {
        pack: Mrpack {
            index: index(built),
            overrides: client_pack.overrides.clone(),
        },
        overrides,
        registry: RegistryId::CurseForge,
        project_id: project.id.to_string(),
        file_id: chosen.id.to_string(),
        client_pack_fallback: true,
        skipped_blocked,
    })))
}

/// Resolve a manifest's files into index entries, downloading each so it has a sha512.
async fn build_from_manifest(
    api: &HttpClient,
    cdn: &HttpClient,
    store: &BlobStore,
    pack: &ClientPack,
    skip_blocked: bool,
    quiet: bool,
) -> Result<(Vec<IndexFile>, bool)> {
    let wanted = &pack.manifest.files;
    let file_ids: Vec<u64> = wanted.iter().map(|f| f.file_id).collect();
    let mut project_ids: Vec<u64> = wanted.iter().map(|f| f.project_id).collect();
    project_ids.sort_unstable();
    project_ids.dedup();

    let files: BTreeMap<u64, WireFile> = cf::get_files(api, &file_ids)
        .await
        .context("resolving the pack's files")?
        .into_iter()
        .map(|f| (f.id, f))
        .collect();
    let projects: BTreeMap<u64, WireMod> = cf::get_mods(api, &project_ids)
        .await
        .context("resolving the pack's projects")?
        .into_iter()
        .map(|m| (m.id, m))
        .collect();

    let mut out: Vec<IndexFile> = Vec::new();
    let mut blocked = Vec::new();
    let mut not_placed = Vec::new();
    let mut seen: BTreeMap<RelPath, u64> = BTreeMap::new();
    for entry in wanted {
        let Some(file) = files.get(&entry.file_id) else {
            if !entry.required {
                if !quiet {
                    println!(
                        "note: optional file {} of project {} no longer exists on CurseForge; skipped",
                        entry.file_id, entry.project_id
                    );
                }
                continue;
            }
            bail!(
                "CurseForge no longer has file {} of project {}, which the pack requires",
                entry.file_id,
                entry.project_id
            );
        };
        let project = projects.get(&entry.project_id);
        let Some(dir) = cf::dir_for_class(project.and_then(|p| p.class_id)) else {
            not_placed
                .push(project.map_or_else(|| entry.project_id.to_string(), |p| p.name.clone()));
            continue;
        };
        let path = RelPath::parse(&format!("{dir}/{}", file.file_name)).with_context(|| {
            format!("CurseForge file {:?} has an unusable name", file.file_name)
        })?;
        // Two projects shipping the same file name would silently replace one another.
        if let Some(other) = seen.insert(path.clone(), file.id) {
            if other == file.id {
                continue;
            }
            bail!(
                "the pack lists two different files that both install as {path} \
                 (CurseForge files {other} and {})",
                file.id
            );
        }

        let (client_tag, server_tag) = file.environment_tags();
        let env = match (client_tag, server_tag) {
            (true, false) => Some(FileEnv {
                client: EnvSupport::Required,
                server: EnvSupport::Unsupported,
            }),
            (false, true) => Some(FileEnv {
                client: EnvSupport::Unsupported,
                server: EnvSupport::Required,
            }),
            _ if !entry.required => Some(FileEnv {
                client: EnvSupport::Optional,
                server: EnvSupport::Optional,
            }),
            _ => None,
        };

        let Some(url) = file.download_url.clone() else {
            // Only worth stopping for if the server could need it.
            if !(client_tag && !server_tag) {
                blocked.push((path, project, file.id));
            }
            continue;
        };

        let blob = download(
            cdn,
            store,
            project.unwrap_or(&placeholder(entry.project_id)),
            file,
        )
        .await
        .with_context(|| format!("downloading {}", file.file_name))?;
        let mut hashes = Hashes::default();
        hashes.set(file.sha1()?);
        hashes.set(blob.digest.clone());
        out.push(IndexFile {
            path,
            hashes,
            downloads: vec![url::Url::parse(&url).context("CurseForge returned a bad URL")?],
            env,
            size: Some(blob.size),
        });
    }

    if !blocked.is_empty() {
        let lines: Vec<String> = blocked
            .iter()
            .map(|(path, project, file_id)| {
                let page = project
                    .and_then(|p| p.links.website_url.clone())
                    .map(|u| format!("{u}/files/{file_id}"))
                    .unwrap_or_else(|| format!("file {file_id}"));
                format!("  - {path}  ({page})")
            })
            .collect();
        if skip_blocked {
            if !quiet {
                println!(
                    "note: {} file(s) cannot be downloaded by third-party tools and were left out:",
                    blocked.len()
                );
                for l in &lines {
                    println!("{l}");
                }
                println!();
            }
        } else {
            eprintln!(
                "error: {} file(s) in this pack have third-party downloads disabled by their authors:",
                blocked.len()
            );
            for l in &lines {
                eprintln!("{l}");
            }
            eprintln!(
                "\nhelp: download them from the pages above into the server's mods/ folder, then"
            );
            eprintln!(
                "      re-run with --skip-blocked. hopper never touches files it did not install,"
            );
            eprintln!("      so they will survive every update.");
            bail!("the pack includes files that cannot be downloaded automatically");
        }
    }
    if !not_placed.is_empty() && !quiet {
        println!(
            "note: {} project(s) are not mods, resource packs or shaders and were not installed: {}",
            not_placed.len(),
            not_placed.join(", ")
        );
    }
    Ok((out, !blocked.is_empty()))
}

fn placeholder(id: u64) -> WireMod {
    WireMod {
        id,
        slug: String::new(),
        name: String::new(),
        class_id: None,
        allow_mod_distribution: None,
        links: Default::default(),
    }
}

async fn download(
    cdn: &HttpClient,
    store: &BlobStore,
    project: &WireMod,
    file: &WireFile,
) -> Result<Blob> {
    let Some(url) = &file.download_url else {
        bail!(
            "{} ({}) cannot be downloaded by third-party tools; its author has disabled that",
            file.file_name,
            project.name
        );
    };
    let sha1 = file.sha1()?;
    Ok(cdn
        .fetch_to_store(
            std::slice::from_ref(url),
            Some(&sha1),
            file.file_length,
            store,
        )
        .await?)
}

fn open(blob: &Blob) -> Result<std::fs::File> {
    std::fs::File::open(&blob.path).with_context(|| format!("opening {}", blob.path.display()))
}
