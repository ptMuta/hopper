//! Working out what the operator meant by one positional argument.
//!
//! `hopper <thing>` accepts a file path, a URL, a modpack slug or id, a collection, or a shared
//! instance link. One argument absorbing all of them is what makes the zero-argument form
//! (`hopper`, meaning "update this directory") coherent — with an explicit `install` verb you
//! end up with `install` and `update` looking like different operations when they are the same
//! one.
//!
//! Sniffing is only ever ambiguous in ways the operator can resolve, and the escape hatches
//! cost nothing: prefix `./` to force a file, paste a URL to force anything else.

use url::Url;

/// What the argument turned out to be.
///
/// Bare tokens stay `Ambiguous` rather than being guessed at, because telling a slug from a
/// collection id needs a network lookup and this function is pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSpec {
    /// A local `.mrpack`.
    File { path: String },
    /// A direct download.
    Url { url: Url },
    /// A modpack on a registry, optionally pinned to a version.
    Pack {
        slug: String,
        version: Option<String>,
    },
    /// A modpack on CurseForge: from a `cf:` prefix or a curseforge.com URL. The version is a
    /// file id or a file's display name.
    CurseForge {
        slug: String,
        version: Option<String>,
    },
    /// A collection id. Carries no versions, so the caller must supply a Minecraft version and
    /// loader or infer them.
    Collection { id: String },
    /// A shared instance link. Experimental: the backend is undocumented.
    SharedInstance { token: String },
    /// A bare token that could be a pack slug, a pack id, or a collection id. Resolved by
    /// asking the registry, in that order.
    Ambiguous {
        token: String,
        version: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    #[error("no pack given")]
    Empty,
    #[error("{0:?} is a {1}, not a modpack")]
    WrongProjectType(String, &'static str),
    #[error("{0:?} is not a URL hopper recognises")]
    UnrecognisedUrl(String),
}

impl SourceSpec {
    pub fn parse(arg: &str) -> Result<Self, SpecError> {
        let arg = arg.trim();
        if arg.is_empty() {
            return Err(SpecError::Empty);
        }

        // 1. Anything that looks like a path is a path. Checked first so a local file always
        //    wins, and so a missing file reports as a missing file rather than an unknown slug.
        if looks_like_path(arg) {
            return Ok(Self::File {
                path: arg.to_owned(),
            });
        }

        // 2. A bare `pack.mrpack`. Slugs cannot end in `.mrpack`, so this is unambiguous.
        if arg.ends_with(".mrpack") && !arg.contains("://") {
            return Ok(Self::File {
                path: arg.to_owned(),
            });
        }

        // 3. URLs.
        if arg.starts_with("http://") || arg.starts_with("https://") {
            return parse_url(arg);
        }

        // 4. An explicit CurseForge slug. Needed when the same slug exists on both registries,
        //    since a bare token tries Modrinth first.
        if let Some(rest) = arg
            .strip_prefix("cf:")
            .or_else(|| arg.strip_prefix("curseforge:"))
        {
            let (slug, version) = split_version(rest);
            if slug.is_empty() {
                return Err(SpecError::Empty);
            }
            return Ok(Self::CurseForge {
                slug: slug.to_owned(),
                version,
            });
        }

        // 5. A bare token, with an optional `@version`.
        let (token, version) = split_version(arg);
        Ok(Self::Ambiguous {
            token: token.to_owned(),
            version,
        })
    }
}

/// A path is anything with a separator or an explicit relative or home prefix.
///
/// Requiring one of these means `hopper adrenaline` is always the slug even when
/// `./adrenaline/` exists — shell completion supplies the `./` naturally when a directory is
/// what was meant.
fn looks_like_path(arg: &str) -> bool {
    arg.starts_with("./")
        || arg.starts_with("../")
        || arg.starts_with('/')
        || arg.starts_with('~')
        || arg.starts_with(".\\")
        || arg.starts_with("..\\")
        || (arg.contains('/') && !arg.contains("://"))
}

/// Split `slug@version`, leaving anything else alone.
fn split_version(arg: &str) -> (&str, Option<String>) {
    match arg.rsplit_once('@') {
        Some((t, v)) if !t.is_empty() && !v.is_empty() => (t, Some(v.to_owned())),
        _ => (arg, None),
    }
}

fn parse_url(raw: &str) -> Result<SourceSpec, SpecError> {
    let url = Url::parse(raw).map_err(|_| SpecError::UnrecognisedUrl(raw.to_owned()))?;
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();

    if host == "modrinth.com" || host == "www.modrinth.com" {
        return parse_modrinth_url(raw, &segments);
    }

    if host == "curseforge.com" || host == "www.curseforge.com" {
        return parse_curseforge_url(raw, &segments);
    }

    // Any other host, including the CDN: treat as a direct download. The allowlist still
    // applies when it is actually fetched.
    Ok(SourceSpec::Url { url })
}

fn parse_curseforge_url(raw: &str, segments: &[&str]) -> Result<SourceSpec, SpecError> {
    match segments {
        // `/minecraft/modpacks/<slug>`, optionally `/files/<id>` or `/download/<id>`.
        ["minecraft", "modpacks", slug, rest @ ..] => {
            let version = match rest {
                ["files" | "download", id, ..] if id.bytes().all(|b| b.is_ascii_digit()) => {
                    Some((*id).to_owned())
                }
                _ => None,
            };
            Ok(SourceSpec::CurseForge {
                slug: (*slug).to_owned(),
                version,
            })
        }
        ["minecraft", "mc-mods", ..] => Err(SpecError::WrongProjectType(raw.to_owned(), "mod")),
        ["minecraft", "texture-packs", ..] => {
            Err(SpecError::WrongProjectType(raw.to_owned(), "resource pack"))
        }
        ["minecraft", "shaders", ..] => Err(SpecError::WrongProjectType(raw.to_owned(), "shader")),
        _ => Err(SpecError::UnrecognisedUrl(raw.to_owned())),
    }
}

fn parse_modrinth_url(raw: &str, segments: &[&str]) -> Result<SourceSpec, SpecError> {
    match segments {
        // `/modpack/<slug>` and `/project/<slug>`, optionally `/version/<id>`.
        ["modpack" | "project", slug, rest @ ..] => {
            let version = match rest {
                ["version", v, ..] => Some((*v).to_owned()),
                _ => None,
            };
            Ok(SourceSpec::Pack {
                slug: (*slug).to_owned(),
                version,
            })
        }
        ["collection", id, ..] => Ok(SourceSpec::Collection {
            id: (*id).to_owned(),
        }),
        ["shared", token, ..] | ["instance", token, ..] => Ok(SourceSpec::SharedInstance {
            token: (*token).to_owned(),
        }),
        // Naming the actual type is far more useful than "not found" — this is the single most
        // likely mistake, since a mod page and a modpack page look identical in a browser.
        ["mod", ..] => Err(SpecError::WrongProjectType(raw.to_owned(), "mod")),
        ["plugin", ..] => Err(SpecError::WrongProjectType(raw.to_owned(), "plugin")),
        ["datapack", ..] => Err(SpecError::WrongProjectType(raw.to_owned(), "datapack")),
        ["resourcepack", ..] => Err(SpecError::WrongProjectType(raw.to_owned(), "resource pack")),
        ["shader", ..] => Err(SpecError::WrongProjectType(raw.to_owned(), "shader")),
        _ => Err(SpecError::UnrecognisedUrl(raw.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> SourceSpec {
        SourceSpec::parse(s).unwrap()
    }

    #[test]
    fn curseforge_slugs_and_urls() {
        assert_eq!(
            parse("cf:deceasedcraft"),
            SourceSpec::CurseForge {
                slug: "deceasedcraft".into(),
                version: None
            }
        );
        assert_eq!(
            parse("curseforge:deceasedcraft@8448820"),
            SourceSpec::CurseForge {
                slug: "deceasedcraft".into(),
                version: Some("8448820".into())
            }
        );
        assert_eq!(
            parse("https://www.curseforge.com/minecraft/modpacks/deceasedcraft"),
            SourceSpec::CurseForge {
                slug: "deceasedcraft".into(),
                version: None
            }
        );
        assert_eq!(
            parse("https://www.curseforge.com/minecraft/modpacks/deceasedcraft/files/8448820"),
            SourceSpec::CurseForge {
                slug: "deceasedcraft".into(),
                version: Some("8448820".into())
            }
        );
        // The files listing page is not a version.
        assert_eq!(
            parse("https://www.curseforge.com/minecraft/modpacks/deceasedcraft/files"),
            SourceSpec::CurseForge {
                slug: "deceasedcraft".into(),
                version: None
            }
        );
        assert!(matches!(
            SourceSpec::parse("https://www.curseforge.com/minecraft/mc-mods/jei"),
            Err(SpecError::WrongProjectType(_, "mod"))
        ));
        assert!(SourceSpec::parse("cf:").is_err());
    }

    #[test]
    fn explicit_paths_are_files() {
        for s in [
            "./pack.mrpack",
            "../packs/x.mrpack",
            "/srv/packs/x.mrpack",
            "~/downloads/x.mrpack",
            "packs/x.mrpack",
        ] {
            assert!(
                matches!(parse(s), SourceSpec::File { .. }),
                "{s} should be a file"
            );
        }
    }

    #[test]
    fn a_bare_mrpack_filename_is_a_file() {
        // Slugs cannot end in .mrpack, so no ambiguity.
        assert_eq!(
            parse("pack.mrpack"),
            SourceSpec::File {
                path: "pack.mrpack".into()
            }
        );
    }

    #[test]
    fn a_bare_token_is_a_slug_even_if_a_directory_shares_its_name() {
        // Requiring `./` for paths is what makes this predictable; tab completion supplies it.
        assert_eq!(
            parse("adrenaline"),
            SourceSpec::Ambiguous {
                token: "adrenaline".into(),
                version: None
            }
        );
    }

    #[test]
    fn a_version_can_be_pinned_with_an_at_sign() {
        assert_eq!(
            parse("cobblemon-fabric@1.6.1"),
            SourceSpec::Ambiguous {
                token: "cobblemon-fabric".into(),
                version: Some("1.6.1".into())
            }
        );
    }

    #[test]
    fn a_trailing_or_leading_at_sign_is_not_a_version() {
        assert_eq!(
            parse("weird@"),
            SourceSpec::Ambiguous {
                token: "weird@".into(),
                version: None
            }
        );
    }

    #[test]
    fn modrinth_modpack_urls_resolve_to_a_pack() {
        for url in [
            "https://modrinth.com/modpack/simply-optimized",
            "https://www.modrinth.com/modpack/simply-optimized",
            "https://modrinth.com/project/simply-optimized",
        ] {
            assert_eq!(
                parse(url),
                SourceSpec::Pack {
                    slug: "simply-optimized".into(),
                    version: None
                },
                "{url}"
            );
        }
    }

    #[test]
    fn a_version_path_pins_the_version() {
        assert_eq!(
            parse("https://modrinth.com/modpack/simply-optimized/version/1.11.0"),
            SourceSpec::Pack {
                slug: "simply-optimized".into(),
                version: Some("1.11.0".into())
            }
        );
    }

    #[test]
    fn collection_urls_resolve_to_a_collection() {
        assert_eq!(
            parse("https://modrinth.com/collection/HQFfBFap"),
            SourceSpec::Collection {
                id: "HQFfBFap".into()
            }
        );
    }

    #[test]
    fn shared_instance_links_are_recognised() {
        assert_eq!(
            parse("https://modrinth.com/shared/abc123"),
            SourceSpec::SharedInstance {
                token: "abc123".into()
            }
        );
    }

    #[test]
    fn pointing_at_a_mod_says_so_rather_than_failing_vaguely() {
        // The most likely mistake by far: a mod page and a modpack page look identical.
        let err = SourceSpec::parse("https://modrinth.com/mod/sodium").unwrap_err();
        assert!(matches!(err, SpecError::WrongProjectType(_, "mod")));
        assert!(err.to_string().contains("not a modpack"), "got {err}");
    }

    #[test]
    fn other_wrong_project_types_are_named_too() {
        for (url, want) in [
            ("https://modrinth.com/shader/complementary", "shader"),
            (
                "https://modrinth.com/resourcepack/faithful",
                "resource pack",
            ),
            ("https://modrinth.com/datapack/x", "datapack"),
            ("https://modrinth.com/plugin/x", "plugin"),
        ] {
            let err = SourceSpec::parse(url).unwrap_err();
            assert!(
                matches!(err, SpecError::WrongProjectType(_, t) if t == want),
                "{url} -> {err:?}"
            );
        }
    }

    #[test]
    fn a_direct_download_url_is_kept_as_a_url() {
        let spec = parse("https://cdn.modrinth.com/data/X/versions/Y/pack.mrpack");
        assert!(matches!(spec, SourceSpec::Url { .. }));
    }

    #[test]
    fn a_self_hosted_pack_url_is_accepted_here_and_allowlisted_at_fetch_time() {
        // Sniffing decides intent; the host allowlist decides trust, separately.
        assert!(matches!(
            parse("https://example.test/packs/mine.mrpack"),
            SourceSpec::Url { .. }
        ));
    }

    #[test]
    fn an_unrecognised_modrinth_path_is_refused() {
        assert!(SourceSpec::parse("https://modrinth.com/").is_err());
        assert!(SourceSpec::parse("https://modrinth.com/settings").is_err());
    }

    #[test]
    fn empty_input_is_refused() {
        assert_eq!(SourceSpec::parse("").unwrap_err(), SpecError::Empty);
        assert_eq!(SourceSpec::parse("   ").unwrap_err(), SpecError::Empty);
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        // Pasting a URL often brings a trailing newline with it.
        assert_eq!(
            parse("  https://modrinth.com/modpack/x  "),
            SourceSpec::Pack {
                slug: "x".into(),
                version: None
            }
        );
    }

    #[test]
    fn a_base62_token_stays_ambiguous_rather_than_being_guessed() {
        // An 8-character token could be a pack id, a collection id, or a legal slug. Telling
        // them apart needs the network, so this stays honest and defers.
        assert_eq!(
            parse("AABBCCDD"),
            SourceSpec::Ambiguous {
                token: "AABBCCDD".into(),
                version: None
            }
        );
    }
}
