#!/usr/bin/env python3
import argparse
import ctypes
import json
import os
import plistlib
import resource
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

PAGE = 16384
BACKGROUND_PRIORITY = 4
PRIO_DARWIN_PROCESS = 4
USER_TICK_SECONDS = 0.1
CPU_ROUND = 300_000
VM_COUNTERS = ("Swapins", "Swapouts", "Compressions", "Decompressions")
LONGEST_GUARD_RELEASE_SECONDS = 90
EXPERIMENT_LABELS = "pressure-guard."
SWEEP_COLUMNS = (
    ("Agent rounds/s", lambda result: result["agent_rounds_per_second"], "{:.1f}"),
    ("Rounds/s per busy job", lambda result: result["agent_rounds_per_second"] / max(result["busy_jobs"], 1), "{:.2f}"),
    ("Busy jobs in background", lambda result: result["busy_jobs_in_background"], "{:.0%}"),
    ("Stand-in median", lambda result: result["user"]["response_ms_median"], "{:.0f} ms"),
    ("Stand-in p95", lambda result: result["user"]["response_ms_p95"], "{:.0f} ms"),
    ("Stand-in max", lambda result: result["user"]["response_ms_max"], "{:.0f} ms"),
    ("Stand-in waits >100 ms", lambda result: result["user"]["responses_over_100ms"], "{:.0f}"),
    ("Stand-in page-ins", lambda result: result["user"]["page_ins"], "{:.0f}"),
    ("Lowest free memory", lambda result: result["lowest_free_memory_percent"], "{:.0f}%"),
    ("Compressor pages/s", lambda result: (result["compressions"] + result["decompressions"]) / result["seconds"], "{:.0f}"),
    ("Swap-ins", lambda result: result["swapins"], "{:.0f}"),
)


def main():
    parser = argparse.ArgumentParser(description="Agent-like load beside a stand-in for your app, measuring both.")
    roles = parser.add_subparsers(dest="role", required=True)
    run = roles.add_parser("run", help="run one benchmark round and print its result as JSON")
    run.add_argument("--label", default="run")
    run.add_argument("--busy", type=int, default=6, help="agent jobs that compute and touch their memory nonstop")
    add_round_arguments(run)
    sweep = roles.add_parser("sweep", help="run rounds at several busy-job counts in turn and print a table of medians")
    sweep.add_argument("--busy", type=job_counts, default=[0, 5, 10, 20, 8],
                       help="busy-job counts to compare; 5 stands for one test run of 5 workers, so 5,10,20 is 1, 2 and 4 runs at once")
    sweep.add_argument("--repeats", type=int, default=3, help="rounds per busy-job count")
    add_round_arguments(sweep)
    for role in ("busy", "idle", "user"):
        job = roles.add_parser(role)
        job.add_argument("--memory-mb", type=int, required=True)
        job.add_argument("--seconds", type=float, required=True)
        job.add_argument("--out", required=True)
    arguments = parser.parse_args()
    roles_run = {"run": run_round, "sweep": run_sweep, "busy": busy_job, "idle": idle_job, "user": user_stand_in}
    roles_run[arguments.role](arguments)


def add_round_arguments(parser):
    parser.add_argument("--idle", type=int, default=6, help="agent jobs that hold memory and sleep, like watchers")
    parser.add_argument("--memory-mb", type=int, default=400, help="memory each agent job holds")
    parser.add_argument("--user-memory-mb", type=int, default=1024, help="memory your stand-in app holds")
    parser.add_argument("--seconds", type=int, default=60)


def job_counts(text):
    return [int(count) for count in text.split(",")]


def run_round(arguments):
    print(json.dumps(measure_round(arguments)))


def run_sweep(arguments):
    results = []
    for busy in arguments.busy * arguments.repeats:
        if results:
            time.sleep(LONGEST_GUARD_RELEASE_SECONDS)
        one_round = argparse.Namespace(**{**vars(arguments), "busy": busy, "label": f"{busy} busy"})
        results.append(measure_round(one_round))
        print(json.dumps(results[-1]), file=sys.stderr)
    print(sweep_table(results, arguments.busy))
    disturbed = sum(result["other_experiments_running"] > 0 or result["pressure_guards_running"] > 1 for result in results)
    if disturbed:
        print(f"\n{disturbed} of {len(results)} rounds shared the Mac with a second pressure-guard or another launchd job labelled {EXPERIMENT_LABELS}*")


def measure_round(arguments):
    leave_background_mode()
    workspace = Path(tempfile.mkdtemp(prefix="pressure-guard-benchmark-"))
    user_out = workspace / "user.json"
    label = own_stand_in_label()
    start_user_stand_in(label, arguments, user_out)
    jobs = [start_job(role, index, arguments, workspace) for role, count in (("busy", arguments.busy), ("idle", arguments.idle)) for index in range(count)]
    counters_before = vm_counters()
    samples = sample_while_running(jobs, arguments.seconds)
    counters_after = vm_counters()
    for process, _, _ in jobs:
        process.wait()
    wait_for_file(user_out, timeout_seconds=arguments.seconds)
    stop_user_stand_in(label)
    busy_rounds = sum(json.loads(out.read_text())["rounds"] for _, role, out in jobs if role == "busy")
    return {
        "label": arguments.label,
        "busy_jobs": arguments.busy,
        "seconds": arguments.seconds,
        "agent_rounds_per_second": round(busy_rounds / arguments.seconds, 2),
        "user": json.loads(user_out.read_text()),
        **{name: counters_after[name] - before for name, before in counters_before.items()},
        **samples,
    }


def own_stand_in_label():
    return f"{EXPERIMENT_LABELS}benchmark.{os.getpid()}"


def leave_background_mode():
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.setpriority(PRIO_DARWIN_PROCESS, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "could not leave background mode inherited from the parent")


def start_user_stand_in(label, arguments, out):
    command = [sys.executable, str(Path(__file__).resolve()), "user", "--memory-mb", str(arguments.user_memory_mb),
               "--seconds", str(arguments.seconds), "--out", str(out)]
    plist = out.with_name("user.plist")
    job = {"Label": label, "ProgramArguments": command, "ProcessType": "Interactive", "RunAtLoad": True}
    plistlib.dump(job, plist.open("wb"))
    subprocess.run(["launchctl", "bootstrap", f"gui/{os.getuid()}", str(plist)], check=True)


def stop_user_stand_in(label):
    subprocess.run(["launchctl", "bootout", f"gui/{os.getuid()}/{label}"], check=False, capture_output=True)


def start_job(role, index, arguments, workspace):
    out = workspace / f"{role}-{index}.json"
    command = [sys.executable, str(Path(__file__).resolve()), role, "--memory-mb", str(arguments.memory_mb),
               "--seconds", str(arguments.seconds), "--out", str(out)]
    return subprocess.Popen(command), role, out


def sample_while_running(jobs, seconds):
    lowest_free = 100
    most_pressure_guards = 0
    most_other_experiments = 0
    background_share = {"busy": [], "idle": []}
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        time.sleep(2)
        lowest_free = min(lowest_free, free_memory_percent())
        most_pressure_guards = max(most_pressure_guards, running_pressure_guards())
        most_other_experiments = max(most_other_experiments, other_experiments_running())
        for role in background_share:
            pids = [process.pid for process, job_role, _ in jobs if job_role == role]
            if pids:
                background_share[role].append(sum(map(in_background, pids)) / len(pids))
    return {
        "lowest_free_memory_percent": lowest_free,
        "pressure_guards_running": most_pressure_guards,
        "other_experiments_running": most_other_experiments,
        "busy_jobs_in_background": round(statistics.fmean(background_share["busy"] or [0]), 2),
        "idle_jobs_in_background": round(statistics.fmean(background_share["idle"] or [0]), 2),
    }


def sweep_table(results, busy_counts):
    titles = ["Busy jobs", *(title for title, _, _ in SWEEP_COLUMNS)]
    rows = [median_row(busy, [result for result in results if result["busy_jobs"] == busy]) for busy in busy_counts]
    return "\n".join("| " + " | ".join(cells) + " |" for cells in (titles, ["---"] * len(titles), *rows))


def median_row(busy, results):
    medians = (shown.format(statistics.median(map(measure, results))) for _, measure, shown in SWEEP_COLUMNS)
    return [str(busy), *medians]


def busy_job(arguments):
    memory = hold(arguments.memory_mb)
    pages = len(memory) // PAGE
    deadline = time.monotonic() + arguments.seconds
    rounds = 0
    while time.monotonic() < deadline:
        memory[::PAGE] = bytes([rounds % 256]) * pages
        compute(CPU_ROUND)
        rounds += 1
    Path(arguments.out).write_text(json.dumps({"rounds": rounds}))


def idle_job(arguments):
    memory = hold(arguments.memory_mb)
    time.sleep(arguments.seconds)
    Path(arguments.out).write_text(json.dumps({"held_bytes": len(memory)}))


def user_stand_in(arguments):
    memory = hold(arguments.memory_mb)
    faults_before = resource.getrusage(resource.RUSAGE_SELF).ru_majflt
    responses = []
    deadline = time.monotonic() + arguments.seconds
    while time.monotonic() < deadline:
        asked = time.monotonic()
        time.sleep(USER_TICK_SECONDS)
        bytes(memory[::PAGE])
        responses.append((time.monotonic() - asked - USER_TICK_SECONDS) * 1000)
    responses.sort()
    Path(arguments.out).write_text(json.dumps({
        "response_ms_median": round(statistics.median(responses), 1),
        "response_ms_p95": round(responses[int(len(responses) * 0.95)], 1),
        "response_ms_max": round(responses[-1], 1),
        "responses_over_100ms": sum(response > 100 for response in responses),
        "page_ins": resource.getrusage(resource.RUSAGE_SELF).ru_majflt - faults_before,
    }))


def hold(megabytes):
    memory = bytearray(megabytes * 1024 * 1024)
    memory[::PAGE] = b"\x01" * len(memory[::PAGE])
    return memory


def compute(iterations):
    total = 0
    for number in range(iterations):
        total += number * number
    return total


def in_background(pid):
    priority = subprocess.run(["ps", "-o", "pri=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return priority == str(BACKGROUND_PRIORITY)


def running_pressure_guards():
    found = subprocess.run(["pgrep", "-x", "pressure-guard"], capture_output=True, text=True).stdout.split()
    watchdogs = subprocess.run(["pgrep", "-f", "pressure-guard watchdog"], capture_output=True, text=True).stdout.split()
    return len(set(found) - set(watchdogs))


def other_experiments_running():
    listed = subprocess.run(["launchctl", "list"], capture_output=True, text=True).stdout.split()
    return sum(word.startswith(EXPERIMENT_LABELS) and word != own_stand_in_label() for word in listed)


def free_memory_percent():
    return int(subprocess.run(["sysctl", "-n", "kern.memorystatus_level"], capture_output=True, text=True).stdout)


def vm_counters():
    lines = subprocess.run(["vm_stat"], capture_output=True, text=True).stdout.splitlines()
    readings = dict(line.split(":", 1) for line in lines if ":" in line)
    return {name.lower(): int(readings[name].strip().rstrip(".")) for name in VM_COUNTERS if name in readings}


def wait_for_file(path, timeout_seconds):
    deadline = time.monotonic() + timeout_seconds
    while not path.exists() and time.monotonic() < deadline:
        time.sleep(0.5)


if __name__ == "__main__":
    main()
