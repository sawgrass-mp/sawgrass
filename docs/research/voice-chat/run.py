"""Runs the NET-7 measurements and appends one JSON line per run to results/<suite>.jsonl.

Usage: python3 -I run.py codec | world | relay | rooms [--quick]

- codec: Opus sizes, speed, look-ahead and loss recovery (bench/, `voice-bench codec`).
- world: fan-out of the simulated city at 1,024, 2,048 and 4,096 players and 5, 10 and 20% speaking.
- relay: our relay over quinn with the city, per-frame and batched, the server on CPUs 0-1 and the load
  generator on CPUs 2-3.
- rooms: rooms of one speaker and ten listeners, the same load for our relay and for LiveKit. Needs the
  LiveKit server and livekit-load binaries; their paths come from LIVEKIT_SERVER and LIVEKIT_LOAD.

Every program is started directly with fixed arguments (no shell). Server CPU is read from /proc between the
load generator's "measuring start" and "measuring stop" lines, the same way for every server.
"""

import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
BIN = HERE / "bench" / "target" / "release" / "voice-bench"
RESULTS = HERE / "results"
LIVEKIT_CONFIG = HERE / "livekit-load" / "livekit.yaml"
LIVEKIT_SECRET = "voice-bench-secret-of-at-least-32-characters"
SERVER_CPUS = {0, 1}
CLIENT_CPUS = {2, 3}
CLK_TCK = os.sysconf("SC_CLK_TCK")


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

    def wait_line(self, prefix, timeout):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            for t, line in list(self.lines):
                if line.startswith(prefix):
                    return t, line
            if self.p.poll() is not None:
                break
            time.sleep(0.05)
        return None, None

    def stop(self):
        if self.p.poll() is None:
            self.p.terminate()
            try:
                self.p.wait(timeout=10)
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


def cpu_ticks(pid):
    """User plus system ticks of a process, and of its busiest thread so far, by thread id."""
    def ticks(path):
        with open(path) as f:
            rest = f.read().rsplit(")", 1)[1].split()
        return int(rest[11]) + int(rest[12])

    threads = {}
    for tid in os.listdir(f"/proc/{pid}/task"):
        try:
            threads[tid] = ticks(f"/proc/{pid}/task/{tid}/stat")
        except (OSError, IndexError):
            pass
    return ticks(f"/proc/{pid}/stat"), threads


def rss_mb(pid):
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) / 1024
    return 0.0


def append(suite, record):
    RESULTS.mkdir(exist_ok=True)
    with open(RESULTS / f"{suite}.jsonl", "a") as f:
        f.write(json.dumps(record, sort_keys=True) + "\n")
    print(json.dumps(record, sort_keys=True), flush=True)


def measure(server, client, timeout):
    """Samples the server's CPU between the client's measuring lines and returns the client's result."""
    t, _ = client.wait_line("measuring start", timeout)
    if t is None:
        return None
    total0, threads0 = cpu_ticks(server.p.pid)
    w0 = time.monotonic()
    t, _ = client.wait_line("measuring stop", timeout)
    total1, threads1 = cpu_ticks(server.p.pid)
    secs = time.monotonic() - w0
    rss = rss_mb(server.p.pid)
    _, line = client.wait_line("result", 30)
    if line is None:
        return None
    r = fields(line)
    r["server_cpu_cores"] = round((total1 - total0) / CLK_TCK / secs, 3)
    busiest = max((v - threads0.get(k, 0) for k, v in threads1.items()), default=0)
    r["server_busiest_thread_cores"] = round(busiest / CLK_TCK / secs, 3)
    r["server_rss_mb"] = round(rss, 1)
    reports = [fields(line) for ts, line in server.lines if line.startswith("report") and w0 + 5 <= ts <= w0 + secs + 1]
    for key in ("datagrams_out_per_s", "frames_out_per_s", "payload_out_bytes_per_s", "route_ns_per_frame"):
        values = [x[key] for x in reports if key in x]
        if values:
            r["server_" + key] = round(sum(values) / len(values), 1)
    return r


def run_relay(players, share, batch, flush_ms, payload, frame_ms, secs, env=None, label=""):
    server = Proc(
        [str(BIN), "serve", "127.0.0.1:4433", str(players), "1", "1" if batch else "0", str(flush_ms), "2"],
        SERVER_CPUS,
        env,
    )
    server.wait_line("ready", 30)
    client = Proc(
        [str(BIN), "load", "127.0.0.1:4433", str(players), "1", str(share), str(payload), str(frame_ms), "2", str(secs)],
        CLIENT_CPUS,
        env,
    )
    try:
        r = measure(server, client, 300)
    finally:
        client.stop()
        server.stop()
    time.sleep(2)
    if r is None:
        print("run failed:", client.lines[-5:], server.lines[-5:], file=sys.stderr)
        return None
    r["server"] = "quinn-batch" if batch else "quinn-frame"
    r["flush_ms"] = flush_ms if batch else 0
    r["label"] = label
    return r


def run_livekit(rooms, size, payload, secs):
    server_bin = os.environ.get("LIVEKIT_SERVER")
    load_bin = os.environ.get("LIVEKIT_LOAD")
    if not server_bin or not load_bin:
        sys.exit("set LIVEKIT_SERVER and LIVEKIT_LOAD to the livekit-server and livekit-load binaries")
    server = Proc([server_bin, "--config", str(LIVEKIT_CONFIG)], SERVER_CPUS, {"GOMAXPROCS": "2"})
    time.sleep(3)
    client = Proc(
        [load_bin, "-secret", LIVEKIT_SECRET, "-rooms", str(rooms), "-size", str(size), "-payload", str(payload),
         "-secs", str(secs), "-join-rate", "100"],
        CLIENT_CPUS,
        {"GOMAXPROCS": "2"},
    )
    try:
        r = measure(server, client, 600)
    finally:
        client.stop()
        server.stop()
    time.sleep(3)
    if r is None:
        print("run failed:", client.lines[-5:], file=sys.stderr)
        return None
    r["server"] = "livekit"
    return r


def main():
    suite = sys.argv[1] if len(sys.argv) > 1 else ""
    quick = "--quick" in sys.argv
    secs = 10 if quick else 30
    if suite == "codec":
        p = Proc([str(BIN), "codec"], {0})
        p.p.wait()
        p.t.join()
        for _, line in p.lines:
            word = line.split()[0] if line.split() else ""
            if word in ("codec", "loss", "mix", "lookahead"):
                r = fields(line)
                r["kind"] = word if word != "mix" else ("mix_encode" if "complexity" in r else "mix")
                append("codec", r)
    elif suite == "world":
        for players in (1024, 2048, 4096):
            for share in (0.05, 0.10, 0.20):
                p = Proc([str(BIN), "world", str(players), str(share), "1", "64"], {0})
                p.p.wait()
                p.t.join()
                for _, line in p.lines:
                    if line.startswith("world"):
                        append("world", fields(line))
    elif suite == "relay":
        # Payload: the measured mean Opus frame at 24 kbit/s (59 bytes) plus the 8-byte send time the
        # harness writes into it would be 67; the send time is written over the first 8 of 59 instead.
        for players in (1024, 2048, 4096):
            for share in (0.05, 0.10, 0.20):
                for batch in (False, True):
                    r = run_relay(players, share, batch, 20, 59, 20, secs)
                    if r:
                        append("relay", r)
        for flush_ms in (10, 40):
            r = run_relay(4096, 0.10, True, flush_ms, 59, 20, secs)
            if r:
                append("relay", r)
        r = run_relay(4096, 0.10, False, 20, 118, 40, secs, label="40 ms frames")
        if r:
            append("relay", r)
    elif suite == "rooms":
        for rooms in (100, 200, 372):
            env = {"ROOMS": "11"}
            for batch in (False, True):
                r = run_relay(rooms * 11, 0.0, batch, 20, 59, 20, secs, env)
                if r:
                    r["rooms"] = rooms
                    append("rooms", r)
            r = run_livekit(rooms, 11, 59, secs)
            if r:
                append("rooms", r)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
