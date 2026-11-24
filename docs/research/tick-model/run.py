"""Runs the SRV-3 tick model measurements and writes results/<suite>.jsonl.

Usage: python3 -I run.py [check] [headline] [scaling] [sweep]   (all when none is named)

Runs bench/target/release/tick-bench with fixed arguments (no shell). Each run is its own process, pinned to as
many CPUs as it has threads (CPU 0 for the one-thread models), so every model gets the same cores.
"""

import json
import os
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
BIN = HERE / "bench" / "target" / "release" / "tick-bench"
RESULTS = HERE / "results"
MODELS = ["current", "soa", "soa-rayon", "zones", "hecs-rayon", "bevy"]
SINGLE = {"current", "soa"}
SCENARIOS = ["spread", "city", "crowd"]
RATES = [30, 60]
THREADS = 4
SWEEP = [512, 1024, 2048, 3072, 4096, 5120, 6144, 8192, 10240, 12288, 16384]


def run(args, cpus, timeout=900):
    p = subprocess.run([str(BIN), *map(str, args)], capture_output=True, text=True, timeout=timeout,
                       preexec_fn=lambda: os.sched_setaffinity(0, set(range(cpus))))
    if p.returncode != 0:
        print(p.stderr[-2000:], file=sys.stderr)
    return p.returncode, [json.loads(l) for l in p.stdout.splitlines() if l.startswith("{")]


def bench(model, scenario, players, rate, threads, secs):
    threads = 1 if model in SINGLE else threads
    code, rows = run(["run", model, scenario, players, rate, threads, secs], threads)
    return rows[0] if rows else {"model": model, "scenario": scenario, "players": players, "rate_hz": rate,
                                 "threads": threads, "exit": code}


def suite(name, stamp):
    rows = []
    if name == "check":
        # Every model must end in the same world, whatever the thread count.
        for scenario in SCENARIOS:
            for threads in (1, 2, 4):
                code, out = run(["check", scenario, 2048, 30, threads, 300], threads)
                rows += out + [{"check_exit": code, "scenario": scenario, "threads": threads}]
    elif name == "headline":
        # 4096 players, three rounds; each round runs every configuration once, so drift on the VM is shared.
        for rnd in range(3):
            for rate in RATES:
                for scenario in SCENARIOS:
                    for model in MODELS:
                        r = bench(model, scenario, 4096, rate, THREADS, 10)
                        r["round"] = rnd
                        rows.append(r)
                        print(json.dumps(r), flush=True)
    elif name == "scaling":
        for scenario in ("city", "crowd"):
            for model in ("soa-rayon", "zones", "hecs-rayon", "bevy"):
                for threads in (1, 2, 3, 4):
                    r = bench(model, scenario, 4096, 30, threads, 10)
                    rows.append(r)
                    print(json.dumps(r), flush=True)
    elif name == "sweep":
        # Growing player counts until p99 passes the budget (70% of the interval); one failing count is kept.
        for rate in RATES:
            for scenario in SCENARIOS:
                for model in MODELS:
                    for players in SWEEP:
                        r = bench(model, scenario, players, rate, THREADS, 10)
                        rows.append(r)
                        print(json.dumps(r), flush=True)
                        if r.get("p99_us", 1e12) > r.get("budget_us", 0):
                            break
    # The sweep is written one file per rate, so that no results file is too large for a pull request's diff.
    files = [(f"sweep-{rate}", [r for r in rows if r.get("rate_hz") == rate]) for rate in RATES] \
        if name == "sweep" else [(name, rows)]
    for file, part in files:
        with open(RESULTS / f"{file}.jsonl", "w") as out:
            for r in part:
                r["at"] = stamp
                out.write(json.dumps(r) + "\n")


def main():
    RESULTS.mkdir(exist_ok=True)
    stamp = time.strftime("%Y-%m-%dT%H:%M:%S")
    for name in sys.argv[1:] or ["check", "headline", "scaling", "sweep"]:
        suite(name, stamp)


if __name__ == "__main__":
    main()
