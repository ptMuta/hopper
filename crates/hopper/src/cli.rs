//! The command surface.
//!
//! One directory is one server is one pack, so the filesystem is the registry and there is
//! nothing to register, list or switch — that removes an entire subcommand tree before it can
//! exist. Install and update are the same verb, distinguished only by whether an argument was
//! given; that symmetry is only expressible with a bare positional, and it is what makes
//! `hopper` on its own mean "update this directory".

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

const EXAMPLES: &str = "\
Examples:
  hopper simply-optimized          install the latest release into .
  hopper simply-optimized@1.11.0   install a specific version
  hopper ./pack.mrpack --eula -y   install from a file, unattended
  hopper                           update the pack installed here
  hopper -n                        check for updates without applying
";

/// Install and update Minecraft server modpacks from Modrinth.
#[derive(Debug, Parser)]
#[command(
    name = "hopper",
    version,
    about,
    after_help = EXAMPLES,
    // The optional leading positional would otherwise swallow subcommand names, so
    // `hopper --dir /srv/mc status` would try to install a pack called "status".
    // Note `args_conflicts_with_subcommands` is deliberately absent: with a flattened
    // positional it defeats this precedence entirely.
    subcommand_precedence_over_arg = true,
    disable_help_subcommand = true,
    max_term_width = 96
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(flatten)]
    pub install: InstallArgs,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show what is installed in this directory
    ///
    /// Reads the lockfile only and never touches the network. To check for updates, run
    /// `hopper -n`.
    Status,

    /// Turn off a mod without removing it
    ///
    /// Renames it to `<file>.disabled`, which hopper respects permanently. Deleting a mod
    /// instead would only get it reinstalled on the next update.
    Disable {
        /// Mod filename or a fragment of it
        name: String,
    },

    /// Turn a disabled mod back on
    Enable {
        /// Mod filename or a fragment of it
        name: String,
    },

    /// Finish an update that was interrupted
    ///
    /// Normally unnecessary: recovery runs automatically at the start of every command.
    Repair,
}

#[derive(Debug, Clone, clap::Args)]
pub struct GlobalArgs {
    /// Server directory
    #[arg(short, long, value_name = "PATH", default_value = ".", global = true)]
    pub dir: PathBuf,

    /// Don't ask for confirmation
    ///
    /// This does NOT accept the Minecraft EULA. Use --eula for that: accepting a legal
    /// agreement because someone wanted to skip a prompt is not defensible.
    #[arg(short, long, global = true)]
    pub yes: bool,

    /// Show what would change, then exit
    ///
    /// Exits 0 when the pack is already current and 10 when there are changes to apply, so it
    /// works as a cron check.
    #[arg(short = 'n', long, global = true)]
    pub dry_run: bool,

    /// Only print errors
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Explain every decision, including files that were skipped
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Emit machine-readable JSON instead of human output
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Clone, clap::Args)]
pub struct InstallArgs {
    /// Pack to install: a slug, a .mrpack file, a URL, or a collection
    ///
    /// Omit it to update the pack already installed in this directory.
    #[arg(value_name = "PACK")]
    pub pack: Option<String>,

    /// Pack version to install [default: latest release]
    ///
    /// `pack@version` is equivalent and preferred. The id is spelled out because a bare
    /// `version` collides with clap's automatic `--version` flag.
    #[arg(id = "pack_version", value_name = "VERSION")]
    pub version: Option<String>,

    /// Accept the Minecraft EULA <https://aka.ms/MinecraftEULA>
    ///
    /// The server will not start until this is accepted.
    #[arg(long)]
    pub eula: bool,

    /// Minecraft version, for sources that don't declare one
    #[arg(long, value_name = "VERSION")]
    pub mc: Option<String>,

    /// Mod loader, for sources that don't declare one
    #[arg(long, value_name = "LOADER")]
    pub loader: Option<Loader>,

    /// Use this Java installation instead of managing one
    #[arg(long, value_name = "PATH")]
    pub java: Option<PathBuf>,

    /// JVM to download when no suitable Java is already installed
    #[arg(long, value_name = "NAME", default_value = "graalvm")]
    pub java_vendor: JavaVendorArg,

    /// Skip files the pack marks optional
    #[arg(long)]
    pub no_optional: bool,

    /// Install only the pack's mods and configs
    ///
    /// Skips the mod loader, the server jar and Java entirely. For a directory that
    /// already has a working server, or one managed by something else.
    #[arg(long)]
    pub mods_only: bool,

    /// Install a file even if hopper classified it as client-only
    #[arg(long, value_name = "NAME")]
    pub force_include: Vec<String>,

    /// Never install a file, whatever the pack says
    #[arg(long, value_name = "NAME")]
    pub force_exclude: Vec<String>,

    /// Replace a different pack already installed in this directory
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Loader {
    Fabric,
    NeoForge,
    Forge,
    Quilt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum JavaVendorArg {
    /// GraalVM (default; faster JIT for long-running servers)
    Graalvm,
    /// Eclipse Temurin
    Adoptium,
}

impl From<Loader> for hopper_core::model::LoaderKind {
    fn from(l: Loader) -> Self {
        match l {
            Loader::Fabric => Self::Fabric,
            Loader::NeoForge => Self::NeoForge,
            Loader::Forge => Self::Forge,
            Loader::Quilt => Self::Quilt,
        }
    }
}

impl From<JavaVendorArg> for hopper_core::java::JavaVendor {
    fn from(v: JavaVendorArg) -> Self {
        match v {
            JavaVendorArg::Graalvm => Self::GraalVm,
            JavaVendorArg::Adoptium => Self::Adoptium,
        }
    }
}

/// Exit codes, so scripts and cron can branch on the outcome.
pub mod exit {
    /// Success, or nothing to do.
    pub const OK: i32 = 0;
    pub const GENERIC: i32 = 1;
    /// The operator declined.
    pub const DECLINED: i32 = 3;
    pub const NETWORK: i32 = 4;
    /// Verification or allowlist refusal.
    pub const SECURITY: i32 = 5;
    /// `--dry-run` found pending changes.
    pub const CHANGES_PENDING: i32 = 10;
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn a_bare_invocation_means_update_here() {
        let cli = Cli::parse_from(["hopper"]);
        assert!(cli.install.pack.is_none());
        assert!(cli.command.is_none());
    }

    #[test]
    fn a_pack_argument_means_install() {
        let cli = Cli::parse_from(["hopper", "simply-optimized"]);
        assert_eq!(cli.install.pack.as_deref(), Some("simply-optimized"));
    }

    #[test]
    fn yes_does_not_accept_the_eula() {
        // Deliberate: a "don't prompt me" flag must not agree to a licence on someone's behalf.
        let cli = Cli::parse_from(["hopper", "x", "-y"]);
        assert!(cli.global.yes);
        assert!(!cli.install.eula, "--yes must never imply --eula");
    }

    #[test]
    fn global_flags_work_on_both_sides_of_a_subcommand() {
        for args in [
            ["hopper", "status", "--dir", "/srv/mc"],
            ["hopper", "--dir", "/srv/mc", "status"],
        ] {
            let cli = Cli::parse_from(args);
            assert_eq!(cli.global.dir, PathBuf::from("/srv/mc"));
            assert!(matches!(cli.command, Some(Command::Status)));
        }
    }

    #[test]
    fn overrides_are_repeatable() {
        let cli = Cli::parse_from([
            "hopper",
            "x",
            "--force-include",
            "sodium",
            "--force-include",
            "iris",
        ]);
        assert_eq!(cli.install.force_include, ["sodium", "iris"]);
    }

    #[test]
    fn quiet_and_verbose_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["hopper", "-q", "-v"]).is_err());
    }

    #[test]
    fn the_default_java_vendor_is_graalvm() {
        let cli = Cli::parse_from(["hopper", "x"]);
        assert_eq!(cli.install.java_vendor, JavaVendorArg::Graalvm);
    }

    #[test]
    fn disable_takes_a_mod_name() {
        let cli = Cli::parse_from(["hopper", "disable", "sodium"]);
        let Some(Command::Disable { name }) = cli.command else {
            panic!("expected disable");
        };
        assert_eq!(name, "sodium");
    }

    #[test]
    fn mods_only_is_off_by_default() {
        assert!(!Cli::parse_from(["hopper", "x"]).install.mods_only);
        assert!(
            Cli::parse_from(["hopper", "x", "--mods-only"])
                .install
                .mods_only
        );
    }

    #[test]
    fn a_subcommand_name_is_not_mistaken_for_a_pack() {
        // The trap this guards: an optional leading positional otherwise swallows the
        // subcommand, and `hopper --dir X status` tries to install a pack called "status".
        for args in [
            vec!["hopper", "--dir", "/srv/mc", "status"],
            vec!["hopper", "status"],
        ] {
            let cli = Cli::parse_from(&args);
            assert!(
                matches!(cli.command, Some(Command::Status)),
                "{args:?} should be the subcommand"
            );
        }
    }

    #[test]
    fn a_version_can_be_a_second_positional() {
        let cli = Cli::parse_from(["hopper", "cobblemon", "1.6.1"]);
        assert_eq!(cli.install.version.as_deref(), Some("1.6.1"));
    }
}
