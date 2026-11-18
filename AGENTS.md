# pressure-guard

## The idea

As many coding agents as the user wants, in parallel, on one 16 GB MacBook Air: the Mac never freezes, never stalls from heat, and answers every click at once. Unlimited agents, zero freezes, no new hardware. It sounds impossible; every change moves toward it.

"Never freezes" means the user never waits on agent work: no app of theirs pages in behind agent memory, no stall from heat, no lag on input. Each change shows, on the real Mac, how it moves that.

## The contract

Every agent runs every command it chooses, as many as it chooses, for as long as it chooses. pressure-guard reshapes *how* the Mac serves that work (priority, cores, whose memory is swapped first), never *whether* it runs. Measure each idea against the contract: an idea that has agents do less, wait, skip or stop, or that adds hardware, breaks it. Keep looking for the version that doesn't.

## The insight

Agents are latency-tolerant: they wait seconds on the model, so a slower command costs them little. The user is not: a 200 ms lag on a click is felt. Spend agent latency to buy user responsiveness: every scarcity on the Mac is paid in agent time, never the user's. Agent work **yields**:

| Scarcity | Paid with | State |
|---|---|---|
| CPU | `nice 10` always, inherited by everything an agent starts, detached children included. `nice` orders work only within a cluster, so a saturated cluster also brings Darwin background mode: agent work moves to the efficiency cores, whose saturation then holds it there | done |
| Memory | Darwin background mode from 30% free, before macOS reports pressure: the background page queue (`vm.vm_page_background_mode`) swaps agent memory before the user's | done |
| Heat | the same background mode once the thermal state leaves normal: agent CPU work moves to the efficiency cores | done; open: act on sustained load before the thermal state changes, as memory does. A fanless Air's heat is a debt: load now, throttle minutes later |
| Disk and network I/O | background mode throttles agent I/O, so the user's swap-ins jump the queue | done under pressure, and while a cluster stays saturated (busy agents hold that for whole sessions) |
| macOS's own services | Darwin background priority for Apple-signed daemons with no app behind them, when they compete with the user | done |
| Disk space | nothing yet | open: swap grows on the boot volume beside agents' builds and caches (the dev Mac: 23 of 228 GB free). A full disk freezes the Mac whatever the priorities; agent time can't buy disk yet |

A calm minute restores full speed. Find agent work from the live system (agent process trees, plus the agent nice level children inherit), never from saved state, so a crash or restart leaves nothing stuck.

## Where the idea grows

- **Steer by the user's wait, not by proxies.** Pressure levels and free memory predict the user waiting; measure the wait itself. Page-ins of the user's own app processes (`proc_pid_rusage` `ri_pageins`) are readable without root; WindowServer is root-owned and is not. A user-interactive-QoS probe thread's wake-up lateness measures CPU lag. Yield when they rise, release when they stay flat.
- **A benchmark.** `scripts/benchmark.py`, started from an agent session so its jobs count as agent work: busy and idle agent-like jobs beside a stand-in for the user's app that launchd starts outside every agent tree. `run` prints one round. `sweep` takes turns between busy-job counts (5 stands for one test run of 5 workers, so `5,10,20` is 1, 2 and 4 runs at once) and prints the medians: agent throughput next to the stand-in's response times, its page-ins and the compressor's pages per second. Each change carries a before and after sweep, and "unlimited agents" becomes the largest count with a flat stand-in wait. Open: the jobs only compute and touch memory; installs, builds and a headless browser are not modelled.
- **User presence.** Lag costs only while someone is at the keyboard (HID idle time, display asleep). With nobody there, agent work can take the performance cores while memory is loose. Catch: detached children are found by their inherited nice level, so lift nice only on processes already tracked through an agent tree.

## Verifying

`cargo test`, then watch the real Mac: the log (`/opt/homebrew/var/log/pressure-guard.log`), `vm.vm_page_background_count`, load average. Tests started from an agent session inherit its nice level; write process tests relative to the test's own nice.

## Agent skills

### Domain docs

Single-context: one `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.
