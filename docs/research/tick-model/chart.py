"""Draws docs/assets/tick.svg from results/headline.jsonl and results/sweep.jsonl.

Usage: python3 -I chart.py
"""

import json
import statistics
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE.parents[2] / "docs" / "assets" / "tick.svg"

# (model, label, note)
ROWS = [
    ("current", "Current loop (hecs, one thread)", "SRV-1 as it is"),
    ("soa", "Plain arrays, one thread", "no ECS at all"),
    ("zones", "Zones in parallel (rayon)", "a crowd is one zone"),
    ("soa-rayon", "Players in parallel, arrays", "rayon over players"),
    ("hecs-rayon", "Players in parallel, hecs", "chosen: current ECS + rayon"),
    ("bevy", "bevy_ecs, parallel queries", "61 crates, slower"),
]
RATE = 30
CHOSEN = "hecs-rayon"


def load(name):
    return [json.loads(l) for l in open(HERE / "results" / f"{name}.jsonl")]


def capacity(rows):
    """Largest player count whose p99 stayed within budget, counting up from the smallest."""
    best = 0
    for r in sorted(rows, key=lambda r: r["players"]):
        if r["p99_us"] > r["budget_us"]:
            break
        best = r["players"]
    return best


def main():
    head = defaultdict(list)
    for r in load("headline"):
        head[(r["rate_hz"], r["scenario"], r["model"])].append(r)
    sweep = defaultdict(list)
    for r in load("sweep"):
        sweep[(r["rate_hz"], r["scenario"], r["model"])].append(r)
    p99 = {m: statistics.median(r["p99_us"] for r in head[(RATE, "crowd", m)]) / 1000 for m, _, _ in ROWS}
    budget = head[(RATE, "crowd", "soa")][0]["budget_us"] / 1000
    cap = {m: capacity(sweep[(RATE, "crowd", m)]) for m, _, _ in ROWS}

    W, x0, bar_w = 880, 300, 400
    parts = []
    y = 44

    def headline(t, s):
        parts.append(f'<text x="28" y="{y}" class="h">{t}</text>')
        parts.append(f'<text x="28" y="{y + 24}" class="s">{s}</text>')

    def bars(values, top, texts, mark=None):
        nonlocal y
        first = y
        for (m, label, note), v, text in zip(ROWS, values, texts):
            chosen = m == CHOSEN
            parts.append(f'<text x="28" y="{y + 8}" class="l">{label}</text>')
            parts.append(f'<text x="28" y="{y + 25}" class="n">{note}</text>')
            parts.append(f'<rect x="{x0}" y="{y}" width="{bar_w}" height="12" rx="6" fill="#13201b"/>')
            fill = "url(#g)" if chosen else "#3f8f7a"
            parts.append(f'<rect x="{x0}" y="{y}" width="{min(v / top, 1) * bar_w:.1f}" height="12" rx="6" fill="{fill}"/>')
            cls = "p" if chosen else "v"
            parts.append(f'<text x="{W - 28}" y="{y + 11}" class="{cls}" text-anchor="end">{text}</text>')
            y += 40
        if mark is not None:
            x = x0 + mark / top * bar_w
            parts.append(f'<line x1="{x:.1f}" y1="{first - 8}" x2="{x:.1f}" y2="{y - 22}" stroke="#ffb454" stroke-dasharray="4 4"/>')
            parts.append(f'<text x="{x + 4:.1f}" y="{first - 12}" class="b">budget {mark:.1f} ms</text>')

    headline(f"Tick p99, 4,096 players at {RATE} Hz, a quarter of them at one event",
             "Median of three 10 s runs, paced at the real rate; 4 cores. Lower is better.")
    y += 70
    top = 50.0
    bars([p99[m] for m, _, _ in ROWS], top, [f"{p99[m]:.1f} ms" for m, _, _ in ROWS], mark=budget)

    y += 22
    headline(f"Players within budget at {RATE} Hz, same crowd",
             "Largest count tried (512 to 16,384) whose p99 stayed under 70% of the tick interval.")
    y += 62
    bars([cap[m] for m, _, _ in ROWS], 8192, [f"{cap[m]:,}" for m, _, _ in ROWS])
    H = y + 16

    label = "; ".join(f"{l}: p99 {p99[m]:.1f} ms, {cap[m]} players" for m, l, _ in ROWS)
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="Tick models. Budget {budget:.1f} ms. {label}">',
        '<defs><linearGradient id="g" x1="0" x2="1" y1="0" y2="0"><stop offset="0" stop-color="#14e1f0"/><stop offset="1" stop-color="#7cf000"/></linearGradient></defs>',
        "<style>text{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif}.h{font-size:22px;font-weight:700;fill:#e8fff4}.s{font-size:13px;fill:#8fb3a6}.l{font-size:15px;font-weight:600;fill:#e8fff4}.n{font-size:12px;fill:#7d9a90}.p{font-size:14px;font-weight:700;fill:#a6ff4d}.v{font-size:14px;font-weight:600;fill:#cfe9df}.b{font-size:12px;fill:#ffb454}</style>",
        f'<rect width="{W}" height="{H}" rx="16" fill="#060a08"/>',
        f'<rect x="1" y="1" width="{W - 2}" height="{H - 2}" rx="15" fill="none" stroke="#1d3b2f"/>',
        *parts,
        "</svg>",
    ]
    OUT.write_text("\n".join(svg) + "\n")
    print(f"wrote {OUT}")
    for m, l, _ in ROWS:
        print(f"{l}: p99 {p99[m]:.1f} ms, {cap[m]} players")


if __name__ == "__main__":
    main()
