# Reading the logs

pressure-guard writes two logs side by side:

- `pressure-guard.log`: one sentence per change, written for the user.
- `pressure-guard.jsonl`: the same events as JSON, plus a `sample` on every scan. Written for agents: query it, don't read it top to bottom.

Under Homebrew both live in `/opt/homebrew/var/log/`. From `./install.sh` they live in `~/Library/Logs/`. The JSON log sits beside whatever file launchd sends stdout to. When stdout is a terminal (a `--dry-run` by hand) there is no JSON log; redirect stdout to a file to get one. The JSON log rolls over to `pressure-guard.jsonl.1` at 64 MB, so query both files for a long window.

Each line is one object with `time` (RFC 3339, local offset, milliseconds) and `event`. Every event that also reaches the readable log carries its sentence as `message`. `sample` has no `message`.

Every line from a dry run (`--dry-run`) carries `"dry_run": true`: nothing it describes was done to any process, even where the `message` reads as if it was. A dry run beside the installed guard reads the system the installed guard shapes, so processes it finds in background mode or held were put there by the installed guard, not by the dry run.

## `sample`: what the Mac looked like at each scan

Written once per scan: every 30 s while calm, every 5 s under stress, every second while jobs take turns. Read the gaps between samples as the scan interval, not as missing data.

| Field | Meaning |
|---|---|
| `trigger` | `timer`, or what woke the scan (`a memory pressure change`, `a thermal state change`, `memory starting to thrash`) |
| `scan_ms` | wall time the scan took; the "stays light" budget |
| `next_scan_seconds` | interval chosen after this scan |
| `memory.pressure` | `normal`, `elevated` or `critical`, as pressure-guard sees it (kernel level, early free %, or thrashing) |
| `memory.free_percent` | `kern.memorystatus_level` |
| `memory.repacked_mb_per_second` | memory the compressor packed and unpacked again, a second; the thrashing signal |
| `memory.thrashing` | at or above `thrashing_megabytes_per_second` |
| `memory.swapped.in_mb_per_second`, `memory.swapped.out_mb_per_second` | the whole Mac's swap traffic since the previous reading (at most a second old). Swap-ins beside the user's page-ins mean the user waited on swapped memory; page-ins with no swap-ins are file reads |
| `thermal` | `normal`, `elevated` (serious) or `critical` |
| `cpu_busy_percent` | busy % of each core cluster since the previous scan (`efficiency`, `performance`) |
| `saturated_clusters` | clusters at or above `busy_cpu_percent` |
| `busiest_processes` | the three processes using the most CPU since the previous scan (at least 1%): `pid`, `name`, `cpu_percent` (100 = one core), `agent_work`. Only the user's own processes: root's, such as WindowServer and `kernel_task`, are not readable |
| `user_wait.app_page_ins_per_second` | pages the user's apps (and their helpers) read from disk or swap, a second; a page is 16 KB on Apple silicon (`sysctl hw.pagesize`). Agent work is left out, even when an app launched it. Includes file-backed page-ins, such as an app opening a file |
| `user_wait.paging_apps` | the three apps paging in the most: `pid`, `name`, `page_ins_per_second` |
| `user_wait.wake_up_lateness` | a user-interactive QoS thread inside pressure-guard sleeps 50 ms over and over; how late it woke since the previous scan: `wake_ups`, `max_ms`, `mean_ms`, `over_a_frame` (later than 16 ms), `over_100_ms` (a lag the user feels). macOS runs user-interactive work on the performance cores first, as it does the main threads of the user's apps, so saturated efficiency cores alone barely move it. Inside an agent session the guard's background mode stretches its timers to about 90 ms: read it only from the launchd daemon |
| `agent_work.agents` | agent programs found (`agent_programs`) |
| `agent_work.processes` | every process counted as agent work |
| `agent_work.idle` | of those, idle for `agent_idle_seconds` |
| `agent_work.in_background` | agent processes held in Darwin background mode |
| `agent_work.taking_their_turn`, `agent_work.waiting_their_turn` | pids running and held (`SIGSTOP`) while jobs take turns |
| `services_slowed` | pids of macOS's own services in background priority |

`user_wait` is the measure the rest exists to keep flat. Pressure levels, free memory and the repack rate only predict it. For now it is only recorded: no decision reads it, so a high `user_wait` never triggers an action by itself.

## Events

| `event` | When | Fields |
|---|---|---|
| `started` | daemon start | `version`, `pid`, `dry_run`, `config_path`, `config` (every setting in force) |
| `config_reloaded` | config file changed or `SIGHUP` | `config` |
| `config_invalid` | the new config failed to parse; the old one stays | `error` |
| `stopped` | clean exit; everything restored | `cause` (`SIGTERM`, `SIGINT`, `the event loop ended`); a start with no `stopped` before it means a crash or a kill |
| `woken` | an event woke a scan | `reason` |
| `condition` | the memory, heat or CPU condition changed | `memory_pressure`, `thrashing`, `free_percent`, `repacked_mb_per_second`, `thermal`, `saturated_clusters` |
| `memory_user` | memory pressure turned critical; one per project, biggest first | `directory`, `bytes`, `programs[{name, bytes}]` |
| `scan_interval` | the interval changed | `seconds` |
| `agent_yields` | a new agent got `nice` | `pid`, `name`, `nice` |
| `agent_work_to_background` | agent work moved to background mode | `pressure`, `scope` (`idle`: only idle work; `all`: heat, a saturated cluster or thrashing), `moved`, `restored` |
| `agent_work_to_full_speed` | a calm `agent_release_after_seconds` passed | `restored`, `background_seconds` |
| `turns_started` | memory short: memory-heavy jobs take turns | `memory_pressure`, `repacked_mb_per_second`, `turn_seconds`, `running[]`, `waiting[]` (each `{pid, name, footprint_mb}`) |
| `turns_resized` | the number running at once changed | `repacked_mb_per_second`, `running[]`, `waiting[]` |
| `turns_ended` | memory has room again | `after_seconds` |
| `turns_recovered` | start-up resumed jobs a previous run left held | `resumed` |
| `service_slowed` | a macOS service went to background priority | `pid`, `name`, `cpu_percent`, `reason` (`runaway`, `stress`, `still_cooling`), `because` |
| `service_restored` | it was released | `pid`, `name`, `slowed_seconds` |
| `services_recovered` | start-up restored services a previous run left slowed | `restored` |
| `action_failed` | a priority change failed for a reason other than the process being gone | `action`, `error`, plus the action's fields |
| `watchdog_*` | the watchdog restarted, found a freeze, restored, saved stacks or killed the daemon | `pid`, `path`, `restored`, `resumed`, `exit`, `error` |

In a dry run, slowing a macOS service also says so in its `message` (`[dry run] would`); every other event relies on `dry_run`.

## Recipes

Set `J=/opt/homebrew/var/log/pressure-guard.jsonl` first.

Did the user wait today, and when:

```sh
jq -c 'select(.event=="sample" and (.user_wait.wake_up_lateness.over_100_ms > 0 or .user_wait.app_page_ins_per_second > 500))
  | {time, lag: .user_wait.wake_up_lateness, page_ins: .user_wait.app_page_ins_per_second, apps: .user_wait.paging_apps, memory}' $J
```

What the guard was doing around a moment the user reports a freeze (here 14:07):

```sh
jq -c 'select(.time >= "2026-11-26T14:05" and .time < "2026-11-26T14:10")' $J
```

Is `thrashing_megabytes_per_second` set right? Bucket user page-ins by repack rate; the threshold belongs where page-ins start to climb:

```sh
jq -r 'select(.event=="sample") | [(.memory.repacked_mb_per_second/200|floor*200), .user_wait.app_page_ins_per_second] | @tsv' $J \
  | sort -n | awk '{s[$1]+=$2; n[$1]++} END {for (k in s) print k" MB/s", n[k]" scans", s[k]/n[k]" page-ins/s"}' | sort -n
```

Does taking turns help the user, not just the repack rate? Compare user wait in the minute before `turns_started` with the minutes after.

```sh
jq -c 'select(.event=="turns_started" or .event=="turns_ended" or (.event=="sample" and .agent_work.waiting_their_turn != []))
  | {time, event, waiting: (.agent_work.waiting_their_turn // .waiting), lag: .user_wait.wake_up_lateness.max_ms, page_ins: .user_wait.app_page_ins_per_second}' $J
```

Is pressure-guard itself light? `scan_ms` over the day:

```sh
jq -s '[.[] | select(.event=="sample") | .scan_ms] | {scans: length, max: max, mean: (add/length)}' $J
```

Which projects hold memory when it runs out:

```sh
jq -c 'select(.event=="memory_user") | {time, directory, gb: (.bytes/1073741824*10|round/10), programs}' $J
```

## Using the logs to improve pressure-guard

A change to a threshold or a rule starts from the samples, not from a guess: find the samples where `user_wait` rose, read which signal moved first (`repacked_mb_per_second`, `free_percent`, `cpu_busy_percent`, `thermal`) and what the guard did (`in_background`, `taking_their_turn`, `services_slowed`). A signal that rises before the user waits is worth acting on earlier; one that rises without the user waiting can be relaxed. Every sample is logged with the `config` from the latest `started` or `config_reloaded`, so compare across config changes, not within one.
