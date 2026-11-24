"""Runs the NET-5 compression measurements and writes results/<suite>.jsonl.

Usage: python3 -I run.py [sizes] [loss] [speed] [fuzz] [quinn]   (all when none is named)

Runs bench/target/release/compression-bench with fixed arguments (no shell), pinned to one CPU. Fuzz runs get a
4 GB address-space limit, so a decoder that trusts a size field and tries to allocate it fails visibly. The quinn
suite runs the NET-3 load test (../network-transport/run.py) with snapshots of the sizes the sizes suite measured;
build ../network-transport/bench first.
"""

import json
import math
import os
import resource
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
BIN = HERE / "bench" / "target" / "release" / "compression-bench"
TRANSPORT_RUN = HERE.parent / "network-transport" / "run.py"
RESULTS = HERE / "results"
FUZZ = [
    "full/none", "full/lz4", "full/zstd-3", "full/zstd-3-dict16k",
    "delta/none", "delta/lz4", "delta/zstd-3", "delta/zstd-3-dict16k",
]
# Snapshot pipelines sent through quinn: (label, encoding, codec).
QUINN = [
    ("full", "full", "none"),
    ("full+zstd-dict", "full", "zstd-3-dict16k"),
    ("delta", "delta", "none"),
    ("delta+zstd-dict", "delta", "zstd-3-dict16k"),
]
DATAGRAM = 1414
FRAGMENT_HEADER = 3


def limits():
    os.sched_setaffinity(0, {2})
    resource.setrlimit(resource.RLIMIT_AS, (4 << 30, 4 << 30))


def run(args, timeout):
    p = subprocess.run([str(BIN), *args], capture_output=True, text=True, timeout=timeout, preexec_fn=limits)
    if p.returncode != 0:
        print(p.stderr[-2000:], file=sys.stderr)
    return p.returncode, [json.loads(l) for l in p.stdout.splitlines() if l.startswith("{")], p.stderr


def write(suite, rows, stamp):
    with open(RESULTS / f"{suite}.jsonl", "w") as out:
        for r in rows:
            r["at"] = stamp
            out.write(json.dumps(r) + "\n")
            print(json.dumps(r), flush=True)


def quinn(stamp):
    """Gateway CPU with quinn at 2,048 and 4,096 players, sending each pipeline's mean snapshot size."""
    sizes = {(r["encoding"], r["codec"]): r for r in map(json.loads, open(RESULTS / "sizes.jsonl"))
             if r["suite"] == "size"}
    out = RESULTS / "quinn.jsonl"
    out.unlink(missing_ok=True)
    for label, enc, codec in QUINN:
        mean = sizes[(enc, codec)]["mean_bytes"]
        # Split into equal datagrams when it does not fit in one, each with its fragment header.
        count = 1 if mean <= DATAGRAM else math.ceil(mean / (DATAGRAM - FRAGMENT_HEADER))
        each = math.ceil(mean / count) + (FRAGMENT_HEADER if count > 1 else 0)
        subprocess.run(
            [sys.executable, "-I", str(TRANSPORT_RUN), "load", "quinn", "--sizes", "2048,4096",
             "--snapshot", f"{each}x{count}", "--label", label, "--results", str(out)],
            check=True, timeout=3600,
        )


def main():
    suites = sys.argv[1:] or ["sizes", "loss", "speed", "fuzz", "quinn"]
    RESULTS.mkdir(exist_ok=True)
    stamp = time.strftime("%Y-%m-%dT%H:%M:%S")
    for suite in suites:
        if suite == "quinn":
            quinn(stamp)
            continue
        rows = []
        if suite == "fuzz":
            for m in FUZZ:
                code, out, err = run(["fuzz", m, "100000"], 1800)
                rows += out or [{"suite": "fuzz", "method": m, "exit": code, "stderr": err[-500:]}]
        elif suite == "speed":
            # Three runs; each cell keeps the median and the spread, since a shared VM varies between runs.
            runs = [run(["speed"], 1800)[1] for _ in range(3)]
            for cells in zip(*runs):
                row = dict(cells[0])
                for k in ("encode_ns", "decode_ns"):
                    v = sorted(c[k] for c in cells)
                    row[k] = v[1]
                    row[k + "_range"] = [v[0], v[2]]
                rows.append(row)
        else:
            rows = run([suite], 1800)[1]
        write(suite, rows, stamp)


if __name__ == "__main__":
    main()
