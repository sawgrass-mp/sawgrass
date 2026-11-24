"""Draws docs/assets/compression.svg from results/sizes.jsonl, results/speed.jsonl and results/quinn.jsonl.

Usage: python3 -I chart.py
"""

import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE.parents[2] / "docs" / "assets" / "compression.svg"

# (encoding, codec, quinn label, label, note)
ROWS = [
    ("full", "none", "full", "Whole snapshot (postcard)", "today's NET-4 choice"),
    ("full", "zstd-1-dict16k", "full+zstd-dict", "Whole + zstd with dictionary", "still two datagrams"),
    ("delta", "none", "delta", "Delta against acknowledged", "chosen: one datagram"),
    ("delta", "zstd-1-dict16k", "delta+zstd-dict", "Delta + zstd with dictionary", "the next 19%, kept for BOT-2"),
]
# Snapshots a second at 20 Hz for the player count the CPU panel shows.
PLAYERS = 2048
WIRE_OVERHEAD = 38 + 28  # QUIC (measured) plus IPv4 and UDP headers, per datagram


def load(name):
    return [json.loads(l) for l in open(HERE / "results" / f"{name}.jsonl")]


def main():
    size = {(r["encoding"], r["codec"]): r for r in load("sizes") if r["suite"] == "size"}
    speed = {(r["encoding"], r["codec"]): r for r in load("speed")}
    quinn = {(r["label"], r["clients"]): r for r in load("quinn")}

    W, x0, bar_w = 880, 300, 400
    parts = []
    y = 44

    def head(t, s):
        parts.append(f'<text x="28" y="{y}" class="h">{t}</text>')
        parts.append(f'<text x="28" y="{y + 24}" class="s">{s}</text>')

    def bars(values, top, texts):
        nonlocal y
        for (enc, codec, _, label, note), v, text in zip(ROWS, values, texts):
            chosen = (enc, codec) == ("delta", "none")
            parts.append(f'<text x="28" y="{y + 8}" class="l">{label}</text>')
            parts.append(f'<text x="28" y="{y + 25}" class="n">{note}</text>')
            parts.append(f'<rect x="{x0}" y="{y}" width="{bar_w}" height="12" rx="6" fill="#13201b"/>')
            fill = "url(#g)" if chosen else "#3f8f7a"
            parts.append(f'<rect x="{x0}" y="{y}" width="{v / top * bar_w:.1f}" height="12" rx="6" fill="{fill}"/>')
            cls = "p" if chosen else "v"
            parts.append(f'<text x="{W - 28}" y="{y + 11}" class="{cls}" text-anchor="end">{text}</text>')
            y += 40

    head("Bytes on the wire per snapshot", "100 visible entities, 20 Hz, 100 ms round trip; payload plus QUIC, UDP and IP headers per datagram.")
    y += 62
    wire = []
    for enc, codec, _, _, _ in ROWS:
        r = size[(enc, codec)]
        d = r["mean_datagrams"]
        wire.append((r["mean_bytes"] + (3 * d if d > 1 else 0) + WIRE_OVERHEAD * d, d))
    bars([w for w, _ in wire], 2200, [f"{w:,.0f} B, {d:.0f} pkt" for w, d in wire])

    y += 22
    head(f"Gateway CPU, {PLAYERS:,} players", "quinn sending them (measured; dictionary rows at zstd 3 sizes, within 1%) plus encoding at zstd 1 (one core).")
    y += 62
    cpu = []
    for enc, codec, label, _, _ in ROWS:
        q = quinn[(label, PLAYERS)]["server_cpu_cores"]
        cpu.append(q + PLAYERS * 20 * speed[(enc, codec)]["encode_ns"] / 1e9)
    bars(cpu, 2.2, [f"{c:.2f} cores" for c in cpu])
    H = y + 16

    label = "; ".join(
        f"{l}: {w:,.0f} bytes in {d:.0f} datagrams, {c:.2f} cores at {PLAYERS} players"
        for (_, _, _, l, _), (w, d), c in zip(ROWS, wire, cpu)
    )
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="Snapshot compression. {label}">',
        '<defs><linearGradient id="g" x1="0" x2="1" y1="0" y2="0"><stop offset="0" stop-color="#14e1f0"/><stop offset="1" stop-color="#7cf000"/></linearGradient></defs>',
        "<style>text{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif}.h{font-size:22px;font-weight:700;fill:#e8fff4}.s{font-size:13px;fill:#8fb3a6}.l{font-size:15px;font-weight:600;fill:#e8fff4}.n{font-size:12px;fill:#7d9a90}.p{font-size:14px;font-weight:700;fill:#a6ff4d}.v{font-size:14px;font-weight:600;fill:#cfe9df}</style>",
        f'<rect width="{W}" height="{H}" rx="16" fill="#060a08"/>',
        f'<rect x="1" y="1" width="{W - 2}" height="{H - 2}" rx="15" fill="none" stroke="#1d3b2f"/>',
        *parts,
        "</svg>",
    ]
    OUT.write_text("\n".join(svg) + "\n")
    print(f"wrote {OUT}")
    for (_, _, _, l, _), (w, d), c in zip(ROWS, wire, cpu):
        print(f"{l}: {w:.0f} B, {d:.0f} datagrams, {c:.2f} cores")


if __name__ == "__main__":
    main()
