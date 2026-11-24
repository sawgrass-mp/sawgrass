"""Draws docs/assets/visibility.svg from results/single-*.jsonl and results/repeat.jsonl (median of the runs).

Usage: python3 -I chart.py
"""

import json
import statistics
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE.parents[2] / "docs" / "assets" / "visibility.svg"

# (index, sets, label, note)
ROWS = [
    ("current", "sorted", "Server today", "hashed grid, sorted visible sets"),
    ("grid-current", "bits", "Today's grid + bitset sets", "only the bookkeeping changed"),
    ("grid-dense-50", "bits", "Dense grid 50 m + bitset sets", "chosen"),
    ("kiddo-immutable", "bits", "kiddo k-d tree + bitset sets", "rebuilt every tick"),
    ("quadtree", "bits", "Quadtree + bitset sets", "rebuilt every tick"),
    ("bvh", "bits", "BVH + bitset sets", "rebuilt every tick"),
    ("rstar-bulk", "bits", "rstar R*-tree + bitset sets", "bulk-loaded every tick"),
]
PANELS = [
    ("event-2048", "Worst crowd: 2,048 of 4,096 players in a 60 m circle",
     "Tick p99 of the index update and every player's visible set, one thread; the whole tick's budget is 35 ms."),
    ("city", "Everyone spread over 3 km",
     "Same measurement with the 4,096 players spread evenly; about 59 visible each."),
]
TOP = 100.0  # ms at the full bar
BUDGET = 35.0


def main():
    runs = {}
    for name in ("single-radius", "single-nearest", "repeat"):
        for line in open(HERE / "results" / f"{name}.jsonl"):
            r = json.loads(line)
            if "tick_p99_us" in r and r["threads"] == 1:
                runs.setdefault((r["index"], r["sets"], r["scenario"], r["mode"]), []).append(r)
    by = {
        k: {f: statistics.median(r[f] for r in v) for f in ("tick_p99_us", "tick_mean_us")}
        for k, v in runs.items()
    }

    W, x0, bar_w = 880, 330, 380
    parts = []
    y = 44
    alt = []
    for scenario, title, sub in PANELS:
        parts.append(f'<text x="28" y="{y}" class="h">{title}</text>')
        parts.append(f'<text x="28" y="{y + 24}" class="s">{sub}</text>')
        y += 62
        top_y = y - 6
        for index, sets, label, note in ROWS:
            r = by[(index, sets, scenario, "radius")]
            p99, mean = r["tick_p99_us"] / 1000, r["tick_mean_us"] / 1000
            chosen = index == "grid-dense-50"
            parts.append(f'<text x="28" y="{y + 8}" class="l">{label}</text>')
            parts.append(f'<text x="28" y="{y + 25}" class="n">{note}</text>')
            parts.append(f'<rect x="{x0}" y="{y}" width="{bar_w}" height="12" rx="6" fill="#13201b"/>')
            fill = "url(#g)" if chosen else ("#8f4a3f" if p99 > BUDGET else "#3f8f7a")
            parts.append(f'<rect x="{x0}" y="{y}" width="{min(p99, TOP) / TOP * bar_w:.1f}" height="12" rx="6" fill="{fill}"/>')
            cls = "p" if chosen else "v"
            parts.append(f'<text x="{W - 28}" y="{y + 11}" class="{cls}" text-anchor="end">{p99:.1f} ms</text>')
            alt.append(f"{label}, {scenario}: p99 {p99:.1f} ms")
            y += 40
        bx = x0 + BUDGET / TOP * bar_w
        parts.append(f'<line x1="{bx:.1f}" x2="{bx:.1f}" y1="{top_y}" y2="{y - 20}" stroke="#e8fff4" stroke-dasharray="3 4" stroke-opacity="0.6"/>')
        parts.append(f'<text x="{bx + 4:.1f}" y="{top_y - 4}" class="n">35 ms</text>')
        y += 30
    H = y - 14

    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="Visible-set tick at 4096 players. {"; ".join(alt)}">',
        '<defs><linearGradient id="g" x1="0" x2="1" y1="0" y2="0"><stop offset="0" stop-color="#14e1f0"/><stop offset="1" stop-color="#7cf000"/></linearGradient></defs>',
        "<style>text{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif}.h{font-size:22px;font-weight:700;fill:#e8fff4}.s{font-size:13px;fill:#8fb3a6}.l{font-size:15px;font-weight:600;fill:#e8fff4}.n{font-size:12px;fill:#7d9a90}.p{font-size:14px;font-weight:700;fill:#a6ff4d}.v{font-size:14px;font-weight:600;fill:#cfe9df}</style>",
        f'<rect width="{W}" height="{H}" rx="16" fill="#060a08"/>',
        f'<rect x="1" y="1" width="{W - 2}" height="{H - 2}" rx="15" fill="none" stroke="#1d3b2f"/>',
        *parts,
        "</svg>",
    ]
    OUT.write_text("\n".join(svg) + "\n")
    print(f"wrote {OUT}")
    for a in alt:
        print(a)


if __name__ == "__main__":
    main()
