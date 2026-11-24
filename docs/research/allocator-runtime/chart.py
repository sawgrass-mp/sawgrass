"""Draws docs/assets/runtime.svg from results/gateway.jsonl, results/capacity.jsonl and results/tick.jsonl.

Usage: python3 -I chart.py

Each bar is the median over repetitions (gateway, tick) or over the three allocators (capacity, one run each).
"""

import json
import statistics
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE.parents[2] / "docs" / "assets" / "runtime.svg"

# (runtime, label, note)
RUNTIMES = [
    ("tokio-mt", "Tokio, multi-thread", "chosen: the gateway today"),
    ("tokio-tpc", "Tokio, thread per core", "one endpoint per thread"),
    ("smol", "smol", "quinn's other runtime"),
    ("compio", "compio + compio-quic", "io_uring, same quinn-proto"),
]
CHOSEN_RUNTIME = "tokio-mt"
# (allocator, label, note)
ALLOCATORS = [
    ("system", "glibc malloc", "today's default"),
    ("mimalloc", "mimalloc 2.3", "fastest tick, most memory"),
    ("jemalloc", "jemalloc 5.3", "chosen: half the memory under RocksDB"),
]
CHOSEN_ALLOCATOR = "jemalloc"


def load(name):
    return [json.loads(l) for l in open(HERE / "results" / f"{name}.jsonl")]


def median(rows, key):
    return statistics.median(r[key] for r in rows)


def main():
    gateway = [r for r in load("gateway") if not r.get("failed")]
    capacity = [r for r in load("capacity") if not r.get("failed")]
    tick = [r for r in load("tick") if not r.get("failed")]

    W, x0, bar_w = 880, 300, 400
    parts = []
    y = 44

    def head(t, s):
        parts.append(f'<text x="28" y="{y}" class="h">{t}</text>')
        parts.append(f'<text x="28" y="{y + 24}" class="s">{s}</text>')

    def bars(rows, values, top, texts, chosen_key):
        nonlocal y
        for (key, label, note), v, text in zip(rows, values, texts):
            chosen = key == chosen_key
            parts.append(f'<text x="28" y="{y + 8}" class="l">{label}</text>')
            parts.append(f'<text x="28" y="{y + 25}" class="n">{note}</text>')
            parts.append(f'<rect x="{x0}" y="{y}" width="{bar_w}" height="12" rx="6" fill="#13201b"/>')
            fill = "url(#g)" if chosen else "#3f8f7a"
            width = min(v / top, 1.0) * bar_w
            parts.append(f'<rect x="{x0}" y="{y}" width="{width:.1f}" height="12" rx="6" fill="{fill}"/>')
            cls = "p" if chosen else "v"
            parts.append(f'<text x="{W - 28}" y="{y + 11}" class="{cls}" text-anchor="end">{text}</text>')
            y += 40

    summary = []

    head("Reliable event round trip, p99", "2,048 players on 2 server cores, generator on the other 2; system allocator, median of 3 runs.")
    y += 62
    p99, cpu = [], []
    for key, label, _ in RUNTIMES:
        rows = [r for r in gateway if r["runtime"] == key and r["allocator"] == "system" and r["players"] == 2048]
        p99.append(median(rows, "event_p99_ms"))
        cpu.append(median(rows, "server_cpu_cores"))
        summary.append(f"{label}: p99 {p99[-1]:.1f} ms at {cpu[-1]:.2f} cores")
    bars(RUNTIMES, p99, 50, [f"{p:.1f} ms, {c:.2f} cores" for p, c in zip(p99, cpu)], CHOSEN_RUNTIME)

    y += 22
    head("Server CPU on one core, 2,048 players", "Generator on 3 cores; bar is the core's load (median over allocators), text adds event p99.")
    y += 62
    load_, lat = [], []
    for key, label, _ in RUNTIMES:
        rows = [r for r in capacity if r["runtime"] == key and r["players"] == 2048]
        load_.append(median(rows, "server_cpu_cores"))
        lat.append(median(rows, "event_p99_ms"))
        summary.append(f"{label}: {load_[-1]:.2f} of one core, p99 {lat[-1]:.0f} ms")
    bars(RUNTIMES, load_, 1.0, [f"{c:.2f} core, p99 {p:.0f} ms" for c, p in zip(load_, lat)], CHOSEN_RUNTIME)

    y += 22
    head("Tick time, p50, 4,096 players", "Real TickLoop at 20 Hz with movement, churn, interest and snapshots; median of 3 runs.")
    y += 62
    p50, rss = [], []
    for key, label, _ in ALLOCATORS:
        rows = [r for r in tick if r["allocator"] == key]
        p50.append(median(rows, "p50_ms"))
        rss.append(median(rows, "rss_mb"))
        summary.append(f"{label}: tick p50 {p50[-1]:.1f} ms, {rss[-1]:.0f} MB")
    bars(ALLOCATORS, p50, 25, [f"{p:.1f} ms, {m:.0f} MB" for p, m in zip(p50, rss)], CHOSEN_ALLOCATOR)
    H = y + 16

    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="Allocators and runtimes. {"; ".join(summary)}">',
        '<defs><linearGradient id="g" x1="0" x2="1" y1="0" y2="0"><stop offset="0" stop-color="#14e1f0"/><stop offset="1" stop-color="#7cf000"/></linearGradient></defs>',
        "<style>text{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif}.h{font-size:22px;font-weight:700;fill:#e8fff4}.s{font-size:13px;fill:#8fb3a6}.l{font-size:15px;font-weight:600;fill:#e8fff4}.n{font-size:12px;fill:#7d9a90}.p{font-size:14px;font-weight:700;fill:#a6ff4d}.v{font-size:14px;font-weight:600;fill:#cfe9df}</style>",
        f'<rect width="{W}" height="{H}" rx="16" fill="#060a08"/>',
        f'<rect x="1" y="1" width="{W - 2}" height="{H - 2}" rx="15" fill="none" stroke="#1d3b2f"/>',
        *parts,
        "</svg>",
    ]
    OUT.write_text("\n".join(svg) + "\n")
    print(f"wrote {OUT}")
    for line in summary:
        print(line)


if __name__ == "__main__":
    main()
