#!/usr/bin/env python3
import argparse
import ctypes
import itertools
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
MEGABYTE = 1024 * 1024
SAVED_DOCUMENT = bytes(64 * 1024)
ROUND_DONE = b"."
PRIO_DARWIN_PROCESS = 4
USER_TICK_SECONDS = 0.1
CPU_ROUND = 300_000
COMMAND_ROUNDS = 5


def main():
    parser = argparse.ArgumentParser(description="Agent-like load beside a stand-in for your app, measuring both.")
    roles = parser.add_subparsers(dest="role", required=True)
    run = roles.add_parser("run", help="run one benchmark round and print its result as JSON")
    run.add_argument("--label", default="run")
    run.add_argument("--busy", type=int, default=6, help="agent jobs that compute and touch their memory nonstop")
    run.add_argument("--idle", type=int, default=6, help="agent jobs that hold memory and sleep, like watchers")
    run.add_argument("--memory-mb", type=int, default=400, help="memory each agent job holds")
    run.add_argument("--overcommit-mb", type=int, help="instead of --memory-mb: size the agent jobs so that, with your stand-in app, they hold this much more than is free when the round starts")
    run.add_argument("--rounds-per-command", type=int, default=COMMAND_ROUNDS, help="how often each command a busy job runs works through its memory before it exits")
    run.add_argument("--user-memory-mb", type=int, default=1024, help="memory your stand-in app holds")
    run.add_argument("--seconds", type=int, default=60)
    for role in ("busy", "idle", "user"):
        job = roles.add_parser(role)
        job.add_argument("--memory-mb", type=int, required=True)
        job.add_argument("--seconds", type=float, required=True)
        job.add_argument("--out", required=True)
    roles.choices["busy"].add_argument("--rounds-per-command", type=int, required=True)
    command = roles.add_parser("command")
    command.add_argument("--memory-mb", type=int, required=True)
    command.add_argument("--rounds", type=int, required=True)
    command.add_argument("--until", type=float, required=True)
    command.add_argument("--progress", required=True)
    arguments = parser.parse_args()
    jobs = {"run": run_round, "busy": busy_job, "command": command_job, "idle": idle_job, "user": user_stand_in}
    jobs[arguments.role](arguments)


def run_round(arguments):
    leave_background_mode()
    workspace = Path(tempfile.mkdtemp(prefix="pressure-guard-benchmark-"))
    user_out = workspace / "user.json"
    label = f"pressure-guard.benchmark.{os.getpid()}"
    swapins_before = swapins()
    free_at_start = free_memory_percent()
    arguments.memory_mb = megabytes_per_agent_job(arguments, free_at_start)
    start_user_stand_in(label, arguments, user_out)
    jobs = [start_job(role, index, arguments, workspace) for role, count in (("busy", arguments.busy), ("idle", arguments.idle)) for index in range(count)]
    samples = sample_while_running(jobs, arguments.seconds)
    for process, _, _ in jobs:
        process.wait()
    wait_for_file(user_out, timeout_seconds=arguments.seconds)
    stop_user_stand_in(label)
    busy_rounds = sum(json.loads(out.read_text())["rounds"] for _, role, out in jobs if role == "busy")
    print(json.dumps({
        "label": arguments.label,
        "free_memory_percent_at_start": free_at_start,
        "megabytes_per_agent_job": arguments.memory_mb,
        "agent_rounds_per_second": round(busy_rounds / arguments.seconds, 2),
        "user": json.loads(user_out.read_text()),
        "swapins": swapins() - swapins_before,
        **samples,
    }))


def megabytes_per_agent_job(arguments, free_percent):
    if arguments.overcommit_mb is None:
        return arguments.memory_mb
    free_megabytes = total_memory_megabytes() * free_percent // 100
    for_agent_jobs = free_megabytes + arguments.overcommit_mb - arguments.user_memory_mb
    return max(1, for_agent_jobs // (arguments.busy + arguments.idle))


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
    if role == "busy":
        command += ["--rounds-per-command", str(arguments.rounds_per_command)]
    return subprocess.Popen(command), role, out


def sample_while_running(jobs, seconds):
    lowest_free = 100
    swap_at_start = swap_used_megabytes()
    most_swap = swap_at_start
    most_pressure_guards = 0
    background_share = {"busy": [], "idle": []}
    waiting_share = []
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        time.sleep(2)
        lowest_free = min(lowest_free, free_memory_percent())
        most_swap = max(most_swap, swap_used_megabytes())
        most_pressure_guards = max(most_pressure_guards, running_pressure_guards())
        for role in background_share:
            pids = [process.pid for process, job_role, _ in jobs if job_role == role]
            if role == "busy":
                pids = [command for parent in pids for command in children(parent)]
                waiting_share.append(sum(map(waits_its_turn, pids)) / max(1, len(pids)))
            if pids:
                background_share[role].append(sum(map(in_background, pids)) / len(pids))
    return {
        "lowest_free_memory_percent": lowest_free,
        "swap_grew_megabytes": round(most_swap - swap_at_start),
        "pressure_guards_running": most_pressure_guards,
        "busy_jobs_in_background": round(statistics.fmean(background_share["busy"] or [0]), 2),
        "busy_jobs_waiting_their_turn": round(statistics.fmean(waiting_share or [0]), 2),
        "idle_jobs_in_background": round(statistics.fmean(background_share["idle"] or [0]), 2),
    }


def busy_job(arguments):
    progress = Path(arguments.out).with_suffix(".rounds")
    progress.touch()
    until = time.time() + arguments.seconds
    command = [sys.executable, str(Path(__file__).resolve()), "command", "--memory-mb", str(arguments.memory_mb),
               "--rounds", str(arguments.rounds_per_command), "--until", str(until), "--progress", str(progress)]
    while time.time() < until:
        subprocess.run(command, check=True)
    Path(arguments.out).write_text(json.dumps({"rounds": progress.stat().st_size}))


def command_job(arguments):
    memory = hold(arguments.memory_mb)
    pages = len(memory) // PAGE
    rounds_in_time = itertools.takewhile(lambda _: time.time() < arguments.until, range(arguments.rounds))
    with open(arguments.progress, "ab", buffering=0) as progress:
        for round_number in rounds_in_time:
            memory[::PAGE] = bytes([round_number % 256]) * pages
            compute(CPU_ROUND)
            progress.write(ROUND_DONE)


def idle_job(arguments):
    memory = hold(arguments.memory_mb)
    time.sleep(arguments.seconds)
    Path(arguments.out).write_text(json.dumps({"held_bytes": len(memory)}))


def user_stand_in(arguments):
    memory = hold(arguments.memory_mb)
    document = Path(arguments.out).with_name("document")
    faults_before = resource.getrusage(resource.RUSAGE_SELF).ru_majflt
    responses = []
    slow_at_seconds = []
    saves = []
    started = time.monotonic()
    deadline = started + arguments.seconds
    while time.monotonic() < deadline:
        asked = time.monotonic()
        time.sleep(USER_TICK_SECONDS)
        bytes(memory[::PAGE])
        responses.append((time.monotonic() - asked - USER_TICK_SECONDS) * 1000)
        if responses[-1] > 100:
            slow_at_seconds.append(round(asked - started))
        saves.append(milliseconds_to_save(document))
    responses.sort()
    saves.sort()
    Path(arguments.out).write_text(json.dumps({
        "response_ms_median": round(statistics.median(responses), 1),
        "response_ms_p95": round(responses[int(len(responses) * 0.95)], 1),
        "response_ms_max": round(responses[-1], 1),
        "responses_over_100ms": len(slow_at_seconds),
        "responses_over_100ms_at_seconds": sorted(set(slow_at_seconds)),
        "save_ms_median": round(statistics.median(saves), 1),
        "save_ms_max": round(saves[-1], 1),
        "saves_over_100ms": sum(save > 100 for save in saves),
        "page_ins": resource.getrusage(resource.RUSAGE_SELF).ru_majflt - faults_before,
    }))


def milliseconds_to_save(document):
    started = time.monotonic()
    draft = document.with_suffix(".draft")
    with draft.open("wb") as file:
        file.write(SAVED_DOCUMENT)
        file.flush()
        os.fsync(file.fileno())
    draft.replace(document)
    return (time.monotonic() - started) * 1000


def hold(megabytes):
    memory = bytearray(megabytes * MEGABYTE)
    megabyte = megabyte_that_compresses_to_half()
    for start in range(0, len(memory), MEGABYTE):
        memory[start:start + MEGABYTE] = megabyte
    return memory


def megabyte_that_compresses_to_half():
    page = os.urandom(PAGE // 2) + bytes(PAGE // 2)
    return page * (MEGABYTE // PAGE)


def compute(iterations):
    total = 0
    for number in range(iterations):
        total += number * number
    return total


def children(pid):
    return [int(child) for child in subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True).stdout.split()]


def in_background(pid):
    return ctypes.CDLL(None).getpriority(PRIO_DARWIN_PROCESS, pid) == 1


def waits_its_turn(pid):
    state = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True).stdout
    return state.startswith("T")


def running_pressure_guards():
    found = subprocess.run(["pgrep", "-x", "pressure-guard"], capture_output=True, text=True).stdout.split()
    watchdogs = subprocess.run(["pgrep", "-f", "pressure-guard watchdog"], capture_output=True, text=True).stdout.split()
    return len(set(found) - set(watchdogs))


def free_memory_percent():
    return int(subprocess.run(["sysctl", "-n", "kern.memorystatus_level"], capture_output=True, text=True).stdout)


def total_memory_megabytes():
    return int(subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True).stdout) // MEGABYTE


def swap_used_megabytes():
    usage = subprocess.run(["sysctl", "-n", "vm.swapusage"], capture_output=True, text=True).stdout
    return float(usage.split("used = ")[1].split("M")[0])


def swapins():
    for line in subprocess.run(["vm_stat"], capture_output=True, text=True).stdout.splitlines():
        if line.startswith("Swapins"):
            return int(line.split(":")[1].strip().rstrip("."))
    return 0


def wait_for_file(path, timeout_seconds):
    deadline = time.monotonic() + timeout_seconds
    while not path.exists() and time.monotonic() < deadline:
        time.sleep(0.5)


if __name__ == "__main__":
    main()
