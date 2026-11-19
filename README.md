<p align="center">
  <img src="docs/icon.png" width="160" alt="Pressure Guard icon: a pixel-art cooling fan">
</p>

# pressure-guard

A background agent for macOS that keeps the Mac responsive while many coding agents (Claude Code, Codex) work in parallel. It never ends, skips or redirects an agent or anything it runs, and every command runs to its end: it makes their work give way to you, swaps their memory before yours when RAM runs short, has their memory-heavy jobs take turns once they hold more memory than the Mac has, and slows down macOS's own background services when they compete with you.

**Coding agents and everything they start** (builds, installs, tests, dev servers, headless browsers) run at lower CPU priority (`nice 10`). They keep every core while the CPU has room. `nice` only orders work within a core cluster, so once a cluster is saturated agent work moves to the efficiency cores (below) and leaves the performance cores to your apps. New processes inherit the priority from their agent, including ones that detach.

**When memory gets tight, idle agent work moves to the background.** The moment free memory drops to 30% (`early_memory_free_percent`, before macOS itself reports pressure) or memory pressure leaves normal, every agent process that has sat below 5% CPU and 1 MB/s of disk I/O for 10 s (`agent_idle_cpu_percent`, `agent_idle_seconds`) switches to Darwin background mode: watchers, idle dev servers, language servers. Agents themselves and any process running others stay out, even when idle: a process started from one in background mode is born in it, so every command an agent runs would start on the efficiency cores. macOS keeps background processes' memory on its own page queue and reclaims it first, so when RAM runs short it swaps that idle agent memory instead of the memory behind WindowServer, your browser and your clicks, and it costs the agent nothing because the process isn't running. Busy agent work (builds, tests, installs) keeps every core at full speed; the moment an idle process gets busy again it leaves the background on the next scan.

**When memory thrashes, all agent work moves to the background.** Free memory can read normal while the Mac is out of room: macOS packs memory away and unpacks it again the moment something touches it, and every app waits on that. On the Mac this was built on, a slowdown showed 45% free and "normal" pressure while 1,600 MB a second went through the compressor in each direction. Once macOS repacks 600 MB a second (`thrashing_megabytes_per_second`), busy agent work joins the idle work in background mode, so its memory is swapped before yours and its disk I/O gives way to your swap-ins. It costs agents the performance cores until a calm minute has passed.

**When agents hold more memory than the Mac has, their memory-heavy jobs take turns.** Background mode decides whose memory is swapped first; it cannot make memory. On the Mac this was built on, four agent projects once held 22 GB between them on 16 GB: macOS swapped without end, the disk stalled for 5.5 s and the trackpad froze, with the background rule above already in force. So while memory is short (thrashing as above, or memory pressure *critical*), busy agent processes holding 256 MB or more (`turn_megabytes`) take turns: half of them keep running and the rest are held with `SIGSTOP`; if the shortage lasts, half again, down to one. Every 30 s (`turn_seconds`) the turn passes to the ones that waited, a job that finishes hands its place on at once and one that goes quiet within seconds, and each turn that passes with room to spare lets one more run. A job whose turn it is leaves background mode: macOS compresses background memory down to 4% of RAM even while it is in use, which would starve the one job that is meant to run. Nothing is ended or skipped: every held job gets its turn and runs to its end, and agents get more done this way than by swapping against each other (below). An agent that gives a command a time limit can still end it while it waits, exactly as before. Never held: the agents themselves, a lone heavy job, and anything started by an interactive shell, which would mistake a held job for a finished one. `turn_seconds = 0` turns turns off.

Measured on a 16 GB MacBook Air with about 12 GB free: 8 busy and 6 idle agent jobs of about 1.2 GB each beside a 1 GB stand-in for your app, 60 s, twice per version (`scripts/benchmark.py run --busy 8 --idle 6 --overcommit-mb 6000 --rounds-per-command 1500`):

| | 0.6.0 | with turns |
|---|---|---|
| Longest wait to save a file in your app | 5.7 s and 3.7 s | 32 ms and 46 ms |
| Disk stalls the kernel logged | 6 and 2 (longest 3.1 s) | none |
| Longest wait to touch your app's memory | 0.15 s and 6.2 s | 0.68 s and 0.25 s |
| Swap written | 5.1 and 6.2 GB | 1.1 and 0.2 GB |
| Agent work done (rounds a second) | 2.0 and 1.0 | 59 and 74 |

**When heat or a busy core cluster gets tight, all agent work moves to the background.** Once the thermal state leaves normal or a core cluster is 90% busy (`busy_cpu_percent`), all agent work goes to background mode, which runs it on the efficiency cores so the Mac cools and the performance cores stay free for you. Agents keep running every command. After a calm minute (`agent_release_after_seconds`) everything returns to full speed.

**Never touched otherwise:** every app, including menu-bar apps, and everything an app launched: Terminal builds, dev servers, browser tabs, call audio. macOS's own ownership records ("responsible process") decide this. Also never touched: anything that isn't part of macOS, such as Homebrew services (Postgres, Redis), VPN extensions and other third-party daemons; the `protect` list (AirDrop, Handoff, Universal Control, calls, Siri, accessibility); and anything owned by root.

**Slowed down:** processes that macOS runs by itself with no app behind them, such as photo and media analysis, indexing, iCloud and other sync daemons, and update agents. A process counts as macOS's own only when its code signature marks it as part of macOS, and the same holds for every process that started it.

| Situation | What it does |
|---|---|
| Memory pressure *warning*, thermal *serious*, or a core cluster (efficiency or performance cores) 90% busy | Busy background services (above 30% CPU) get Darwin background priority: efficiency cores, throttled disk and network. They keep running. Released once the Mac is calm. |
| Memory pressure or thermal *critical* | The same, for background services above 10% CPU. |
| A background service pegs the CPU for 3 minutes | Slowed the same way even on a calm Mac, to stop the heat before it builds. Released after 3 quiet minutes. |

When memory pressure turns critical, the log lists the projects using the most memory and the programs in each, so a slowdown can be traced afterwards.

Nothing is ever closed, or announced with a pop-up. Stopping the agent restores every process it slowed and resumes every job it held; if it crashes or is killed, its watchdog resumes the held jobs, launchd restarts it, and the new start restores the rest.

## If it freezes

A crash is easy: launchd restarts the agent. A freeze is not: a stuck agent keeps running, so launchd leaves it alone, and the services it slowed would stay slow. So the agent starts a watchdog: a second, tiny process (under 2 MB, no CPU) that sleeps on a pipe and wakes only when the agent sends it a heartbeat every 5 s. If 30 s pass with no heartbeat, the watchdog:

1. restores every process the agent had slowed and resumes every job it held,
2. saves where the agent was stuck to `~/Library/Logs/pressure-guard-freeze-<time>.txt`,
3. kills the agent, so launchd starts a fresh one.

Each step goes to the log. If the watchdog itself dies, the agent starts a new one on its next heartbeat; when the agent stops, its watchdog exits with it. The clock pauses while the Mac sleeps, so waking up never counts as a freeze.

## How it stays light

It is event-driven: macOS wakes it the instant memory pressure or the thermal state changes (a GCD memory-pressure source and `NSProcessInfoThermalStateDidChangeNotification`). Between events it scans every 30 s while calm and every 5 s under stress or while a background service runs hot, so CPU saturation and a stuck background service are still caught. Once a second it reads two kernel counters to catch memory starting to thrash, which no macOS event announces, and it scans every second while jobs take turns. A burst of events is folded into at most one scan a second, so a flapping pressure signal can't make it add load. It runs as an interactive launchd job, so launchd's background throttling can't make it lag behind a slowdown. Each scan reads CPU time straight from the kernel (`proc_pid_rusage`) and looks up names and app ownership only for the few processes busy enough to matter. Measured on a Mac with 570 processes: about 0.35 ms of CPU per scan, 0.02 s of CPU per two calm minutes, 7 MB of memory.

## Install

```sh
brew trust --tap betterCallHex/tap
brew install betterCallHex/tap/pressure-guard
brew services start pressure-guard
```

Log: `$(brew --prefix)/var/log/pressure-guard.log`.

Or from a clone, without Homebrew:

```sh
./install.sh            # builds, installs to ~/.cargo/bin, starts at login
tail -f ~/Library/Logs/pressure-guard.log
./uninstall.sh
```

Watch what it would do without acting: `cargo run --release -- --dry-run`.

See what is slowed right now, and why:

```console
$ pressure-guard status
1 background service slowed:
  mediaanalysisd (48183), slowed for 2m 4s, at 97% CPU, busy for 3m straight
2 memory-heavy agent jobs wait for a turn while memory is short: tsc (51377), node (51402)
```

Each slow-down in the log carries the same reason: the CPU %, plus either how long the service ran hot or which pressure (memory, thermal) triggered it. Each release says how long the service was slowed.

Configuration: copy `config.example.toml` (Homebrew puts it in `$(brew --prefix)/share/pressure-guard/`) to `~/.config/pressure-guard/config.toml`. Saved changes apply on the next scan, without a restart; `pkill -HUP pressure-guard` applies them at once. An invalid file is logged and the previous settings stay in force.
