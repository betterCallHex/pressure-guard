<p align="center">
  <img src="docs/icon.png" width="160" alt="Pressure Guard icon: a pixel-art cooling fan">
</p>

# pressure-guard

A background agent for macOS that keeps the Mac cool and responsive by slowing down macOS's own background services when they compete with you. It never touches your work.

**Never touched:** every app, including menu-bar apps, and everything an app launched: Terminal builds, dev servers, browser tabs, call audio. macOS's own ownership records ("responsible process") decide this. Also never touched: anything that isn't part of macOS, such as Homebrew services (Postgres, Redis), VPN extensions and other third-party daemons; the `protect` list (AirDrop, Handoff, Universal Control, calls, Siri, accessibility); and anything owned by root.

**Slowed down:** processes that macOS runs by itself with no app behind them, such as photo and media analysis, indexing, iCloud and other sync daemons, and update agents. A process counts as macOS's own only when its code signature marks it as part of macOS, and the same holds for every process that started it.

| Situation | What it does |
|---|---|
| Memory pressure *warning*, thermal *serious*, or the CPU 90% busy | Busy background services (above 30% CPU) get Darwin background priority: efficiency cores, throttled disk and network. They keep running. Released once the Mac is calm. |
| Memory pressure or thermal *critical* | The same, for background services above 10% CPU. |
| A background service pegs the CPU for 3 minutes | Slowed the same way even on a calm Mac, to stop the heat before it builds. Released after 3 quiet minutes. |

Nothing is ever paused, closed, or announced with a pop-up. Stopping the agent restores every process it slowed; if it crashes, the next start restores them.

## How it stays light

It is event-driven: macOS wakes it the instant memory pressure or the thermal state changes (a GCD memory-pressure source and `NSProcessInfoThermalStateDidChangeNotification`). Between events it scans every 30 s while calm and every 5 s under stress or while a background service runs hot, so CPU saturation and a stuck background service are still caught. A burst of events is folded into at most one scan a second, so a flapping pressure signal can't make it add load. It runs as an interactive launchd job, so launchd's background throttling can't make it lag behind a slowdown. Each scan reads CPU time straight from the kernel (`proc_pid_rusage`) and looks up names and app ownership only for the few processes busy enough to matter. Measured on a Mac with 570 processes: about 0.35 ms of CPU per scan, 0.02 s of CPU per two calm minutes, 7 MB of memory.

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
```

Each slow-down in the log carries the same reason: the CPU %, plus either how long the service ran hot or which pressure (memory, thermal) triggered it. Each release says how long the service was slowed.

Configuration: copy `config.example.toml` (Homebrew puts it in `$(brew --prefix)/share/pressure-guard/`) to `~/.config/pressure-guard/config.toml`. Saved changes apply on the next scan, without a restart; `pkill -HUP pressure-guard` applies them at once. An invalid file is logged and the previous settings stay in force.
