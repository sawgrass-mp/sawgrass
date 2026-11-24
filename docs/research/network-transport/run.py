"""Runs the NET-3 transport measurements and appends one JSON line per run to results/<suite>.jsonl.

Usage: python3 -I run.py latency|loss|load [transport ...] [--sizes 1000,4096]
       python3 -I run.py load quinn --sizes 4096 --snapshot 993x2 --label full --results PATH

`--snapshot BYTESxCOUNT` sends COUNT snapshot datagrams of BYTES each per client per tick instead of 2 x 1000;
NET-5 uses it with `--results` (a JSON-lines file to append to) and `--label` to measure compressed sizes.

Starts the server, the lossy relay and the client from bench/target*/release/transport-bench with fixed
arguments (no shell), pins them to CPUs so the server never shares a core with the load generator, and parses
their output lines. See README.md for what each suite measures.
"""

import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
BIN = HERE / "bench" / "target" / "release" / "transport-bench"
BIN_GNS = HERE / "bench" / "target-gns" / "release" / "transport-bench"
RESULTS = HERE / "results"
TRANSPORTS = ["udp", "quinn", "enet", "renet", "gns"]
SERVER_CPUS = {0, 1}
CLIENT_CPUS = {2, 3}
RELAY_CPUS = {3}


def binary(kind):
    return str(BIN_GNS if kind == "gns" else BIN)


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


# Where `save` writes, and what it adds to each record; `--results` and `--label` change them.
OUTPUT = {"path": None, "extra": {}}


def save(suite, record):
    RESULTS.mkdir(exist_ok=True)
    record.update(OUTPUT["extra"])
    record["at"] = time.strftime("%Y-%m-%dT%H:%M:%S")
    with open(OUTPUT["path"] or RESULTS / f"{suite}.jsonl", "a") as f:
        f.write(json.dumps(record) + "\n")
    print(json.dumps(record), flush=True)


def ping_run(kind, mode, hz, secs, delay_ms=None, loss=None, seed=1):
    port = 7100
    server = Proc([binary(kind), "server", kind, f"127.0.0.1:{port}", "echo"], SERVER_CPUS)
    relay = None
    target = f"127.0.0.1:{port}"
    try:
        time.sleep(1.0)
        if delay_ms is not None:
            relay = Proc(
                [binary(kind), "relay", "127.0.0.1:7200", target, str(delay_ms), str(loss), str(seed)],
                RELAY_CPUS,
            )
            target = "127.0.0.1:7200"
            time.sleep(0.5)
        client = Proc([binary(kind), "ping", kind, target, mode, str(hz), str(secs)], {2})
        client.wait(timeout=secs + 60)
        result = [fields(l) for _, l in client.lines if l.startswith("ping ")]
        if not result:
            print("\n".join(l for _, l in client.lines[-20:]), file=sys.stderr)
            return None
        return result[0]
    finally:
        if relay:
            relay.stop()
        server.stop()


def latency(kinds):
    for kind in kinds:
        for mode in ["unreliable", "reliable"]:
            if kind == "udp" and mode == "reliable":
                continue
            r = ping_run(kind, mode, 100, 20)
            if r:
                save("latency", {"transport": kind, "mode": mode, "hz": 100, "secs": 20, **r})


def loss(kinds):
    for kind in kinds:
        for loss_pct in [0, 1, 5, 10]:
            for mode in ["unreliable", "reliable"]:
                if kind == "udp" and mode == "reliable":
                    continue
                r = ping_run(kind, mode, 50, 30, delay_ms=20, loss=loss_pct, seed=loss_pct + 1)
                if r:
                    save(
                        "loss",
                        {"transport": kind, "mode": mode, "one_way_delay_ms": 20, "loss_pct": loss_pct,
                         "hz": 50, "secs": 30, **r},
                    )


# Snapshot size and count per tick for the load suite, as environment variables for server and clients.
SNAPSHOT_ENV = {}


def load(kinds, sizes=(1000, 4096), secs=30):
    for kind in kinds:
        for n in sizes:
            shards = {"renet": -(-n // 1024), "enet": -(-n // 4095)}.get(kind, 1)
            env = {"SHARDS": str(shards), **SNAPSHOT_ENV}
            server_args = [binary(kind), "server", kind, "127.0.0.1:7300", "load"]
            if kind == "quinn":
                server_args.append(str(len(SERVER_CPUS)))
            # renet: one server process per netcode server (1,024 clients each), so each gets its own thread.
            if kind == "renet":
                servers = [
                    Proc([binary(kind), "server", kind, f"127.0.0.1:{7300 + i}", "load"], SERVER_CPUS, {"SHARDS": "1"})
                    for i in range(shards)
                ]
            else:
                servers = [Proc(server_args, SERVER_CPUS, env)]
            # GNS runs every connection of a process on one service thread, so its generator is split in two.
            procs = 2 if kind == "gns" and n > 2048 else 1
            try:
                time.sleep(1.0)
                cpus = sorted(CLIENT_CPUS)
                clients = [
                    Proc(
                        [binary(kind), "load", kind, "127.0.0.1:7300", str(n // procs),
                         str(len(CLIENT_CPUS) // procs), str(secs)],
                        set(cpus[i::procs]),
                        env,
                    )
                    for i in range(procs)
                ]
                for c in clients:
                    c.wait(timeout=secs + 300)
                lines = [x for c in clients for x in c.lines]
                # Measurement starts 5 s after every client connected, or after 120 s if some never did.
                started = sorted(t - (115 if l.startswith("only ") else 0)
                                 for t, l in lines if l.startswith(("connected ", "only ")))
                done = [fields(l) for _, l in lines if l.startswith("load ")]
                if not started or len(done) < procs:
                    print("\n".join(l for _, l in lines[-20:]), file=sys.stderr)
                    continue
                t0 = started[-1] + 5 + 5
                t1 = started[-1] + 5 + secs
                per_server = [
                    [fields(l) for t, l in srv.lines if l.startswith("report ") and t0 <= t <= t1] for srv in servers
                ]
                if not all(per_server):
                    continue
                # Sum over server processes of each one's mean (and of each one's worst sample for the maximum).
                total_of = lambda k, f: sum(f(r[k] for r in rs) for rs in per_server)
                mean = lambda xs: (lambda v: sum(v) / len(v))(list(xs))
                reports = per_server[0]
                total = {k: sum(d[k] for d in done) for k in ("connected", "events_sent", "events_back", "client_cpu_cores")}
                total["client_busiest_thread_cores"] = max(d["client_busiest_thread_cores"] for d in done)
                total["expected_recv_per_s"] = n * 21
                total["snapshot_delivery_pct"] = round(sum(d["snapshot_delivery_pct"] for d in done) / procs, 1)
                total["snapshots_per_client_s"] = round(sum(d["snapshots_per_client_s"] for d in done) / procs, 1)
                save(
                    "load",
                    {
                        "transport": kind,
                        "clients": n,
                        "shards": shards,
                        "generator_processes": procs,
                        "secs": secs,
                        "all_connected": not any(l.startswith("only ") for _, l in lines),
                        "server_processes": len(servers),
                        "server_conns": total_of("conns", min),
                        "server_cpu_cores": round(total_of("cpu_cores", mean), 3),
                        "server_cpu_cores_max": round(total_of("cpu_cores", max), 3),
                        "server_busiest_thread_cores": round(
                            max(mean(r["busiest_thread_cores"] for r in rs) for rs in per_server), 3
                        ),
                        "server_rss_mb": round(total_of("rss_mb", max), 1),
                        "server_recv_per_s": round(total_of("recv_per_s", mean)),
                        "server_sent_per_s": round(total_of("sent_per_s", mean)),
                        "reports": len(reports),
                        **total,
                    },
                )
            finally:
                for srv in servers:
                    srv.stop()
                time.sleep(2)


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in ("latency", "loss", "load"):
        print(__doc__)
        sys.exit(2)
    args = sys.argv[2:]
    sizes = None
    if "--sizes" in args:
        i = args.index("--sizes")
        sizes = tuple(int(x) for x in args[i + 1].split(","))
        args = args[:i] + args[i + 2:]
    if "--snapshot" in args:
        i = args.index("--snapshot")
        size, count = (int(x) for x in args[i + 1].split("x"))
        SNAPSHOT_ENV.update({"SNAPSHOT_BYTES": str(size), "SNAPSHOTS_PER_TICK": str(count)})
        OUTPUT["extra"].update({"snapshot_bytes": size, "snapshots_per_tick": count})
        args = args[:i] + args[i + 2:]
    if "--results" in args:
        i = args.index("--results")
        OUTPUT["path"] = Path(args[i + 1])
        args = args[:i] + args[i + 2:]
    if "--label" in args:
        i = args.index("--label")
        OUTPUT["extra"]["label"] = args[i + 1]
        args = args[:i] + args[i + 2:]
    kinds = args or TRANSPORTS
    if sys.argv[1] == "load" and sizes:
        load(kinds, sizes)
    else:
        {"latency": latency, "loss": loss, "load": load}[sys.argv[1]](kinds)


if __name__ == "__main__":
    main()
