# hopper

Install and update Minecraft **server** modpacks from Modrinth and CurseForge. One static
binary with no runtime dependencies: it fetches the JVM and mod loader a pack needs itself.

```sh
hopper adrenaserver          # install into the current directory
hopper cf:deceasedcraft      # a CurseForge pack (needs an API key, see below)
hopper                       # update whatever is installed here
hopper -n                    # check for updates, change nothing
hopper status                # what's installed
```

## Install

On x86-64 Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/ptMuta/hopper/main/install.sh | sh
```

This puts `hopper` in `~/.local/bin` after verifying it against the release's `SHA256SUMS`. It
adds that directory to `PATH` if it isn't already on it, and sets up tab completion for bash,
zsh and fish. Running it again is safe: it rewrites its own marked block in your shell startup
files rather than adding another. `--version v0.1.2` pins a release; `--no-modify-path` leaves
startup files alone; `--help` lists the rest.

Update later with:

```sh
hopper self-update            # or --check to only ask whether there is a newer release
```

Prefer to do it by hand? Download the tarball from the
[releases page](https://github.com/ptMuta/hopper/releases), check it with `sha256sum -c
SHA256SUMS`, and put `hopper` anywhere on `PATH`. `hopper completions <bash|zsh|fish>` prints a
completion script.

## Why another one

`mrpack-install` already does the one-liner install well. The gap is **updating**: Prism never
deletes mods a pack removed, so orphaned jars accumulate and crash servers; ferium overwrites
configs you tuned; nothing protects files you added yourself.

hopper tracks every file it installed in a lockfile and reconciles three things on each update —
what it installed, what is on disk now, and what the new pack wants. Two guarantees fall out of
that, and both are enforced structurally rather than by care:

- **A file hopper did not install is never deleted.** Drop your own jar in `mods/` and it
  survives every update, including one that removes everything the pack shipped.
- **A file you edited is never silently overwritten.** Tune a config and it wins; the pack's new
  version lands beside it as `.new` so you can diff them.

Every update restates how many of your own files it left alone.

## What it does

- Installs from a Modrinth slug, a `.mrpack` file or URL, a **collection**, or a **CurseForge**
  modpack (`cf:<slug>` or a curseforge.com URL; a bare slug Modrinth doesn't know is looked up
  on CurseForge too).
- Installs the **mod loader** and the vanilla server jar, and provisions a **JVM** (GraalVM by
  default, `--java-vendor adoptium` otherwise) when nothing suitable is installed.
- Writes a `start.sh` that works the same whatever the loader, and a `jvm.args` that is yours to
  edit. On Forge and NeoForge this replaces `user_jvm_args.txt`, so there is one knob to learn.
  Edit `start.sh` and hopper stops regenerating it, writing `start.sh.new` beside it instead.
- **Keeps client-only mods off your server.** Pack metadata is frequently missing or wrong, so
  several signals are weighed against each other, and what was skipped is reported rather than
  silently dropped.

## CurseForge

CurseForge requires an API key on every request and does not allow one to be shipped inside
an open-source binary, so bring your own from <https://console.curseforge.com/>:

```sh
export CURSEFORGE_API_KEY='...'   # or --cf-api-key, which other users can see in `ps`
hopper deceasedcraft
```

The key is sent only to `api.curseforge.com`: the client carrying it is refused a redirect to
any other host, and files are downloaded from the CDN by a client that never has it.

When a pack publishes a **server pack**, hopper installs that: it is what the author decided
belongs on a server. Launch scripts, the bundled loader installer and anything else hopper
provides itself are left out.

When a pack publishes **no server pack**, hopper says so before doing anything, and asks
whether to build the server from the client pack instead, leaving out mods it judges
client-only. That judgement is a heuristic; `-v` shows why each mod was kept or skipped and
`--force-exclude` overrules it. `--yes` alone does not accept this; pass `--allow-client-pack`
for unattended installs. The choice is remembered, and if the author later publishes a server
pack, the next update switches to it.

Some authors disable third-party downloads of their mods. hopper lists any such files the
server needs, with links, and stops. Download them into `mods/` yourself and re-run with
`--skip-blocked`; as files hopper did not install, they survive every update.

## Commands

| | |
|---|---|
| `hopper [PACK] [VERSION]` | install, switch, or update |
| `hopper status` | what's installed here |
| `hopper disable <mod>` / `enable <mod>` | turn a mod off without removing it |
| `hopper repair` | finish an interrupted update (rarely needed) |
| `hopper self-update` | update hopper to the latest release |
| `hopper completions <shell>` | print a shell completion script |

Useful flags: `-d/--dir`, `-y/--yes`, `-n/--dry-run`, `--eula`, `--mc`, `--loader`,
`--mods-only`, `--no-optional`, `--force-include`/`--force-exclude`, `--java`, `--java-vendor`,
`--cf-api-key`, `--allow-client-pack`, `--skip-blocked`.

`--yes` deliberately does **not** accept the Minecraft EULA. `--eula` is separate, because
agreeing to a licence on your behalf because you skipped a prompt is not defensible.

`--dry-run` exits `10` when changes are pending and `0` when current, so it works as a cron
check. It downloads no JVM and runs no loader installer; a loader that still needs installing
counts as a pending change.

`--force-include`, `--force-exclude`, `--no-optional` and `--skip-blocked` are remembered, so a
bare `hopper` keeps applying them. Passing `--force-include` or `--force-exclude` again replaces
that list. Security refusals exit `5`, transient network failures `4`.

## Safety

- Downloads are verified against the hashes the pack declares, and land in a content-addressed
  cache before your server directory is touched. A failed download leaves the directory
  genuinely unchanged.
- Pack download hosts are restricted to Modrinth's allowlist, enforced on **every redirect hop**
  — checking only the declared URL would let an allowed host redirect anywhere.
  GitHub's asset host is accepted only as a redirect from `github.com`, never as a URL a pack
  names itself.
- Pack entry paths cannot escape the install directory. Symlinks inside pack archives are
  refused; inside JDK archives, where they are legitimate, each is checked to stay inside.
- `world/`, `logs/`, ban lists and `server.properties` are never written or deleted.
- `ops.json`, `whitelist.json`, the ban lists and `usercache.json` are never installed from a
  pack, even into an empty directory: an author's `ops.json` would make them an operator on
  your server.
- A symlinked file or directory in the server (say `mods -> /srv/shared`) is never written
  through or replaced; hopper reports it and leaves it alone.
- **Forge and NeoForge installers are code hopper runs.** The installer jar is verified
  against the SHA-1 its Maven repository publishes, and runs in a cache directory, never in
  your server directory. The installer then downloads the vanilla jar and libraries itself,
  from Mojang and the loader's Maven, and those downloads are outside hopper's allowlist.
  Only what it produced under `libraries/` is kept, as ordinary managed files. It runs without
  your CurseForge API key in its environment.
- An interrupted run leaves a journal, and the next run reconciles rather than replaying, so the
  lockfile cannot durably disagree with the disk.

## Building

```sh
cargo build --release          # target/release/hopper, ~4MB
cargo test                     # 516 tests, no network needed
```

### A static binary for servers

The default build links against the build machine's glibc, so a binary built on a rolling
distribution will not run on Debian 12. For deployment, build a fully static one:

```sh
cargo zigbuild --release --target x86_64-unknown-linux-musl
# target/x86_64-unknown-linux-musl/release/hopper: static, ~4.4MB, no dynamic loader
```

[`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) uses zig as the C toolchain,
which `ring` needs for this target. A musl C toolchain (`musl-tools` on Debian) works too.

The result runs on any x86-64 Linux, including a bare `debian:12-slim` with no CA certificates
and no Java: Mozilla's root certificates are built in, and the system store is used as well
when there is one.

## Status

Working: Fabric, Forge, NeoForge and vanilla servers; `.mrpack`, Modrinth slug, collection and
CurseForge sources; Java provisioning; the full update and reconcile path.

Not yet: **Quilt servers**, which are refused with a clear message rather than half-installed.

Forge and NeoForge run their installer once per build; the result is cached, so updates and
dry runs do not run it again.

NeoForge version discovery maps Minecraft versions by dropping the leading `1.` (`1.21.1` →
`21.1.x`). That convention is defined against the old numbering scheme only, so for the
year-based versions (`26.x`) it returns nothing rather than guessing a mapping and installing
for the wrong version. A pack that names its NeoForge build is unaffected.

**Shared instances** are behind `--features shared-instances`, off by default, and unverified.
Modrinth publishes no API for them; the implementation is reconstructed from their app's source
and may simply be wrong.

## Name

A hopper pulls items in and feeds a container, which is what this does to a server directory.
Not affiliated with or endorsed by Modrinth or CurseForge.

## License

[MIT](LICENSE).
