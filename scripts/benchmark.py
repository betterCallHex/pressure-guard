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
COMMAND_ROUNDS = 5


def main():
    parser = argparse.ArgumentParser(description="Agent-like load beside a stand-in for your app, measuring both.")
    roles = parser.add_subparsers(dest="role", required=True)
    run = roles.add_parser("run", help="run one benchmark round and print its result as JSON")
    run.add_argument("--label", default="run")
    run.add_argument("--busy", type=int, default=6, help="agent jobs that compute and touch their memory nonstop")
    run.add_argument("--idle", type=int, default=6, help="agent jobs that hold memory and sleep, like watchers")
    run.add_argument("--memory-mb", type=int, default=400, help="memory each agent job holds")
    run.add_argument("--user-memory-mb", type=int, default=1024, help="memory your stand-in app holds")
    run.add_argument("--seconds", type=int, default=60)
    for role in ("busy", "idle", "user"):
        job = roles.add_parser(role)
        job.add_argument("--memory-mb", type=int, required=True)
        job.add_argument("--seconds", type=float, required=True)
        job.add_argument("--out", required=True)
    roles.add_parser("command").add_argument("--memory-mb", type=int, required=True)
    arguments = parser.parse_args()
    jobs = {"run": run_round, "busy": busy_job, "command": command_job, "idle": idle_job, "user": user_stand_in}
    jobs[arguments.role](arguments)


def run_round(arguments):
    leave_background_mode()
    workspace = Path(tempfile.mkdtemp(prefix="pressure-guard-benchmark-"))
    user_out = workspace / "user.json"
    label = f"pressure-guard.benchmark.{os.getpid()}"
    swapins_before = swapins()
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
        "agent_rounds_per_second": round(busy_rounds / arguments.seconds, 2),
        "user": json.loads(user_out.read_text()),
        "swapins": swapins() - swapins_before,
        **samples,
    }))


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
    background_share = {"busy": [], "idle": []}
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        time.sleep(2)
        lowest_free = min(lowest_free, free_memory_percent())
        most_pressure_guards = max(most_pressure_guards, running_pressure_guards())
        for role in background_share:
            pids = [process.pid for process, job_role, _ in jobs if job_role == role]
            if role == "busy":
                pids = [command for parent in pids for command in children(parent)]
            if pids:
                background_share[role].append(sum(map(in_background, pids)) / len(pids))
    return {
        "lowest_free_memory_percent": lowest_free,
        "pressure_guards_running": most_pressure_guards,
        "busy_jobs_in_background": round(statistics.fmean(background_share["busy"] or [0]), 2),
        "idle_jobs_in_background": round(statistics.fmean(background_share["idle"] or [0]), 2),
    }


def busy_job(arguments):
    command = [sys.executable, str(Path(__file__).resolve()), "command", "--memory-mb", str(arguments.memory_mb)]
    deadline = time.monotonic() + arguments.seconds
    rounds = 0
    while time.monotonic() < deadline:
        subprocess.run(command, check=True)
        rounds += COMMAND_ROUNDS
    Path(arguments.out).write_text(json.dumps({"rounds": rounds}))


def command_job(arguments):
    memory = hold(arguments.memory_mb)
    pages = len(memory) // PAGE
    for round_number in range(COMMAND_ROUNDS):
        memory[::PAGE] = bytes([round_number]) * pages
        compute(CPU_ROUND)


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


def children(pid):
    return [int(child) for child in subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True).stdout.split()]


def in_background(pid):
    priority = subprocess.run(["ps", "-o", "pri=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return priority == str(BACKGROUND_PRIORITY)


def running_pressure_guards():
    found = subprocess.run(["pgrep", "-x", "pressure-guard"], capture_output=True, text=True).stdout.split()
    watchdogs = subprocess.run(["pgrep", "-f", "pressure-guard watchdog"], capture_output=True, text=True).stdout.split()
    return len(set(found) - set(watchdogs))


def free_memory_percent():
    return int(subprocess.run(["sysctl", "-n", "kern.memorystatus_level"], capture_output=True, text=True).stdout)


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
