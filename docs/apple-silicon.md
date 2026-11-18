# Apple silicon, as pressure-guard sees it

What the M-series chip in the dev Mac is made of, how macOS hands work to it, what we measured during a real peak, and which of those facts point to better load balancing. Each claim is marked **measured** (on the dev Mac, with the commands below), **Apple** (Apple's own documentation or specs) or **hypothesis** (still to be tested).

## The chip in the dev Mac

MacBook Air 13" M4 (`hw.model` Mac16,12), 16 GB, fanless, macOS 27.

| Part | What it is | Source |
|---|---|---|
| Performance cluster | 4 P cores ("everest"), one shared 16 MB L2, 192 KB L1i and 128 KB L1d per core | measured: `sysctl hw.perflevel0`, `ioreg -l` |
| Efficiency cluster | 6 E cores ("sawtooth"), one shared 4 MB L2, 128 KB L1i and 64 KB L1d per core | measured: `sysctl hw.perflevel1` |
| CPU numbering | `cpu0`–`cpu5` are the E cores, `cpu6`–`cpu9` the P cores | measured: `ioreg -l` `compatible` order |
| Threads | 10: no SMT, one thread per core | measured: `machdep.cpu.thread_count` |
| Memory | 16 GB unified LPDDR5X, about 120 GB/s, shared by CPU, GPU, Neural Engine and display | Apple spec |
| Page size | 16 KB, four times an x86 page: every page-in and swap-out moves 16 KB | measured: `hw.pagesize` |
| Cache line | 128 bytes | measured: `hw.cachelinesize` |
| Scheduler | XNU's Edge scheduler: one run queue per cluster, threads migrate across clusters along "edges" | measured: `kern.sched` = `edge` |
| Cooling | none: no fan, the case is the heatsink | Apple spec |

### Four things that follow from the layout

1. **The cluster is the unit, not the core.** Each cluster runs at one shared clock and voltage. One busy P core keeps the whole P cluster at a high voltage, so where agent work lands decides both speed and heat, and the choice is between two clusters, not ten cores.
2. **Background QoS runs only on E cores.** Darwin background mode (`PRIO_DARWIN_BG`, what pressure-guard applies under stress) puts a process in the background QoS class. On Apple silicon that class is confined to the efficiency cluster (Apple, *Tuning your code's performance for Apple silicon*). `nice` changes priority within a cluster, not which cluster runs the work.
3. **Memory is one pool.** The GPU has no separate VRAM: WindowServer's compositing, a headless browser's GPU process and every agent's heap draw from the same 16 GB, and they share one memory bus and the system-level cache. Background mode decides whose pages are swapped first. Nothing in userspace decides who gets the bandwidth.
4. **A fanless chip runs on a heat budget.** All 10 cores can run flat out for a few minutes. After that the chip lowers its clocks to stay within what the case can shed, and macOS's thermal state changes only after the clocks have dropped. The P cluster burns most of that budget.

## How macOS places work

- **QoS class** sets which cluster a thread may use: user-interactive and user-initiated prefer P, utility may use either, background is E only (Apple).
- **CLPC** (Closed Loop Performance Controller, inside the kernel) sets each cluster's clock and recommends a cluster per *thread group*, roughly one per app or coalition, from the group's QoS and recent behaviour. `kern.sched_recommended_cores` is the bitmask of cores CLPC currently allows; `1023` means all 10 (measured).
- **Darwin background mode** applies three things together: background QoS (E cores only), the lowest disk and network I/O tier, and the background page queue (`vm.vm_page_background_mode`), which the kernel reclaims first. pressure-guard uses it for the memory part. The CPU and I/O parts come with it whether we want them or not.

## A peak, measured

2026-11-18, 09:09–09:11, many agent sessions building and testing in parallel:

| Signal | Value | How |
|---|---|---|
| Load average | 37–45 | `uptime` |
| Agent processes (nice 10) | 105 | `ps -Ao nice` |
| Processes in background mode | 143–150 of about 525 | `proc_pidinfo` `PROC_FLAG_EXT_DARWINBG` |
| E cluster (`cpu0`–`5`) | 99–100% busy, every sample | `host_processor_info` per-CPU ticks over 3 s |
| P cluster (`cpu6`–`9`) | 11–63% busy | same |
| Free memory | 50% → 32% → 50% → 32%, flapping every 30–90 s | pressure-guard log |
| pressure-guard's CPU signal | "CPU normal" throughout | pressure-guard log |
| Compressor | 7.4 GB of RAM holding about 2.0× compressed data | `vm.compressor_bytes_used`, input / compressed bytes |
| Swap | 446 MB of 1 GB used | `vm.swapusage` |
| Free disk | 29 GB of 228 GB | `df -h /` |

Per-process counters from `proc_pid_rusage(RUSAGE_INFO_V6)`, readable without root for the user's own processes:

| Process | Share of its CPU time on P cores |
|---|---|
| T3 Code renderer (user app) | 53% |
| Google Chrome (user app) | 31% |
| Finder | 6% |
| An agent's `node` (nice 10, background mode) | 0.8% |
| A test spinner at nice 10, background mode cleared, user-initiated QoS | 0% |

### What the peak says

1. **Responsiveness works as designed.** The user's apps get the P cores, and agent work stays off them.
2. **Up to half the chip sat idle while agents queued.** About 40 runnable agent threads shared 6 E cores while the 4 P cores ran at 11–63%. Agent work waits on the E cluster while P capacity goes unused, and that unused capacity is the "not getting the best out of the Mac" this project feels.
3. **The idle P capacity is not caused by background mode alone.** The test spinner with background mode cleared still got 0% P time while P cores were idle. **Hypothesis:** CLPC recommends the E cluster for the whole thread group the agent session belongs to, since most of the group's work is background QoS. Test this before building on it.
4. **"CPU normal" hides a full E cluster.** pressure-guard averaged busy time over all 10 cores, so 6 cores at 100% and 4 at 25% read as 70%, under the 90% threshold. Fixed in #10: each cluster is now measured on its own.
5. **Memory pressure flaps on a minute scale.** Free memory crossed the 30% early threshold four times in two minutes. Each crossing switches about 100 processes in or out of background mode, so agent work spends most of a busy session confined to E cores.

## Signals readable without root

| Signal | Reads | Use |
|---|---|---|
| `host_processor_info(PROCESSOR_CPU_LOAD_INFO)` | busy ticks per core | busy share of each cluster |
| `proc_pid_rusage` `ri_user_ptime` + `ri_system_ptime` | CPU time a process spent on P cores | check where agent work actually runs |
| `proc_pid_rusage` `ri_energy_nj`, `ri_penergy_nj` | energy a process used, total and on P cores, in nanojoules | agent joules per minute: a heat signal that leads the thermal state |
| `proc_pid_rusage` `ri_cycles`, `ri_instructions` | cycles and instructions | effective clock and throughput per process |
| `proc_pid_rusage` `ri_runnable_time` | time a process was runnable, running time included | minus its CPU time: how long a user's app waited for a core |
| `proc_pid_rusage` `ri_pageins` | page-ins | the user's apps waiting on memory |
| `kern.sched_recommended_cores` | cores CLPC currently allows | catches CLPC taking cores away under heat |

## Where this points

Each idea keeps the contract: agents run everything they choose, and only *how* the Mac serves them changes.

1. **Measure saturation per cluster.** Done (#10): any cluster at `busy_cpu_percent` counts as CPU stress, and the log names it ("efficiency cores saturated"). Still open: P busy is the user's side and E busy the agents' side, but both now trigger the same response.
2. **Let agent work spill onto idle P cores while the user's wait stays flat.** The P cluster is the user's when they need it. When the user's apps aren't waiting (P busy low, user-app runnable-wait flat), the idle P capacity is free agent throughput. First find what holds agent work on E today (hypothesis 3 above), then find a lever that lets it spill without releasing memory protection.
3. **Separate the three parts of background mode.** Memory protection (background page queue) is what we need under memory pressure. The E-only CPU confinement is a side effect there. Find out whether the page queue can be had without the CPU confinement, for example with a utility QoS clamp (`taskpolicy -c utility`, applied at spawn) instead of full background mode. Not yet tested.
4. **Spend the heat budget on purpose.** Sum agent `ri_energy_nj` per minute and confine agent work to E cores once it stays above what the case can shed, before macOS's thermal state changes. This is the open "act on sustained load" item in `AGENTS.md`, and these counters make it measurable without root.
5. **Damp the flapping.** A threshold crossed four times in two minutes moves about 100 processes back and forth each time. Hysteresis (enter at 30% free, release only above a higher mark held for the calm minute) keeps placement stable. Measure how often it switches before and after.

What the chip can't give: memory bandwidth and the system-level cache are shared, with no userspace control. A cluster full of agent builds can still slow the user's GPU and memory traffic even while every agent thread is on E cores. The GPU and Neural Engine sit mostly idle during agent peaks, but agents' work is compiling and testing, which runs on the CPU, so they offer nothing to balance onto.

## Reproducing the measurements

- Chip layout: `sysctl hw.perflevel0 hw.perflevel1 hw.pagesize hw.cachelinesize kern.sched kern.sched_recommended_cores`, and `ioreg -l -w0 | grep '"compatible" = <"apple,'` for the core order.
- Per-cluster load: sum `CPU_STATE_USER + SYSTEM + NICE` against `IDLE` per CPU from `host_processor_info`, twice, 3 s apart, then group `cpu0`–`5` and `cpu6`–`9`.
- Per-process P share and energy: `proc_pid_rusage(pid, RUSAGE_INFO_V6, …)`. Times are in Mach ticks (`mach_timebase_info`), energy in nanojoules.
- Started from an agent session, test processes inherit nice 10 and pressure-guard's background mode; clear background mode with `setpriority(PRIO_DARWIN_PROCESS, 0, 0)` and expect pressure-guard to set it again on its next scan while memory is under pressure.
