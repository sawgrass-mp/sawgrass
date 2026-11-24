"""Runs the IM-2 visibility index measurements and writes results/<suite>.jsonl.

Usage: python3 -I run.py [verify] [single] [threads] [repeat]   (all when none is named)

Runs bench/target/release/visibility-index-bench with fixed arguments (no shell), one process per run so each
starts with a fresh heap. Single-thread runs are pinned to one CPU; the threads suite gets all four.

- verify: every index against brute force in every scenario.
- single: every index, scenario, mode (radius, nearest 128) and visible-set layout (sorted, bits), one thread.
- threads: the main candidates with four threads.
- repeat: the headline rows twice more, to show how much a run varies.
"""

import json
import os
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
BIN = HERE / "bench" / "target" / "release" / "visibility-index-bench"
RESULTS = HERE / "results"
INDEXES = [
    "current", "grid-current", "grid-dense-25", "grid-dense-50", "grid-dense-100", "quadtree", "bvh",
    "rstar-bulk", "rstar-insert", "kiddo-immutable", "kiddo-mutable", "brute",
]
SCENARIOS = ["spread", "city", "crowd-1km", "event-512", "event-2048", "spawn-512"]
MODES = ["radius", "nearest-128"]
TICKS = "200"
CANDIDATES = ["grid-current", "grid-dense-25", "grid-dense-50", "bvh", "kiddo-immutable"]
HEADLINE = [
    ("current", "sorted"), ("grid-current", "bits"), ("grid-dense-50", "bits"), ("grid-dense-25", "bits"),
    ("kiddo-immutable", "bits"), ("bvh", "bits"), ("quadtree", "bits"), ("rstar-bulk", "bits"),
]


def run(args, cpus):
    p = subprocess.run(
        [str(BIN), *args], capture_output=True, text=True, timeout=3600,
        preexec_fn=lambda: os.sched_setaffinity(0, cpus),
    )
    if p.returncode != 0:
        print(p.stderr[-2000:], file=sys.stderr)
        return [{"args": args, "exit": p.returncode, "stderr": p.stderr[-500:]}]
    return [json.loads(line) for line in p.stdout.splitlines() if line.startswith("{")]


def write(suite, rows, stamp):
    with open(RESULTS / f"{suite}.jsonl", "w") as out:
        for r in rows:
            r.setdefault("at", stamp)
            out.write(json.dumps(r) + "\n")


def main():
    suites = sys.argv[1:] or ["verify", "single", "threads", "repeat"]
    RESULTS.mkdir(exist_ok=True)
    stamp = time.strftime("%Y-%m-%dT%H:%M:%S")
    one, four = {2}, {0, 1, 2, 3}
    for suite in suites:
        rows = []
        if suite == "verify":
            for s in SCENARIOS:
                rows += run(["verify", s, "20"], one)
        elif suite == "single":
            for s in SCENARIOS:
                for m in MODES:
                    for i in INDEXES:
                        for sets in ["sorted", "bits"]:
                            if i == "current" and (m != "radius" or sets != "sorted"):
                                continue  # the server's own code has one mode and one layout
                            rows += run(["run", i, s, m, "1", TICKS, sets], one)
                            print(json.dumps(rows[-1]), flush=True)
        elif suite == "threads":
            for s in SCENARIOS:
                for m in MODES:
                    for i in CANDIDATES:
                        rows += run(["run", i, s, m, "4", TICKS, "bits"], four)
                        print(json.dumps(rows[-1]), flush=True)
                    if m == "radius":
                        rows += run(["run", "current", s, m, "4", TICKS, "sorted"], four)
        elif suite == "repeat":
            for _ in range(2):
                for s in SCENARIOS:
                    for m in MODES:
                        for i, sets in HEADLINE:
                            if i == "current" and m != "radius":
                                continue
                            rows += run(["run", i, s, m, "1", TICKS, sets], one)
        else:
            sys.exit(f"unknown suite {suite}")
        if suite == "single":
            # Two files, one per query, so that each stays small enough for a pull request diff.
            write("single-radius", [r for r in rows if r.get("mode") == "radius"], stamp)
            write("single-nearest", [r for r in rows if r.get("mode") != "radius"], stamp)
        else:
            write(suite, rows, stamp)


if __name__ == "__main__":
    main()
