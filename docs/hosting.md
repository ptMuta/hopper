# Hosting and JVM performance

GTNH already provides its supported performance/compatibility stack. Prefer its modern
Java server distribution rather than substituting Paper, Thermos or another hybrid.
The [official FAQ](https://www.gtnewhorizons.com/faq/) recommends modern Java;
[lwjgl3ify server instructions](https://github.com/GTNewHorizons/lwjgl3ify#server)
describe the required patched launcher and arguments and unsupported hybrid servers.
Hopper uses those bundled, version-matched components. Older releases may require older
lwjgl3ify arguments; copying current-master arguments into an old pack is unsafe.

There is no single verified GTNH-specific high-performance hosting recipe behind
Hopper's defaults. This guide distinguishes compatibility requirements from tuning
that must be measured on your own world.

## Establish a baseline

Use fast local SSD/NVMe storage and strong per-core CPU performance. Leave memory for
the OS, file cache and Java native allocations; do not give the JVM all host RAM.
Hopper's initial GTNH heap sizing is capped at 6 GiB, not a promised requirement for
every world. Tune the instance's jvm.args once realistic player/machine load is known.
Avoid swapping, oversubscribed CPUs and backups competing with the tick thread.

Record startup time, steady-state tick duration (including p95/p99), GC pause time,
heap occupancy after collection and native/RSS usage. Run comparable warm workloads
on a copy of the same world before and after one change. If profiling tools are added,
use versions compatible with the pack and keep their files operator-owned.

## Java and garbage collection

Modern Java plus GTNH's compatibility mods is the supported alternative to legacy Java
8. GraalVM changes the JIT compiler; it is not automatically faster for every server.
Compare it to a supported HotSpot distribution on the same Java major and workload.
Do not use a native-image/AOT binary as a drop-in Forge modpack runtime.

For modern Java, begin with the VM's normal G1 ergonomics and suitable heap bounds.
Oracle's [G1 tuning guide](https://docs.oracle.com/en/java/javase/25/gctuning/garbage-first-garbage-collector-tuning.html)
recommends starting with defaults rather than carrying forward old collector flags.
Use GC logging (for example -Xlog:gc*:file=logs/gc.log:time,uptime:filesize=20M,filecount=5)
to establish whether pauses or allocation rate are actually the bottleneck.
Consider another supported collector only after measuring its CPU/memory tradeoffs.

[Aikar's flags](https://docs.papermc.io/paper/aikars-flags/) target Paper; they are not a
GTNH compatibility recipe. Do not blindly combine them, Java 8 flags, old CMS options
and modern G1/ZGC settings. Retain Hopper's required bootstrap arguments and put only
your tuning overrides in jvm.args. JVM flags and memory limits should match the chosen
major and actual RAM budget.

## General hosting

Limit simulation/view distance and excessive force-loaded chunks before trying exotic
JIT flags. Investigate machines/entities/chunk generation with a compatible profiler.
Never remove a pack's performance or compatibility mods merely to emulate vanilla.
Other packs should keep their declared Java and loader requirements; a newer JVM is
not an unconditional optimization for arbitrary older Forge packs.

Keep verified, off-host world backups and test recovery. A scheduled graceful restart
is maintenance, not a cure for a memory leak. Monitor exit status and failed maintenance
jobs. Allow the configured warning and stop timeout; systemd's final SIGTERM fallback
is not proof that a world was saved. Keep RCON firewalled, even with private passwords.
