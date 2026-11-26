# pressure-guard

## The idea

As many coding agents as the user wants, in parallel, on one 16 GB MacBook Air: the Mac never freezes, never stalls from heat, and answers every click at once. Unlimited agents, zero freezes, no new hardware. It sounds impossible; every change moves toward it.

"Never freezes" means the user never waits on agent work: no app of theirs pages in behind agent memory, no stall from heat, no lag on input. Each change shows, on the real Mac, how it moves that.

## The contract

Every agent runs every command it chooses, as many as it chooses, and each one runs to its end. pressure-guard reshapes *how* and *when* the Mac serves that work (priority, cores, whose memory is swapped first, and whose turn it is while memory is short), never *whether* it runs. Measure each idea against the contract: an idea that has agents do less, skip or end work, or that adds hardware, breaks it. Keep looking for the version that doesn't.

Waiting is allowed as a **turn** only: a job held while memory is short gets its turn within the rota, and the rota ends when memory has room. Turns earn their place by the numbers: agents finish sooner taking turns than thrashing together (2026-11-19, 17 GB of jobs on 12 GB free: 59 and 74 rounds a second in turns, 1 and 2 without).

## The insight

Agents are latency-tolerant: they wait seconds on the model, so a slower command costs them little. The user is not: a 200 ms lag on a click is felt. Spend agent latency to buy user responsiveness: every scarcity on the Mac is paid in agent time, never the user's. Agent work **yields**:

| Scarcity | Paid with | State |
|---|---|---|
| CPU | `nice 10` always, inherited by everything an agent starts, detached children included. `nice` orders work only within a cluster, so saturated performance cores also bring Darwin background mode: agent work moves to the efficiency cores. Saturated efficiency cores alone move nothing, since agent work held there keeps them saturated: on 2026-11-26 that pinned 98% of agent processes to them for an hour while the performance cores sat 84% idle and the user's lag never passed 15 ms | done |
| Memory | Darwin background mode from 30% free, before macOS reports pressure, for idle agent processes only: the background page queue (`vm.vm_page_background_mode`) swaps their memory before the user's at no cost to them, while busy agent work keeps every core. Once memory thrashes (the compressor repacks 600 MB a second while memory spills out to swap at 10 MB a second, whatever the free % says; swap-ins alone are memory read back, as held jobs do on resuming, and never count) busy agent work goes to the background too. Repacking alone misled: bursts passed 1,700 MB/s at 62% free with nothing swapped, and turns held agent jobs for 94% of an hour on them (2026-11-26). Background mode cannot make memory: macOS keeps only 4% of RAM for background memory and compresses the rest whether or not it is in use (`vm_pageout.c`: a page taken from the background queue skips reactivation), so once agents hold more than the Mac has, memory-heavy agent jobs (busy, 256 MB and up) take **turns**: `SIGSTOP` holds the waiting ones, a turn lasts 30 s, the number running halves while the shortage lasts and doubles per roomy turn. A job whose turn it is leaves background mode, so macOS leaves the memory it is using alone | done; open: a shortage shows only once memory has filled (10 s into a sudden 12 GB fill), so the first seconds still swap. The rota settles at one job at a time while free memory could hold more. A held job can miss a deadline its own program set, such as a test runner's per-test timeout. Below the thrashing rate busy agent memory still competes with the user's. The 600 MB rate comes from one slowdown (1,600 MB/s) and calm readings under 250 MB/s, and the 10 MB swap floor from one hour of readings, where real shortages swapped 30–400 MB/s out and bursts with room swapped under 1 (0.8.1 counted swap-ins too, and restarted turns 8 s after each resume at 40% free): tune both from the log |
| Heat | the same background mode once the thermal state leaves normal: agent CPU work moves to the efficiency cores | done; open: act on sustained load before the thermal state changes, as memory does. A fanless Air's heat is a debt: load now, throttle minutes later |
| Disk and network I/O | background mode throttles agent I/O, so the user's swap-ins jump the queue | done under pressure, and while the performance cores stay saturated |
| macOS's own services | Darwin background priority for Apple-signed daemons with no app behind them, when they compete with the user | done |
| Disk space | nothing yet | open: swap grows on the boot volume beside agents' builds and caches (the dev Mac: 23 of 228 GB free). A full disk freezes the Mac whatever the priorities; agent time can't buy disk yet |

A calm minute restores full speed. Find agent work from the live system (agent process trees, plus the agent nice level children inherit), never from saved state, so a crash or restart leaves nothing stuck.

## Where the idea grows

- **Steer by the user's wait, not by proxies.** Pressure levels and free memory predict the user waiting; measure the wait itself. Page-ins of the user's own app processes (`proc_pid_rusage` `ri_pageins`) are readable without root; WindowServer is root-owned and is not. A user-interactive-QoS probe thread's wake-up lateness measures CPU lag. Both are measured and logged on every scan (`user_wait` in `pressure-guard.jsonl`); open: yield when they rise, release when they stay flat, with thresholds read from those samples.
- **A benchmark.** A reproducible load of N parallel agent-like jobs (installs, builds, test runs, a headless browser) beside those user-wait measures, so each change carries a before and after number and "unlimited agents" becomes the largest N with a flat user wait.
- **User presence.** Lag costs only while someone is at the keyboard (HID idle time, display asleep). With nobody there, agent work can take the performance cores while memory is loose. Catch: detached children are found by their inherited nice level, so lift nice only on processes already tracked through an agent tree.

## Verifying

`cargo test`, then watch the real Mac: the log (`/opt/homebrew/var/log/pressure-guard.log`) and its JSON twin (`pressure-guard.jsonl`, fields and queries in `docs/agents/logs.md`), `vm.vm_page_background_count`, load average. A new decision or reading goes into the JSON log with its inputs, so the next agent can diagnose it from the log alone. Tests started from an agent session inherit its nice level; write process tests relative to the test's own nice.

Each change carries a before and after number from `scripts/benchmark.py run`, started from an agent session so its jobs count as agent work. It runs busy and idle agent-like jobs beside a stand-in for the user's app that launchd starts outside every agent tree, and prints agent throughput (`agent_rounds_per_second`) next to the stand-in's response times, file-save times and page-ins. Alternate the variants under comparison, since other agents on the Mac move the numbers.

The user works on this Mac while you measure. A round with `--overcommit-mb` fills its memory on purpose and freezes it for seconds under a guard that fails: run one only for a change that targets memory shortage, as few rounds as prove it, and tell the user before the first. Prove everything else with a few small processes, the way the hold-and-resume tests do.

## Agent skills

### Domain docs

Single-context: one `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.
