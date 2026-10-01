//! `hopper self-update`: replace this binary with the latest GitHub release.
//!
//! The release is verified against its own `SHA256SUMS` before anything is replaced, and the
//! swap is a rename within the binary's directory, so an interrupted update leaves the old
//! binary working. The checksum file comes from the same release over the same TLS connection,
//! so it guards against a corrupted download rather than a compromised GitHub account.

use std::path::Path;

use anyhow::{Context, Result, bail};

use hopper_core::cache::BlobStore;
use hopper_core::model::{Digest, HashAlgo};
use hopper_core::net::{HostAllowlist, HttpClient};

use crate::cli::exit;

const REPO: &str = "ptMuta/hopper";

pub struct Options<'a> {
    pub check: bool,
    pub force: bool,
    pub yes: bool,
    pub quiet: bool,
    pub user_agent: &'a str,
    pub cache: &'a Path,
}

#[derive(Debug, serde::Deserialize)]
struct WireRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<WireAsset>,
}

#[derive(Debug, serde::Deserialize)]
struct WireAsset {
    name: String,
    browser_download_url: String,
}

/// The release asset built for the machine this binary runs on, if one is published.
fn asset_name(tag: &str) -> Option<String> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-linux",
        _ => return None,
    };
    Some(format!("hopper-{tag}-{target}.tar.gz"))
}

/// `v0.1.2` or `0.1.2` -> (0, 1, 2). Anything else is not a release we understand.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.strip_prefix('v').unwrap_or(s).splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// The digest `SHA256SUMS` lists for `name`.
fn checksum_for(sums: &str, name: &str) -> Option<Digest> {
    sums.lines().find_map(|line| {
        let (hex, file) = line.split_once(char::is_whitespace)?;
        // `sha256sum` marks binary mode with a leading `*`.
        let file = file.trim().trim_start_matches('*');
        (file == name)
            .then(|| Digest::new(HashAlgo::Sha256, hex).ok())
            .flatten()
    })
}

/// How long one download may take before it is reported as stalled. A release is a few
/// megabytes; two minutes is generous on any connection that is actually moving.
const STEP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// Run a download with a deadline, naming it in the error either way.
async fn within<T>(
    deadline: std::time::Duration,
    what: &str,
    fut: impl std::future::Future<Output = Result<T, hopper_core::net::HttpError>>,
) -> Result<T> {
    match tokio::time::timeout(deadline, fut).await {
        Ok(result) => result.with_context(|| format!("downloading {what}")),
        Err(_) => bail!(
            "downloading {what} got no answer for {}s.\n\
             help: check that this machine can reach github.com and\n\
             release-assets.githubusercontent.com, e.g.\n\
             curl -sSL -o /dev/null -w '%{{http_code}}\\n' https://github.com/{REPO}/releases/latest",
            deadline.as_secs()
        ),
    }
}

pub async fn run(opts: &Options<'_>) -> Result<i32> {
    let current = env!("CARGO_PKG_VERSION");
    // The API and the download both live on GitHub; assets redirect to its asset host, which
    // the allowlist accepts only as a hop from github.com.
    let client = HttpClient::new(
        opts.user_agent,
        HostAllowlist::new("hopper releases", &["api.github.com", "github.com"])
            .with_redirects(hopper_core::net::allowlist::REDIRECT_TARGETS),
    )
    .context("building the HTTP client")?;

    let release: WireRelease = client
        .get_json(&format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .await
        .context("asking GitHub for the latest hopper release")?;
    let latest = &release.tag_name;
    let (Some(have), Some(want)) = (parse_version(current), parse_version(latest)) else {
        bail!("cannot compare this version ({current}) with the latest release ({latest})");
    };

    if want <= have && !opts.force {
        if !opts.quiet {
            println!("hopper {current} is the latest release.");
        }
        return Ok(exit::OK);
    }
    if !opts.quiet {
        println!("hopper {current} -> {}", latest.trim_start_matches('v'));
    }
    if opts.check {
        return Ok(exit::CHANGES_PENDING);
    }

    let Some(name) = asset_name(latest) else {
        bail!(
            "no release is published for {} {}; build from source instead",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    };
    let url_of = |n: &str| {
        release
            .assets
            .iter()
            .find(|a| a.name == n)
            .map(|a| a.browser_download_url.clone())
    };
    let (Some(tarball), Some(sums_url)) = (url_of(&name), url_of("SHA256SUMS")) else {
        bail!("release {latest} is missing {name} or SHA256SUMS");
    };

    let exe = std::env::current_exe().context("locating this binary")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let dir = exe
        .parent()
        .context("this binary has no parent directory")?
        .to_path_buf();

    if !opts.yes && !crate::confirm(&format!("Replace {}?", exe.display()))? {
        println!("Aborted. Nothing changed.");
        return Ok(exit::DECLINED);
    }

    // Each step says what it is doing and gives up after a deadline: a stalled connection must
    // read as an error naming what stalled, not as a hang after "yes".
    let say = |msg: &str| {
        if !opts.quiet {
            println!("{msg}");
        }
    };
    say("Downloading SHA256SUMS...");
    let sums = within(STEP_DEADLINE, "SHA256SUMS", client.get_bytes(&sums_url)).await?;
    let Some(expect) = checksum_for(&String::from_utf8_lossy(&sums), &name) else {
        bail!("SHA256SUMS in release {latest} does not list {name}");
    };
    say(&format!("Downloading {name}..."));
    let store = BlobStore::new(opts.cache);
    let blob = within(
        STEP_DEADLINE,
        &name,
        client.fetch_to_store(std::slice::from_ref(&tarball), Some(&expect), None, &store),
    )
    .await?;
    say("Verified; installing...");

    // Unpacked next to the binary, so the final rename stays on one filesystem.
    let staging = dir.join(format!(".hopper-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let result = (|| -> Result<()> {
        hopper_core::java::install::extract_tar_gz(&blob.path, &staging)
            .with_context(|| format!("unpacking {name}"))?;
        let new = staging.join("hopper");
        if !new.is_file() {
            bail!("{name} does not contain a hopper binary");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
        }
        // Replacing a running executable by rename is safe on Unix: the running process keeps
        // its open copy, and the next start gets the new one.
        std::fs::rename(&new, &exe).with_context(|| {
            format!(
                "replacing {} (if it is in a system directory, re-run with sudo or reinstall \
                 with install.sh)",
                exe.display()
            )
        })
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result?;

    if !opts.quiet {
        println!("Updated to hopper {}.", latest.trim_start_matches('v'));
    }
    Ok(exit::OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_stalled_download_becomes_an_error_naming_it() {
        let stalled = std::future::pending::<Result<(), hopper_core::net::HttpError>>();
        let err = within(std::time::Duration::from_millis(50), "SHA256SUMS", stalled)
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("SHA256SUMS"), "{text}");
        assert!(
            text.contains("release-assets.githubusercontent.com"),
            "{text}"
        );
    }

    #[test]
    fn versions_compare_numerically() {
        assert_eq!(parse_version("v0.1.10"), Some((0, 1, 10)));
        assert!(parse_version("v0.1.10") > parse_version("0.1.9"));
        assert_eq!(parse_version("v1.2"), None);
        assert_eq!(parse_version("nightly"), None);
    }

    #[test]
    fn checksums_are_found_by_exact_name() {
        let hex = "f6830a98081fd6afe7429049f0f4c4cf0f227568ca93dbcc82f7fafd6eefe453";
        let sums = format!(
            "{hex}  hopper-v0.1.1-x86_64-linux.tar.gz\n{}  *other.tar.gz\n",
            "0".repeat(64)
        );
        assert_eq!(
            checksum_for(&sums, "hopper-v0.1.1-x86_64-linux.tar.gz")
                .unwrap()
                .hex(),
            hex
        );
        assert!(checksum_for(&sums, "other.tar.gz").is_some());
        assert!(checksum_for(&sums, "hopper-v0.1.1").is_none());
    }

    #[test]
    fn the_asset_name_matches_what_releases_publish() {
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(
                asset_name("v0.1.1").as_deref(),
                Some("hopper-v0.1.1-x86_64-linux.tar.gz")
            );
        }
    }
}
