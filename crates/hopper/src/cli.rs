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
  hopper adrenaserver              install the latest release into .
  hopper adrenaserver@<version>    install a specific version
  hopper cf:deceasedcraft          install a CurseForge pack
  hopper ./pack.mrpack --eula -y   install from a file, unattended
  hopper                           update the pack installed here
  hopper -n                        check for updates without applying
";

/// Install and update Minecraft server modpacks from Modrinth and CurseForge.
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

    /// Update hopper itself to the latest GitHub release
    ///
    /// Downloads the release for this platform, verifies it against the release's SHA256SUMS
    /// and replaces this binary in place. Honours --yes and --dry-run.
    SelfUpdate {
        /// Only report whether a newer release exists [exit 10 if so]
        #[arg(long)]
        check: bool,
        /// Reinstall even if this is already the latest release
        #[arg(long)]
        force: bool,
    },

    /// Run this server under systemd --user
    #[command(subcommand)]
    Service(ServiceAction),

    /// Send a command to the running server over RCON, or open an interactive console
    ///
    /// `hopper service install` turns RCON on. With no command, reads commands line by line.
    Console {
        /// The server command, e.g. `list` or `say hello`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },

    /// Print a shell completion script
    ///
    /// For example: hopper completions bash > ~/.local/share/bash-completion/completions/hopper
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
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
    /// Pack to install: a slug, a .mrpack file, a URL, a collection, or cf:<slug>
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
    /// [default: graalvm, or adoptium where GraalVM publishes no build]
    ///
    /// Left unset rather than defaulted, because naming a vendor means "only this one":
    /// GraalVM publishes only Java 21 and 25, and the Java 17 servers that Minecraft 1.20.4
    /// and older need would have nothing to fall back to.
    #[arg(long, value_name = "NAME")]
    pub java_vendor: Option<JavaVendorArg>,

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

    /// CurseForge API key [env: CURSEFORGE_API_KEY]
    ///
    /// Visible to other users in `ps`; prefer the environment variable.
    #[arg(long, value_name = "KEY")]
    pub cf_api_key: Option<String>,

    /// Build from the client pack when a CurseForge pack has no server pack, without asking
    #[arg(long)]
    pub allow_client_pack: bool,

    /// Leave out files whose authors disabled third-party downloads, instead of stopping
    #[arg(long)]
    pub skip_blocked: bool,

    /// With --dry-run: report changes only when the pack itself changed
    ///
    /// Files the server deleted or rewrote while running are local drift, not an update. The
    /// update timer uses this so drift alone never restarts a server.
    #[arg(long, hide = true, requires = "dry_run")]
    pub pack_changes_only: bool,
}

#[derive(Debug, Subcommand)]
pub enum ServiceAction {
    /// Write and enable systemd user units for the server in --dir
    ///
    /// Also turns on RCON (random password, server.properties made private) so `hopper
    /// console` can reach the server. Safe to re-run: units are rewritten, settings kept.
    Install {
        /// Unit name [default: the directory's name] -> hopper-<name>.service
        #[arg(long)]
        name: Option<String>,
        /// Update schedule: off, hourly, daily, weekly, or a systemd OnCalendar expression
        ///
        /// When it fires and the pack has changes, the server is stopped, updated and started
        /// again. Omit to keep this installation's current schedule.
        #[arg(long, value_name = "SCHEDULE")]
        update: Option<String>,
        /// Start (or restart) the server now
        #[arg(long)]
        now: bool,
    },
    /// Stop the server and remove its units; the server directory is untouched
    Remove {
        /// Unit name, if one was given at install
        #[arg(long)]
        name: Option<String>,
    },
    /// Ask the server to stop over RCON and wait for it to exit
    ///
    /// What the server unit's ExecStop runs.
    #[command(hide = true)]
    StopServer {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        pid: u32,
    },
    /// Check for and apply an update, stopping and starting the server around it
    ///
    /// What the update timer runs.
    #[command(hide = true)]
    RunUpdate {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        unit: String,
    },
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
    fn the_default_java_vendor_prefers_graalvm_and_can_fall_back() {
        use hopper_core::java::{Arch, JavaVendor, Os, Platform, vendor_chain};
        let cli = Cli::parse_from(["hopper", "x"]);
        assert_eq!(cli.install.java_vendor, None);

        let linux = Platform {
            os: Os::Linux,
            arch: Arch::X64,
        };
        let preferred = cli.install.java_vendor.map(Into::into);
        assert_eq!(vendor_chain(preferred, 21, linux)[0], JavaVendor::GraalVm);
        // Minecraft 1.20.1 needs Java 17, which GraalVM does not publish.
        assert_eq!(vendor_chain(preferred, 17, linux), [JavaVendor::Adoptium]);
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
