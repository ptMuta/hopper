//! Resolving everything a server needs beyond its mods: the vanilla jar, the mod loader, and a
//! JVM to run them under.
//!
//! Loader artifacts are ordinary managed files, tracked in the lockfile exactly like mod jars.
//! That is what makes a loader upgrade clean up after itself — the old
//! `libraries/<version>/` tree stops being wanted and is removed through the same path as any
//! dropped mod, with no special case. Nothing else in the ecosystem does this.

use anyhow::{Context, Result, bail};

use hopper_core::api::mojang::{ServerRelease, VersionManifest, WireVersionMeta};
use hopper_core::cache::BlobStore;
use hopper_core::java::{JavaPlan, JavaReason, JavaRequirement, Platform, plan_java};
use hopper_core::loader::{
    ForgePromotions, LaunchProfile, fabric, installer, neoforge_versions_for, versions,
};
use hopper_core::model::{LoaderKind, Managed, MinecraftVersion, Provenance, RelPath};
use hopper_core::net::HttpClient;
use hopper_core::plan::DesiredFile;

/// Everything needed to start the server, once installed.
pub struct Runtime {
    /// Server jar and loader libraries, ready to be planned like any other file.
    pub files: Vec<DesiredFile>,
    /// Digests keyed by path, since these are staged into the store as they are resolved.
    pub launch: LaunchProfile,
    pub java_major: u32,
    pub loader_version: String,
    /// The loader's installer has never run for this build, and this run may not run it
    /// (a dry run). `files` and `launch` are then placeholders.
    pub installer_pending: bool,
}

/// The vanilla server jar's path inside the server directory.
pub fn server_jar_path() -> RelPath {
    RelPath::parse("server.jar").expect("a constant path")
}

/// Resolve the Minecraft server release, which also tells us the required Java version.
pub async fn resolve_minecraft(
    client: &HttpClient,
    mc: &MinecraftVersion,
) -> Result<ServerRelease> {
    let manifest_bytes = client
        .get_bytes(hopper_core::api::mojang::VERSION_MANIFEST)
        .await
        .context("fetching Mojang's version manifest")?;
    let manifest =
        VersionManifest::parse(&manifest_bytes).context("reading Mojang's version manifest")?;

    let entry = manifest
        .find(mc)
        .with_context(|| format!("Minecraft {mc} is not in Mojang's version manifest"))?;
    let meta: WireVersionMeta = client
        .get_json(&entry.url)
        .await
        .with_context(|| format!("fetching metadata for Minecraft {mc}"))?;

    meta.into_server_release()
        .with_context(|| format!("reading metadata for Minecraft {mc}"))
}

/// What the loader needs from its surroundings beyond the network.
pub struct LoaderEnv<'a> {
    /// The JVM the server will run under. Forge and NeoForge installers run on it too.
    pub java: Option<&'a std::path::Path>,
    /// Where installer runs are staged and remembered between invocations.
    pub cache_root: &'a std::path::Path,
    pub quiet: bool,
    /// False for a dry run: third-party installer code is not run just to show a plan.
    pub run_installer: bool,
}

/// Resolve the loader into installable files plus a launch profile.
///
/// Fabric is resolvable without running anything: its meta service publishes the whole launch
/// descriptor. Forge and NeoForge patch the vanilla jar with an installer, which is run once per
/// build in the cache and never in the server directory.
pub async fn resolve_loader(
    client: &HttpClient,
    store: &BlobStore,
    loader: LoaderKind,
    loader_version: Option<&str>,
    mc: &MinecraftVersion,
    release: &ServerRelease,
    env: &LoaderEnv<'_>,
) -> Result<Runtime> {
    match loader {
        LoaderKind::Vanilla => Ok(Runtime {
            files: vec![vanilla_jar(client, store, mc, release).await?],
            launch: LaunchProfile::ExecutableJar {
                jar: server_jar_path(),
                jvm_args: vec![],
                program_args: vec!["nogui".into()],
            },
            java_major: release.java_major,
            loader_version: String::new(),
            installer_pending: false,
        }),
        LoaderKind::Fabric => {
            let mut files = vec![vanilla_jar(client, store, mc, release).await?];
            let version = match loader_version {
                Some(v) if !v.is_empty() => v.to_owned(),
                _ => {
                    let list = client
                        .get_bytes(&fabric::loaders_url(mc))
                        .await
                        .context("listing Fabric loader versions")?;
                    fabric::choose_loader(&list, mc).context("choosing a Fabric loader version")?
                }
            };

            let profile_bytes = client
                .get_bytes(&fabric::server_profile_url(mc, &version))
                .await
                .context("fetching the Fabric server profile")?;
            let server = fabric::parse_server_profile(&profile_bytes, &version, &server_jar_path())
                .context("reading the Fabric server profile")?;

            for lib in &server.libraries {
                let blob = client
                    .fetch_to_store(
                        std::slice::from_ref(&lib.url),
                        lib.verify.as_ref(),
                        lib.size,
                        store,
                    )
                    .await
                    .with_context(|| format!("downloading {}", lib.coordinate.artifact))?;
                files.push(DesiredFile {
                    path: lib.path.clone(),
                    content: blob.digest,
                    size: Some(blob.size),
                    executable: false,
                    provenance: Provenance::Loader {
                        loader: LoaderKind::Fabric,
                        version: version.clone(),
                    },
                    managed: Managed::Full,
                });
            }

            Ok(Runtime {
                files,
                launch: server.launch,
                java_major: release.java_major,
                loader_version: version,
                installer_pending: false,
            })
        }
        // The installer fetches its own copy of the vanilla jar into libraries/, so ours would
        // only be a second, unused copy.
        LoaderKind::Forge | LoaderKind::NeoForge => {
            let version = installer_build(client, loader, loader_version, mc).await?;
            let Some(record) = installed_build(client, store, loader, mc, &version, env).await?
            else {
                return Ok(Runtime {
                    files: vec![],
                    launch: LaunchProfile::ExecutableJar {
                        jar: server_jar_path(),
                        jvm_args: vec![],
                        program_args: vec![],
                    },
                    java_major: release.java_major,
                    loader_version: version,
                    installer_pending: true,
                });
            };
            let files = record
                .files
                .into_iter()
                .map(|f| DesiredFile {
                    path: f.path,
                    content: f.content,
                    size: Some(f.size),
                    executable: false,
                    provenance: Provenance::Loader {
                        loader,
                        version: version.clone(),
                    },
                    managed: Managed::Full,
                })
                .collect();
            Ok(Runtime {
                files,
                launch: record.launch,
                java_major: release.java_major,
                loader_version: version,
                installer_pending: false,
            })
        }
        LoaderKind::Quilt => bail!(
            "Quilt servers are not supported yet.\n\
             help: Fabric, Forge, NeoForge and vanilla packs work today."
        ),
    }
}

/// The vanilla jar, verified against Mojang's sha1.
async fn vanilla_jar(
    client: &HttpClient,
    store: &BlobStore,
    mc: &MinecraftVersion,
    release: &ServerRelease,
) -> Result<DesiredFile> {
    let blob = client
        .fetch_to_store(
            std::slice::from_ref(&release.jar_url),
            Some(&release.jar_sha1),
            Some(release.jar_size),
            store,
        )
        .await
        .context("downloading the Minecraft server jar")?;
    Ok(DesiredFile {
        path: server_jar_path(),
        content: blob.digest,
        size: Some(blob.size),
        executable: false,
        provenance: Provenance::ServerJar {
            minecraft: mc.clone(),
        },
        managed: Managed::Full,
    })
}

/// The pack's loader build, or the one upstream recommends for this Minecraft version.
async fn installer_build(
    client: &HttpClient,
    loader: LoaderKind,
    pinned: Option<&str>,
    mc: &MinecraftVersion,
) -> Result<String> {
    if let Some(v) = pinned.filter(|v| !v.is_empty()) {
        return Ok(v.to_owned());
    }
    if loader == LoaderKind::Forge {
        let body = client
            .get_bytes(versions::FORGE_PROMOTIONS)
            .await
            .context("fetching Forge's promoted builds")?;
        let promos = ForgePromotions::parse(&body)?;
        return promos
            .build_for(mc)
            .map(str::to_owned)
            .ok_or_else(|| versions::VersionError::NoForgeBuild(mc.clone()).into());
    }
    // NeoForge's first release, for 1.20.1, lives under Forge's old artifact name.
    let legacy = mc.as_str() == "1.20.1";
    let body = client
        .get_bytes(if legacy {
            versions::NEOFORGE_LEGACY_MAVEN_METADATA
        } else {
            versions::NEOFORGE_MAVEN_METADATA
        })
        .await
        .context("listing NeoForge builds")?;
    let all = versions::parse_maven_metadata(&body)?;
    let candidates = if legacy {
        versions::neoforge_legacy_versions(&all)
    } else {
        neoforge_versions_for(&all, mc)
    };
    installer::choose_neoforge(&candidates)
        .map(str::to_owned)
        .ok_or_else(|| versions::VersionError::NoNeoForgeBuild(mc.clone()).into())
}

/// The collected output of a build's installer, running it only if no earlier run is cached.
async fn installed_build(
    client: &HttpClient,
    store: &BlobStore,
    loader: LoaderKind,
    mc: &MinecraftVersion,
    version: &str,
    env: &LoaderEnv<'_>,
) -> Result<Option<installer::InstallRecord>> {
    let staging = installer::staging_dir(env.cache_root, loader, mc, version)?;
    let record_path = staging.join(installer::RECORD_FILE);

    // A complete earlier run whose blobs are all still in the store needs nothing more.
    if let Ok(bytes) = std::fs::read(&record_path)
        && let Some(record) = installer::InstallRecord::from_json(&bytes)
        && record.files.iter().all(|f| store.contains(&f.content))
    {
        return Ok(Some(record));
    }
    if !env.run_installer {
        return Ok(None);
    }

    let Some(java) = env.java else {
        bail!(
            "installing {loader} needs a JVM to run its installer, and none is available.\n\
             help: pass --java <path> to use a specific JVM"
        );
    };

    // Run into a fresh sibling and rename into place, so an interrupted run is never mistaken
    // for a complete one.
    if let Some(parent) = staging.parent() {
        hopper_core::fs::create_dir_all(parent)?;
    }
    let work = staging.with_extension(format!("incoming-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    hopper_core::fs::create_dir_all(&work)?;

    let url = installer::installer_url(loader, mc, version)?;
    let sidecar = client
        .get_bytes(&installer::sidecar_url(&url))
        .await
        .with_context(|| format!("fetching the checksum for the {loader} {version} installer"))?;
    let expect = installer::parse_sha1_sidecar(&String::from_utf8_lossy(&sidecar))?;
    let jar = client
        .fetch_to_store(std::slice::from_ref(&url), Some(&expect), None, store)
        .await
        .with_context(|| format!("downloading the {loader} {version} installer"))?;

    if !env.quiet {
        println!("Running the {loader} {version} installer (this can take a minute)...");
    }
    let log_path = staging.with_extension("log");
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("creating {}", log_path.display()))?;
    let status = tokio::process::Command::new(java)
        .arg("-jar")
        .arg(&jar.path)
        .arg("--installServer")
        .arg(&work)
        .current_dir(&work)
        // The installer is third-party code and has no use for the operator's API key.
        .env_remove(crate::curseforge::KEY_ENV)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .status()
        .await
        .with_context(|| format!("running the {loader} installer with {}", java.display()))?;
    if !status.success() {
        bail!(
            "the {loader} {version} installer failed ({status}).\n\
             help: its output is in {}",
            log_path.display()
        );
    }

    let (kept, launch) = installer::collect(&work, loader)
        .with_context(|| format!("reading what the {loader} installer produced"))?;
    let mut files = Vec::with_capacity(kept.len());
    for f in kept {
        let mut reader = std::fs::File::open(&f.source)
            .with_context(|| format!("reading {}", f.source.display()))?;
        let blob = store.insert_reader(&mut reader, None, None)?;
        files.push(installer::RecordedFile {
            path: f.path,
            content: blob.digest,
            size: blob.size,
        });
    }
    let record = installer::InstallRecord { files, launch };
    std::fs::write(work.join(installer::RECORD_FILE), record.to_json())?;

    let _ = std::fs::remove_dir_all(&staging);
    std::fs::rename(&work, &staging)
        .with_context(|| format!("publishing the installer output to {}", staging.display()))?;
    Ok(Some(record))
}

/// Decide what to do about Java, without downloading anything yet.
pub fn plan_runtime_java(
    java_major: u32,
    mc: &MinecraftVersion,
    explicit: Option<&std::path::Path>,
    vendor: Option<hopper_core::java::JavaVendor>,
) -> Result<JavaPlan> {
    let platform = Platform::current()
        .context("hopper has no JVM story for this operating system or architecture")?;
    let req = JavaRequirement {
        major: java_major,
        reason: JavaReason::Minecraft {
            version: mc.as_str().to_owned(),
        },
    };

    // An explicitly supplied JVM is taken at face value: the operator knows their machine.
    let installed = match explicit {
        Some(path) => vec![probe_or_bail(path)?],
        None => discover_system_java(),
    };
    Ok(plan_java(&req, &installed, vendor, platform))
}

fn probe_or_bail(path: &std::path::Path) -> Result<hopper_core::java::SystemJava> {
    let out = std::process::Command::new(path)
        .args(["-XshowSettings:properties", "-version"])
        .output()
        .with_context(|| format!("running {}", path.display()))?;
    let text = String::from_utf8_lossy(&out.stderr);
    hopper_core::java::parse_probe_output(path, &text)
        .with_context(|| format!("identifying the JVM at {}", path.display()))
}

/// JVMs already on this machine, in preference order.
fn discover_system_java() -> Vec<hopper_core::java::SystemJava> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("JAVA_HOME") {
        candidates.push(std::path::PathBuf::from(home).join("bin/java"));
    }
    if let Ok(out) = std::process::Command::new("sh")
        .args(["-c", "command -v java"])
        .output()
        && out.status.success()
    {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !path.is_empty() {
            candidates.push(std::path::PathBuf::from(path));
        }
    }
    // JVMs hopper provisioned earlier, so a second run uses them rather than reporting that
    // none was found and then "installing" the one already there.
    if let Ok(entries) = data_dir().and_then(|d| Ok(std::fs::read_dir(d.join("toolchains"))?)) {
        candidates.extend(
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.path().join("bin/java"))
                .filter(|p| p.exists()),
        );
    }
    if let Ok(entries) = std::fs::read_dir("/usr/lib/jvm") {
        candidates.extend(
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.path().join("bin/java"))
                .filter(|p| p.exists()),
        );
    }

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for path in candidates {
        if !path.exists() || !seen.insert(path.clone()) {
            continue;
        }
        if let Ok(java) = probe_or_bail(&path) {
            out.push(java);
        }
    }
    out
}

/// Download, verify and unpack a JVM.
///
/// Toolchains land in the data directory rather than the cache, so a cache cleaner cannot brick
/// every server on the machine. The archive stays in the content store, where it genuinely is
/// disposable.
pub async fn provision_java(
    client: &HttpClient,
    store: &BlobStore,
    candidate: &hopper_core::java::JavaCandidate,
    data_root: &std::path::Path,
) -> Result<hopper_core::java::SystemJava> {
    use hopper_core::java::{fetch, install, toolchain_dir};

    let dest = toolchain_dir(
        data_root,
        candidate.vendor,
        candidate.major,
        candidate.platform,
    );

    // Already provisioned by an earlier run, or by another server on this box.
    if let Ok(java) = install::find_java_binary(&dest)
        && let Ok(probed) = probe_or_bail(&java)
    {
        return Ok(probed);
    }

    let download = match candidate.vendor {
        hopper_core::java::JavaVendor::Adoptium => {
            let body = client
                .get_bytes(&candidate.url)
                .await
                .context("asking Adoptium for a JDK")?;
            fetch::parse_adoptium_assets(&body, candidate.major)?
        }
        _ => {
            let mut d = fetch::graalvm_download(candidate);
            // Oracle publishes the checksum alongside the archive rather than inline.
            if let Some(url) = &candidate.checksum_url
                && let Ok(body) = client.get_bytes(url).await
            {
                d.checksum =
                    fetch::parse_sha256_sidecar(&String::from_utf8_lossy(&body), candidate.vendor)
                        .ok();
            }
            d
        }
    };

    let blob = client
        .fetch_to_store(
            std::slice::from_ref(&download.url),
            download.checksum.as_ref(),
            download.size,
            store,
        )
        .await
        .with_context(|| format!("downloading {} {}", candidate.vendor, candidate.major))?;

    // Extract to a temporary sibling, then rename, so a partial unpack is never mistaken for a
    // usable toolchain.
    let staging = dest.with_extension(format!("incoming-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    install::extract_tar_gz(&blob.path, &staging)
        .with_context(|| format!("unpacking {} {}", candidate.vendor, candidate.major))?;

    let java = install::find_java_binary(&staging)
        .with_context(|| format!("locating java in the {} archive", candidate.vendor))?;
    // Confirm the archive really is what the vendor claimed before publishing it.
    let probed = probe_or_bail(&java).context("verifying the downloaded JVM")?;
    if probed.major != candidate.major {
        let _ = std::fs::remove_dir_all(&staging);
        bail!(
            "{} published a Java {} archive under Java {}",
            candidate.vendor,
            probed.major,
            candidate.major
        );
    }

    if let Some(parent) = dest.parent() {
        hopper_core::fs::create_dir_all(parent)?;
    }
    if dest.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    } else {
        std::fs::rename(&staging, &dest)
            .with_context(|| format!("installing the JVM into {}", dest.display()))?;
    }

    let java = install::find_java_binary(&dest)?;
    probe_or_bail(&java)
}

/// Where managed toolchains live.
pub fn data_dir() -> Result<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("HOPPER_DATA_DIR") {
        return Ok(std::path::PathBuf::from(dir));
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
        .context("could not determine a data directory; set HOPPER_DATA_DIR")?;
    Ok(base.join("hopper"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_jar_sits_at_the_directory_root() {
        assert_eq!(server_jar_path().as_str(), "server.jar");
    }

    #[test]
    fn discovery_never_panics_on_a_machine_with_no_java() {
        // Returns an empty list rather than failing; the caller then provisions one.
        let _ = discover_system_java();
    }

    #[test]
    fn probing_a_nonexistent_jvm_is_an_error_not_a_panic() {
        assert!(probe_or_bail(std::path::Path::new("/nonexistent/java")).is_err());
    }
}
