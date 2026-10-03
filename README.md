# hopper

Manage named Minecraft modpack servers on Linux with systemd 247 or newer.
Every installation has a service; there is no directory-dependent implicit install/update.

```sh
hopper providers
hopper search --provider modrinth "server pack"
hopper versions --provider gtnh --channel testing
hopper install horizons --provider gtnh --channel stable --eula
hopper status horizons
hopper start horizons
hopper check horizons
hopper update horizons
```

Install creates a stopped, disabled service. Edit the reported server directory before
starting it. The first `start` enables the service and its configured timers.
Use `--start` to start immediately. EULA acceptance requires `--eula` or an
interactive confirmation; `--yes` does not accept the EULA.

## Install the binary

Download and verify a binary from the [releases](https://github.com/ptMuta/hopper/releases),
or use the installer:

```sh
curl -fsSL https://raw.githubusercontent.com/ptMuta/hopper/main/install.sh | sh
hopper self-update --check
hopper completions bash
```

## Explicit sources and targets

| Provider | Source | Targets |
| --- | --- | --- |
| modrinth | `--pack ID\|SLUG` | `--channel stable\|testing`, or `--pack-version ID\|VERSION` |
| modrinth | `--collection ID --mc VERSION --loader LOADER` | stable/testing, no single collection version |
| curseforge | `--pack ID\|SLUG` | stable/testing, or exact file ID/display name |
| gtnh | no pack selector | stable/testing/daily/experimental, or exact release/snapshot |
| mrpack | `--file PATH` or `--url HTTPS_URL` | archive contents, no registry channel |

Stable is the default. Testing means prereleases, not an automatic fallback when stable
is unavailable. IDs are preferable to ambiguous display names. Providers never fall back
to another provider. Bare `hopper` prints help; bare pack names, `cf:` prefixes,
`--dir`, and `--mods-only` are not public commands.

```sh
hopper install friends --provider modrinth --pack adrenaserver --eula
hopper install forge --provider curseforge --pack deceasedcraft --eula
hopper install local --provider mrpack --file ./server.mrpack --eula
hopper configure horizons --channel testing
hopper check horizons
hopper update horizons
```

Configure records a desired target but never applies pack files. Version downgrades are
refused when the registry baseline can be compared; Hopper does not downgrade worlds.
Changing provider/pack identity is not an update operation.

CurseForge requires `CURSEFORGE_API_KEY`. The key is sent only to its API, never CDN
downloads. Scheduled updates save it privately; system workers receive a systemd
credential, not a public unit environment variable. Packs without a server pack require
explicit `--allow-client-pack`; blocked downloads can be skipped with `--skip-blocked`.

## GregTech: New Horizons

GTNH uses its native server distributions, not a generic Minecraft 1.7.10 Java 8 install.
Stable and testing follow the official [version catalog](https://downloads.gtnewhorizons.com/versions.json);
testing includes beta and release-candidate builds. Daily and experimental use separate
[DreamAssembler manifests](https://github.com/GTNewHorizons/DreamAssemblerXXL/tree/master/releases/manifests).
Those manifests, asset indexes and bootstrap files are resolved against one Git commit
per staged installation. Upstream destructive updater scripts are never run.

New installations prefer the newest supported Java LTS (25, then 21, then 17), bounded
by the chosen release's compatibility metadata. GraalVM is preferred by the existing JVM
provisioner; `--java-vendor adoptium` selects the alternative. `--java PATH` supplies a JVM.
Use `--java-major 8` to select a published legacy distribution deliberately.
Daily/experimental require modern Java. Existing onboarding requires an explicit major
when Hopper cannot recover it from a previous lock.

```sh
hopper install lab --provider gtnh --channel testing --java-major 21 --eula
hopper configure lab --java-major 25
hopper update lab
```

The configuration change alone does not switch the installed runtime.
A daily installed version has an immutable `daily:CONFIG:COMMIT` label, usable as
`--pack-version`. Do not assume testing or daily is safe for a production world.
See the [hosting/performance guide](docs/hosting.md) before tuning JVM flags.

## Scheduled graceful maintenance

```sh
hopper configure horizons --restart 'daily' --warn 300
hopper configure horizons --update 'Sun *-*-* 04:00:00'
hopper restart horizons
hopper stop horizons
```

Schedules accept off/hourly/daily/weekly or systemd OnCalendar expressions.
Updates check upstream first and stage downloads/runtime preparation before stopping.
Only a changed update warns players. Restarts and changed updates use RCON warnings,
save the world, and stop through the service's graceful stop helper before applying.
A previously stopped server stays stopped. An operator stop cancels pending maintenance
and prevents it restarting the server. Failed checks/staging/warnings leave the service
untouched; failed apply leaves it stopped. The default warning period is five minutes.
A combined update/restart calendar creates one maintenance job; update-only timers have
randomized delay. Updates are persistent across downtime; restart timers are not.
Operations are serialized per instance.

## Onboard an existing server, world and edited configs

Stop the old server and disable its previous supervisor first. Onboarding moves the
complete installation into Hopper's managed directory, after a verified private tar.gz
backup. It does not upgrade the installed pack during adoption:

```sh
hopper onboard old-world --provider gtnh --channel stable \
  --from /srv/old-gtnh --installed-version 2.8.4 --java-major 21 --eula
```

An existing Hopper lock or CurseForge manifest can identify the baseline; otherwise
`--installed-version` is required. The exact baseline must still be downloadable.
Use `--backup /outside/source/backup.tar.gz` to select a backup location.
The invoking user must be able to remove the source directory from its parent and
write/search its directories; ownership of the source alone is not sufficient.
System backups require a root-owned, non-writable parent hierarchy (not /tmp or a user home).
Allow enough disk space for the backup, a full destination and staged artifacts.

Worlds, modified configs, custom jars, disabled files and access lists are restored.
Runtime launcher/ledger are regenerated; server.properties retains its settings except
service ports and a fresh RCON secret. The original is removed only after archive and
destination verification plus successful service setup. External/absolute symlinks,
special files and external worlds are rejected. A changed source cancels cleanup.
Backups are retained for manual recovery; `hopper repair NAME` retries interrupted
baseline adoption/service setup. Never start the old copy while migration is pending.

Updates reconcile the installed baseline, current files and new pack: untracked files
are never deleted; edited configs win and incoming versions appear as `.new`.
Review and merge `.new` files before starting a major pack upgrade: old or
server-generated configs can reference items removed by the new version.
Starts and maintenance restarts are refused while any `.new` file remains in the
server directory (including launcher conflicts). Review and merge or deliberately
reject each incoming change, then remove its `.new` sidecar. Editing the original
alone does not mark the conflict resolved. The service also checks before launching
Java, including direct systemd starts and boot-time activation. Existing installations
can refresh their generated unit with `hopper repair NAME --scope user` (or `system`).
Forge can also request explicit world-remapping consent for removed IDs. Hopper
does not automatically confirm potentially destructive world conversions; review
the pack's upgrade instructions and keep a verified backup before consenting.
World paths (including the configured level-name), logs and access lists are protected.
`hopper disable NAME mods/example.jar` and `hopper enable NAME mods/example.jar`
record operator choices for later updates.

## User and system scopes

User is always the default, even under sudo. Select `--scope system` explicitly:

```sh
sudo hopper install production --scope system --provider gtnh --eula
sudo hopper start production --scope system --rcon-firewall-confirmed
sudo hopper configure production --scope system --restart daily
```

User storage is under XDG data `hopper/servers/NAME/server`, instance records under XDG
config `hopper/instances`, units under `~/.config/systemd/user`.
For operation after logout, an administrator must enable user lingering.
System records live in `/etc/hopper/instances`, storage in
`/var/lib/hopper/NAME/server`, caches in `/var/cache/hopper/NAME`, units in
`/etc/systemd/system`. System installations require root.

System instances receive separate static non-login accounts. Pack downloads, archive
processing, installers, Java and RCON run as the instance account; root only coordinates
trusted configuration and systemd. Units use no-new-privileges, empty capability sets,
private devices/tmp, protected home/system/kernel paths and narrowly scoped writable
directories. Executable-memory denial is deliberately absent because Java uses a JIT.

Hopper does not configure your firewall. Block RCON externally before starting any
instance; Minecraft commonly exposes it beyond localhost. System activation requires
`--rcon-firewall-confirmed`, tied to the current RCON port; changing it requires
reconfirmation. Open only the game port to players.

## Operations and recovery

```sh
hopper list
hopper status horizons --json
hopper show horizons
hopper logs horizons -- -f
hopper console horizons -- list
hopper chat horizons
hopper remove horizons
```

Remove deletes units/timers, not server data, accounts or backups. Reattach a removed
instance with `onboard NAME --from ITS_MANAGED_SERVER_PATH` and its recorded source
arguments; data is not migrated twice. Pending instances cannot start until repaired.
`--dry-run` reports intended mutations; `check` checks pack changes without applying them
(metadata/cache reads can still occur). Exit 10 indicates changes available; 0 means
success/no changes and nonzero other than 10 means failure.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release
```

Linux service activation should additionally be smoke-tested on a disposable systemd
VM for both scopes; unit rendering and local integration tests do not prove host policy
or distribution-specific sandbox compatibility.
