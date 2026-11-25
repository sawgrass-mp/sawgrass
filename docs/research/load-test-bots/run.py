"""Runs the real Sawgrass server under the load-test bots and appends one JSON line per run to results/.

Usage: python3 -I run.py SUITE [--counts 1000,2000] [--areas 4000] [--repeats 3] [--secs 60]
                         [--server PATH] [--preload LIB] [--label NAME]

SUITE names the results file (results/SUITE.jsonl). `--server` runs another build of the server (for example
one with other code); `--preload` starts the server with LD_PRELOAD set to LIB, which replaces malloc in the
whole server process (for example /usr/lib/x86_64-linux-gnu/libjemalloc.so.2). The bots are always target/release/sawgrass-bots of this workspace. Build both
first with `cargo build --release -p sawgrass-server -p sawgrass-bots` at the workspace root.

The server is pinned to CPUs 0 and 1 with one gateway thread next to its tick thread, the bots to CPUs 2 and 3
with two runtime threads, as the bots README advises for one machine. Processes are started directly with fixed
arguments (no shell). Each run is parsed from the bots' "whole load phase" report. A run whose generator line
says SATURATED is recorded with "saturated": true and must not be quoted as a server figure.
"""

import json
import os
import re
import resource
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
BOTS = ROOT / "target" / "release" / "sawgrass-bots"
SERVER = ROOT / "target" / "release" / "sawgrass-server"
RESULTS = HERE / "results"
SERVER_CPUS = {0, 1}
BOT_CPUS = {2, 3}
ADDR = "127.0.0.1:4433"

NUM = r"([0-9.]+)"
PATTERNS = {
    "connected": r"bots (\d+)/\d+ connected \((\d+) failed, (\d+) dropped\)",
    "tick": rf"tick p50 {NUM} ms, p99 {NUM} ms .*max {NUM} ms, (\d+) overruns",
    "cpu": rf"server: cpu {NUM} cores \(tick thread {NUM}",
    "server_bytes": rf"down {NUM} kB/s and up {NUM} kB/s per player",
    "bots": rf"bots: down {NUM} kB/s per bot in {NUM} datagrams/s, {NUM} ticks/s with a snapshot, {NUM} entities",
    "generator": rf"generator: cpu {NUM} of \d+ cores.*: (\w+)",
}


def parse(report):
    """Turns the whole-load-phase lines into a flat dict of numbers."""
    found = {k: re.search(p, report) for k, p in PATTERNS.items()}
    missing = [k for k, m in found.items() if m is None]
    if missing:
        return {"error": "missing " + ",".join(missing)}
    g = {k: m.groups() for k, m in found.items()}
    return {
        "connected": int(g["connected"][0]),
        "failed": int(g["connected"][1]),
        "dropped": int(g["connected"][2]),
        "tick_p50_ms": float(g["tick"][0]),
        "tick_p99_ms": float(g["tick"][1]),
        "tick_max_ms": float(g["tick"][2]),
        "overruns": int(g["tick"][3]),
        "server_cores": float(g["cpu"][0]),
        "tick_thread_cores": float(g["cpu"][1]),
        "network_cores": round(float(g["cpu"][0]) - float(g["cpu"][1]), 3),
        "down_kb_s_per_player": float(g["server_bytes"][0]),
        "up_kb_s_per_player": float(g["server_bytes"][1]),
        "bot_datagrams_s": float(g["bots"][1]),
        "bot_ticks_s": float(g["bots"][2]),
        "entities_per_tick": float(g["bots"][3]),
        "generator_cores": float(g["generator"][0]),
        "saturated": g["generator"][1] != "ok",
    }


def pinned(cpus):
    return lambda: os.sched_setaffinity(0, cpus)


def one_run(server_bin, preload, cert_dir, count, area, secs, seed):
    env = dict(os.environ)
    if preload:
        env["LD_PRELOAD"] = preload
    server = subprocess.Popen(
        [
            str(server_bin), "--listen", ADDR,
            "--cert", str(cert_dir / "cert.der"), "--key", str(cert_dir / "key.der"),
            "--stats-to-clients", "--net-threads", "1",
            # Every bot connects from this machine's one address, and the server allows 16 per address (NET-12).
            "--max-per-address", str(count),
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=env,
        preexec_fn=pinned(SERVER_CPUS),
    )
    try:
        time.sleep(1)
        bots = subprocess.run(
            [
                str(BOTS), "run", "--server", ADDR, "--cert", str(cert_dir / "cert.der"),
                "--count", str(count), "--area", str(area), "--duration", str(secs),
                "--report-every", str(secs), "--threads", "2", "--seed", str(seed),
            ],
            capture_output=True,
            text=True,
            timeout=secs + 120,
            preexec_fn=pinned(BOT_CPUS),
        )
    finally:
        server.terminate()
        server.wait(timeout=10)
    report = bots.stdout.split("== whole load phase", 1)[-1]
    return parse(report)


def main():
    args = sys.argv[1:]
    if not args or args[0].startswith("--"):
        print(__doc__)
        return 2
    suite = args[0]
    opts = {"--counts": "1000", "--areas": "4000", "--repeats": "3", "--secs": "60",
            "--server": str(SERVER), "--preload": "", "--label": "main"}
    i = 1
    while i < len(args):
        if args[i] not in opts or i + 1 >= len(args):
            print(f"unknown or incomplete option {args[i]}")
            return 2
        opts[args[i]] = args[i + 1]
        i += 2
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    resource.setrlimit(resource.RLIMIT_NOFILE, (min(20000, hard), hard))
    RESULTS.mkdir(exist_ok=True)
    out = RESULTS / f"{suite}.jsonl"
    with tempfile.TemporaryDirectory() as tmp:
        cert_dir = Path(tmp)
        subprocess.run([str(BOTS), "gen-cert", "--out", str(cert_dir)], check=True, capture_output=True)
        for count in [int(c) for c in opts["--counts"].split(",")]:
            for area in [int(a) for a in opts["--areas"].split(",")]:
                for repeat in range(int(opts["--repeats"])):
                    row = {"label": opts["--label"], "bots": count, "area_m": area, "secs": int(opts["--secs"]),
                           "repeat": repeat}
                    row.update(one_run(Path(opts["--server"]), opts["--preload"], cert_dir, count, area,
                                       int(opts["--secs"]), seed=repeat + 1))
                    row["at"] = time.strftime("%Y-%m-%dT%H:%M:%S")
                    print(json.dumps(row), flush=True)
                    with out.open("a") as f:
                        f.write(json.dumps(row) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
