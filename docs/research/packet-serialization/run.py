"""Runs the NET-4 serialization measurements and writes results/<suite>.jsonl.

Usage: python3 -I run.py [sizes] [speed] [fuzz]   (all three when none is named)

Runs bench/target/release/serialization-bench with fixed arguments (no shell), pinned to one CPU. Fuzz runs get
a 4 GB address-space limit, so a format that trusts a length prefix and tries to allocate it fails visibly
instead of taking the machine down; an abort is recorded as such.
"""

import json
import os
import resource
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
BIN = HERE / "bench" / "target" / "release" / "serialization-bench"
RESULTS = HERE / "results"
FORMATS = ["current", "bitcode", "postcard", "rkyv", "flatbuffers"]


def limits():
    os.sched_setaffinity(0, {2})
    resource.setrlimit(resource.RLIMIT_AS, (4 << 30, 4 << 30))


def run(args, timeout):
    p = subprocess.run([str(BIN), *args], capture_output=True, text=True, timeout=timeout, preexec_fn=limits)
    return p.returncode, [json.loads(l) for l in p.stdout.splitlines() if l.startswith("{")], p.stderr


def main():
    suites = sys.argv[1:] or ["sizes", "speed", "fuzz"]
    RESULTS.mkdir(exist_ok=True)
    stamp = time.strftime("%Y-%m-%dT%H:%M:%S")
    for suite in suites:
        rows = []
        if suite == "fuzz":
            for f in FORMATS:
                code, out, err = run(["fuzz", f, "200000"], 900)
                rows += out or [{"suite": "fuzz", "format": f, "exit": code, "stderr": err[-500:]}]
        elif suite == "speed":
            # Three runs; each cell keeps the median and the spread, since a shared VM varies between runs.
            runs = []
            for _ in range(3):
                code, out, err = run(["speed"], 1800)
                if code != 0:
                    print(err, file=sys.stderr)
                runs.append(out)
            for cells in zip(*runs):
                row = dict(cells[0])
                for k in ("encode_ns", "decode_ns", "read_ns"):
                    v = sorted(c[k] for c in cells)
                    row[k] = v[1]
                    row[k + "_range"] = [v[0], v[2]]
                rows.append(row)
        else:
            code, rows, err = run([suite], 1800)
            if code != 0:
                print(err, file=sys.stderr)
        with open(RESULTS / f"{suite}.jsonl", "w") as out:
            for r in rows:
                r["at"] = stamp
                out.write(json.dumps(r) + "\n")
                print(json.dumps(r), flush=True)


if __name__ == "__main__":
    main()
