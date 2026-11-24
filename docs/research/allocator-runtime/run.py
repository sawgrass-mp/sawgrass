"""Runs the PERF-1 measurements and appends one JSON line per run to results/<suite>.jsonl.

Usage: python3 -I run.py gateway [--players 2048,4096] [--reps 3] [--runtimes tokio-mt,compio] [--allocators system]
       python3 -I run.py capacity [--players 1536,2048,2560,3072] [--runtimes ...] [--allocators ...]
       python3 -I run.py tick [--reps 3]
       python3 -I run.py ledger [--reps 2]

Starts bench/bin/runtime-bench-<allocator> (one build per allocator, see README.md) with fixed arguments (no
shell), pins the server and the load generator to separate CPUs, and parses their output lines. Repetitions are
interleaved (every variant once, then every variant again) so that drift on the VM does not favour one variant.
"""

import json
import os
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
BIN = HERE / "bench" / "bin"
RESULTS = HERE / "results"
ALLOCATORS = ["system", "mimalloc", "jemalloc"]
# Name, runtime argument, endpoints per server (None: one per thread, as the runtime decides).
GATEWAYS = {
    "tokio-mt": ("tokio-mt", 1),
    "tokio-mt-2ep": ("tokio-mt", 2),
    "tokio-tpc": ("tokio-tpc", None),
    "smol": ("smol", 1),
    "compio": ("compio", None),
}
ADDR = "127.0.0.1:7400"
MEASURE_SECS = 30


def binary(allocator):
    path = BIN / f"runtime-bench-{allocator}"
    if not path.exists():
        sys.exit(f"{path} is missing; build it first (README.md, Reproduce)")
    return str(path)


class Proc:
    """A child process whose output lines are collected with the time they arrived."""

    def __init__(self, args, cpus, env=None):
        self.lines = []
        full_env = dict(os.environ)
        full_env.update(env or {})
        self.p = subprocess.Popen(
            args,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            env=full_env,
            preexec_fn=lambda: os.sched_setaffinity(0, cpus),
        )
        self.t = threading.Thread(target=self._read, daemon=True)
        self.t.start()

    def _read(self):
        for line in self.p.stdout:
            self.lines.append((time.monotonic(), line.rstrip("\n")))

    def wait(self, timeout):
        self.p.wait(timeout=timeout)
        self.t.join(timeout=2)

    def stop(self):
        if self.p.poll() is None:
            self.p.terminate()
            try:
                self.p.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.p.kill()
                self.p.wait()
        self.t.join(timeout=2)


def fields(line):
    """Parses `word key=value key=value` into a dict of the key=value pairs."""
    out = {}
    for part in line.split():
        if "=" in part:
            k, v = part.split("=", 1)
            try:
                out[k] = float(v)
            except ValueError:
                out[k] = v
    return out


def last(lines, prefix):
    for _, line in reversed(lines):
        if line.startswith(prefix):
            return fields(line)
    return None


def append(suite, record):
    RESULTS.mkdir(exist_ok=True)
    record["at"] = time.strftime("%Y-%m-%dT%H:%M:%S")
    with open(RESULTS / f"{suite}.jsonl", "a") as f:
        f.write(json.dumps(record) + "\n")
    print(json.dumps(record), flush=True)


def gateway_run(suite, name, allocator, players, server_cpus, client_cpus, rep):
    runtime, endpoints = GATEWAYS[name]
    threads = len(server_cpus)
    args = [binary(allocator), "server", runtime, ADDR, str(threads)]
    if endpoints is not None:
        args.append(str(endpoints))
    # smol's global executor runs SMOL_THREADS threads; the others ignore it.
    server = Proc(args, server_cpus, env={"SMOL_THREADS": str(threads)})
    time.sleep(1.0)
    # The generator is the same build for every variant: Tokio and the system allocator.
    load = Proc(
        [binary("system"), "load", ADDR, str(players), str(len(client_cpus)), str(MEASURE_SECS)],
        client_cpus,
    )
    try:
        load.wait(timeout=180 + players * 0.01)
    except subprocess.TimeoutExpired:
        load.stop()
    server.stop()
    result = last(load.lines, "load ")
    connected_at = next((t for t, line in load.lines if line.startswith("connected ")), None)
    record = {
        "runtime": name,
        "allocator": allocator,
        "players": players,
        "server_cpus": len(server_cpus),
        "generator_cpus": len(client_cpus),
        "rep": rep,
        "secs": MEASURE_SECS,
    }
    driver = last(server.lines, "compio ")
    if driver:
        record["compio_driver"] = driver.get("driver")
    if result is None or connected_at is None:
        record["failed"] = True
        record["output"] = [line for _, line in (load.lines + server.lines)[-10:]]
        append(suite, record)
        return
    # Server reports cover the 5 s before they are printed; keep those inside the measured window.
    start, end = connected_at + 5.0, connected_at + 5.0 + MEASURE_SECS + 0.5
    reports = [fields(line) for t, line in server.lines if line.startswith("report ") and start + 5.0 <= t <= end]
    if reports:
        for key in ["cpu_cores", "busiest_thread_cores", "rss_mb", "recv_per_s", "sent_per_s"]:
            values = [r[key] for r in reports]
            record[f"server_{key}"] = round(sum(values) / len(values), 3)
        record["server_cpu_cores_max"] = max(r["cpu_cores"] for r in reports)
        record["server_rss_mb_max"] = max(r["rss_mb"] for r in reports)
    record["reports"] = len(reports)
    record.update({k: v for k, v in result.items() if k != "clients"})
    record["expected_recv_per_s"] = int(players * 21)
    sent = record.get("events_sent", 0)
    record["event_delivery_pct"] = round(100.0 * record.get("events_back", 0) / sent, 3) if sent else 0.0
    append(suite, record)


def options(argv):
    opts = {}
    i = 0
    while i < len(argv):
        if argv[i].startswith("--") and i + 1 < len(argv):
            opts[argv[i][2:]] = argv[i + 1]
            i += 2
        else:
            i += 1
    return opts


def listed(opts, key, default):
    return opts[key].split(",") if key in opts else default


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    suite, opts = sys.argv[1], options(sys.argv[2:])
    reps = int(opts.get("reps", "3"))
    allocators = listed(opts, "allocators", ALLOCATORS)
    if suite == "gateway":
        # Server on 2 CPUs, generator on the other 2, as in NET-3.
        runtimes = listed(opts, "runtimes", list(GATEWAYS))
        players = [int(p) for p in listed(opts, "players", ["2048", "4096"])]
        for rep in range(reps):
            for n in players:
                for name in runtimes:
                    for allocator in allocators:
                        gateway_run("gateway", name, allocator, n, {0, 1}, {2, 3}, rep)
    elif suite == "capacity":
        # Server on 1 CPU, generator on 3: the most players one server core carries, with the generator far
        # from its limit. One thread per server, so tokio-mt and tokio-mt-2ep are left out (2 endpoints on one
        # thread is not thread per core).
        runtimes = listed(opts, "runtimes", ["tokio-mt", "tokio-tpc", "smol", "compio"])
        players = [int(p) for p in listed(opts, "players", ["1536", "2048", "2560", "3072"])]
        for rep in range(int(opts.get("reps", "1"))):
            for n in players:
                for name in runtimes:
                    for allocator in allocators:
                        gateway_run("capacity", name, allocator, n, {0}, {1, 2, 3}, rep)
    elif suite == "tick":
        # The tick thread and the two gateway threads on 3 CPUs; 10 s of warm-up, then 60 s (1,200 ticks).
        for rep in range(reps):
            for allocator in allocators:
                p = Proc([binary(allocator), "tick", "4096", "200", "1200"], {0, 1, 2})
                p.wait(timeout=300)
                result = last(p.lines, "tick ") or {"failed": True}
                result["rep"] = rep
                append("tick", result)
    elif suite == "ledger":
        # Callers on 2 CPUs (2 runtime threads), the writer thread with them; RocksDB's own threads too.
        reps = int(opts.get("reps", "2"))
        with tempfile.TemporaryDirectory(dir=HERE / "bench") as tmp:
            for rep in range(reps):
                for mode in ["closed", "2000"]:
                    for runtime in listed(opts, "runtimes", ["tokio", "compio"]):
                        for allocator in allocators:
                            db = str(Path(tmp) / "db")
                            p = Proc(
                                [binary(allocator), "ledger", runtime, db, "64", mode, "30"],
                                {0, 1, 2},
                            )
                            p.wait(timeout=300)
                            result = last(p.lines, "ledger ") or {"failed": True, "runtime": runtime}
                            result["rep"] = rep
                            append("ledger", result)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
