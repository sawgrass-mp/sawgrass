"""Runs the SCRIPT-1 scripting engine measurements and writes results/<suite>.jsonl.

Usage: python3 -I run.py [build] [verify] [speed] [latency] [memory] [isolation] [size]   (all when none is named)

Builds the WebAssembly guest and one bench binary per engine (mlua's Lua flavours exclude each other), then runs
every suite in a fresh process with fixed arguments (no shell), pinned to one CPU. Isolation runs get a 4 GB data
limit and a 30 s timeout, so an engine that cannot stop a script, or that takes the process down, shows as such.
"""

import json
import os
import resource
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
BENCH = HERE / "bench"
GUEST = HERE / "guest"
RESULTS = HERE / "results"
ENGINES = ["luau", "luau-jit", "lua54", "luajit", "luajit-interp", "quickjs", "v8", "wasm", "rhai"]
CASES = ["spin", "bomb", "recurse", "fail"]
CPU = 2
ISOLATION_TIMEOUT = 30
# Crates whose versions the page cites, read from bench/Cargo.lock and guest/Cargo.lock.
CRATES = ["mlua", "mlua-sys", "luau0-src", "lua-src", "luajit-src", "wasmtime", "rquickjs", "rquickjs-sys", "v8", "rhai"]


def binary(engine):
    return BENCH / "target" / engine / "release" / "scripting-bench"


def build():
    subprocess.run(["cargo", "build", "--release", "--target", "wasm32-unknown-unknown"], cwd=GUEST, check=True)
    for e in ENGINES:
        subprocess.run(
            ["cargo", "build", "--release", "--features", e, "--target-dir", str(BENCH / "target" / e)],
            cwd=BENCH, check=True,
        )


def pinned():
    os.sched_setaffinity(0, {CPU})


def limited():
    pinned()
    resource.setrlimit(resource.RLIMIT_DATA, (4 << 30, 4 << 30))


def run(engine, args, timeout=1800, preexec=pinned):
    p = subprocess.run([str(binary(engine)), *args], capture_output=True, text=True, timeout=timeout,
                       preexec_fn=preexec)
    if p.returncode != 0:
        print(f"{engine} {args}: exit {p.returncode}: {p.stderr[-500:]}", file=sys.stderr)
    return p, [json.loads(l) for l in p.stdout.splitlines() if l.startswith("{")]


def write(suite, rows, stamp):
    with open(RESULTS / f"{suite}.jsonl", "w") as out:
        for r in rows:
            r["at"] = stamp
            out.write(json.dumps(r) + "\n")
            print(json.dumps(r), flush=True)


def versions():
    found = {}
    for lock in (BENCH / "Cargo.lock",):
        name = None
        for line in lock.read_text().splitlines():
            if line.startswith("name = "):
                name = line.split('"')[1]
            elif line.startswith("version = ") and name in CRATES:
                found[name] = line.split('"')[1]
    rustc = subprocess.run(["rustc", "--version"], capture_output=True, text=True, cwd=BENCH).stdout.strip()
    return {"suite": "versions", "rustc": rustc, "crates": found, "cpu": cpu_model(), "cores": os.cpu_count()}


def cpu_model():
    for line in Path("/proc/cpuinfo").read_text().splitlines():
        if line.startswith("model name"):
            return line.split(":", 1)[1].strip()
    return "unknown"


def size(engine):
    """Size of the bench binary with symbols stripped (strip from binutils), the engine plus a small harness."""
    out = RESULTS / f".{engine}.stripped"
    subprocess.run(["strip", "-o", str(out), str(binary(engine))], check=True)
    n = out.stat().st_size
    out.unlink()
    return {"suite": "size", "engine": engine, "stripped_bytes": n}


def isolation():
    rows = []
    for e in ENGINES:
        for case in CASES:
            try:
                p, out = run(e, ["isolation", case], timeout=ISOLATION_TIMEOUT, preexec=limited)
                if out:
                    rows.append(out[0])
                else:
                    # The process died before it could report: record how.
                    how = f"signal {-p.returncode}" if p.returncode < 0 else f"exit {p.returncode}"
                    rows.append({"suite": "isolation", "engine": e, "case": case, "outcome": "process died",
                                 "message": f"{how}: {p.stderr.strip()[-200:]}", "usable_after": False})
            except subprocess.TimeoutExpired:
                rows.append({"suite": "isolation", "engine": e, "case": case, "outcome": "hung",
                             "message": f"still running after {ISOLATION_TIMEOUT} s; killed",
                             "usable_after": False})
    return rows


def main():
    suites = sys.argv[1:] or ["build", "verify", "speed", "latency", "memory", "isolation", "size"]
    RESULTS.mkdir(exist_ok=True)
    stamp = time.strftime("%Y-%m-%dT%H:%M:%S")
    if "build" in suites:
        build()
        write("versions", [versions()], stamp)
    if "verify" in suites:
        rows = [r for e in ENGINES for r in run(e, ["verify"])[1]]
        keys = ["noop", "host_loop", "echo", "zone", "chat", "payday", "inventory", "checksum"]
        reference = {k: rows[0][k] for k in keys}
        for r in rows:
            r["agrees"] = all(r[k] == reference[k] for k in keys)
        write("verify", rows, stamp)
        if not all(r["agrees"] for r in rows):
            sys.exit("engines disagree; see results/verify.jsonl")
    if "speed" in suites:
        # Three passes over all engines, interleaved so that a slow period of the machine hits every engine; the
        # page and chart.py use the median of the three per workload.
        rows = []
        for n in range(3):
            for e in ENGINES:
                for args in (["speed"], ["speed", "--no-limits"]):
                    for r in run(e, args)[1]:
                        r["pass"] = n
                        rows.append(r)
        write("speed", rows, stamp)
    if "latency" in suites:
        write("latency", [r for e in ENGINES for r in run(e, ["latency"])[1]], stamp)
    if "memory" in suites:
        # Three runs per engine; the median per-instance figure is kept by chart.py and the page.
        write("memory", [r for e in ENGINES for _ in range(3) for r in run(e, ["memory"])[1]], stamp)
    if "isolation" in suites:
        write("isolation", isolation(), stamp)
    if "size" in suites:
        write("size", [size(e) for e in ENGINES], stamp)


if __name__ == "__main__":
    main()
