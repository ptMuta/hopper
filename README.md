# hopper

Install and update Minecraft **server** modpacks from Modrinth. One static-ish binary, no
runtime dependencies beyond libc.

```sh
hopper simply-optimized      # install into the current directory
hopper                       # update whatever is installed here
hopper -n                    # check for updates, change nothing
hopper status                # what's installed
```

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

- Installs from a Modrinth slug, a `.mrpack` file or URL, or a **collection**.
- Installs the **mod loader** and the vanilla server jar, and provisions a **JVM** (GraalVM by
  default, `--java-vendor adoptium` otherwise) when nothing suitable is installed.
- Writes a `start.sh` that works the same whatever the loader, and a `jvm.args` that is yours to
  edit. On Forge and NeoForge this replaces `user_jvm_args.txt`, so there is one knob to learn.
- **Keeps client-only mods off your server.** Pack metadata is frequently missing or wrong, so
  several signals are weighed against each other, and what was skipped is reported rather than
  silently dropped.

## Commands

| | |
|---|---|
| `hopper [PACK] [VERSION]` | install, switch, or update |
| `hopper status` | what's installed here |
| `hopper disable <mod>` / `enable <mod>` | turn a mod off without removing it |
| `hopper repair` | finish an interrupted update (rarely needed) |

Useful flags: `-d/--dir`, `-y/--yes`, `-n/--dry-run`, `--eula`, `--mc`, `--loader`,
`--mods-only`, `--no-optional`, `--force-include`/`--force-exclude`, `--java`, `--java-vendor`.

`--yes` deliberately does **not** accept the Minecraft EULA. `--eula` is separate, because
agreeing to a licence on your behalf because you skipped a prompt is not defensible.

`--dry-run` exits `10` when changes are pending and `0` when current, so it works as a cron
check. Security refusals exit `5`, transient network failures `4`.

## Safety

- Downloads are verified against the hashes the pack declares, and land in a content-addressed
  cache before your server directory is touched. A failed download leaves the directory
  genuinely unchanged.
- Pack download hosts are restricted to Modrinth's allowlist, enforced on **every redirect hop**
  — checking only the declared URL would let an allowed host redirect anywhere.
- Pack entry paths cannot escape the install directory; archive symlinks are resolved and
  checked rather than trusted or blanket-banned.
- `world/`, `logs/`, ban lists and `server.properties` are never written or deleted.
- An interrupted run leaves a journal, and the next run reconciles rather than replaying, so the
  lockfile cannot durably disagree with the disk.

## Building

```sh
cargo build --release          # target/release/hopper, ~4MB
cargo test                     # 464 tests, no network needed
```

A fully static musl build additionally needs a musl C toolchain (`musl-tools` on Debian), as
`ring` compiles C for that target.

## Status

Working: Fabric and vanilla servers, `.mrpack` and slug and collection sources, Java
provisioning, the full update and reconcile path.

Not yet: **Forge, NeoForge and Quilt servers**, which install by running a vendor jar rather
than from metadata. Their version discovery and launch handling are implemented and tested, but
running the installer is not wired up, so those packs are refused with a clear message rather
than half-installed.

NeoForge version discovery maps Minecraft versions by dropping the leading `1.` (`1.21.1` →
`21.1.x`). That convention is defined against the old numbering scheme only, so for the
year-based versions (`26.x`) it returns nothing rather than guessing a mapping and installing
for the wrong version.

**Shared instances** are behind `--features shared-instances`, off by default, and unverified.
Modrinth publishes no API for them; the implementation is reconstructed from their app's source
and may simply be wrong.

## Name

A hopper pulls items in and feeds a container, which is what this does to a server directory.
Not affiliated with or endorsed by Modrinth.
