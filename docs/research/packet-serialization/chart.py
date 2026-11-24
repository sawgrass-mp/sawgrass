"""Draws docs/assets/serialization.svg from results/sizes.jsonl and results/speed.jsonl.

Usage: python3 -I chart.py
"""

import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE.parents[2] / "docs" / "assets" / "serialization.svg"

ROWS = [
    ("postcard", "postcard", "chosen: smallest, stable wire format"),
    ("current", "Current hand-written", "sawgrass-protocol style, fixed width"),
    ("bitcode", "bitcode", "format changes between major versions"),
    ("rkyv", "rkyv", "fastest; zero-copy, validated"),
    ("flatbuffers", "FlatBuffers", "zero-copy, verified; schema and codegen"),
]


def load(name):
    return [json.loads(l) for l in open(HERE / "results" / f"{name}.jsonl")]


def main():
    size = {(r["message"], r["format"]): r["bytes"] for r in load("sizes")}
    speed = {(r["message"], r["format"]): r for r in load("speed")}

    W, x0, bar_w = 880, 330, 400
    parts = []
    y = 44

    def head(t, s):
        parts.append(f'<text x="28" y="{y}" class="h">{t}</text>')
        parts.append(f'<text x="28" y="{y + 24}" class="s">{s}</text>')

    def bars(values, top, fmt):
        nonlocal y
        for key, label, note in ROWS:
            v = values[key]
            chosen = key == "postcard"
            parts.append(f'<text x="28" y="{y + 8}" class="l">{label}</text>')
            parts.append(f'<text x="28" y="{y + 25}" class="n">{note}</text>')
            parts.append(f'<rect x="{x0}" y="{y}" width="{bar_w}" height="12" rx="6" fill="#13201b"/>')
            fill = "url(#g)" if chosen else "#3f8f7a"
            parts.append(f'<rect x="{x0}" y="{y}" width="{v / top * bar_w:.1f}" height="12" rx="6" fill="{fill}"/>')
            cls = "p" if chosen else "v"
            parts.append(f'<text x="{W - 28}" y="{y + 11}" class="{cls}" text-anchor="end">{fmt(v)}</text>')
            y += 40

    head("Snapshot size", "One snapshot with 100 visible entities, same quantized values in every format. Smaller is better.")
    y += 62
    sizes = {k: size[("snapshot", k)] for k, _, _ in ROWS}
    bars(sizes, 3000, lambda v: f"{v:,} bytes")

    y += 22
    head("Snapshot encode + decode time", "Median of three runs, one core, decoding untrusted bytes with every check on. Smaller is better.")
    y += 62
    times = {k: (speed[("snapshot", k)]["encode_ns"] + speed[("snapshot", k)]["decode_ns"]) / 1000 for k, _, _ in ROWS}
    bars(times, 8.5, lambda v: f"{v:.1f} µs")
    H = y + 16

    label = "; ".join(f"{l}: {sizes[k]} bytes, {times[k]:.1f} microseconds" for k, l, _ in ROWS)
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="Snapshot of 100 entities. {label}">',
        '<defs><linearGradient id="g" x1="0" x2="1" y1="0" y2="0"><stop offset="0" stop-color="#14e1f0"/><stop offset="1" stop-color="#7cf000"/></linearGradient></defs>',
        "<style>text{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif}.h{font-size:22px;font-weight:700;fill:#e8fff4}.s{font-size:13px;fill:#8fb3a6}.l{font-size:15px;font-weight:600;fill:#e8fff4}.n{font-size:12px;fill:#7d9a90}.p{font-size:14px;font-weight:700;fill:#a6ff4d}.v{font-size:14px;font-weight:600;fill:#cfe9df}</style>",
        f'<rect width="{W}" height="{H}" rx="16" fill="#060a08"/>',
        f'<rect x="1" y="1" width="{W - 2}" height="{H - 2}" rx="15" fill="none" stroke="#1d3b2f"/>',
        *parts,
        "</svg>",
    ]
    OUT.write_text("\n".join(svg) + "\n")
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
