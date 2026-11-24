"""Draws docs/assets/transport.svg from results/loss.jsonl and results/load.jsonl.

Usage: python3 -I chart.py
"""

import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE.parents[2] / "docs" / "assets" / "transport.svg"

ROWS = [
    ("quinn", "QUIC (quinn)", "chosen: encrypted, uses every core"),
    ("gns", "GameNetworkingSockets", "encrypted; C++ library"),
    ("enet", "ENet (rusty_enet)", "no encryption; 4,095 per host"),
    ("renet", "renet (netcode)", "encrypted; 1,024 per server"),
    ("udp", "Plain UDP", "floor: no reliability, no encryption"),
]


def load(name):
    return [json.loads(l) for l in open(HERE / "results" / f"{name}.jsonl")]


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;")


def main():
    loss = {(r["transport"], r["mode"], r["loss_pct"]): r for r in load("loss")}
    cpu = {(r["transport"], int(r["clients"])): r for r in load("load")}

    W, x0, bar_w = 880, 330, 400
    parts = []
    y = 44
    head = lambda t, s: [
        f'<text x="28" y="{y}" class="h">{esc(t)}</text>',
        f'<text x="28" y="{y + 24}" class="s">{esc(s)}</text>',
    ]
    parts += head("Reliable events under packet loss", "p99 delivery time of an event, 40 ms round trip, 1% / 5% / 10% loss each way. Lower is better.")
    y += 62
    cap = 1000.0
    for key, label, note in ROWS:
        chosen = key == "quinn"
        parts.append(f'<text x="28" y="{y + 12}" class="l">{esc(label)}</text>')
        parts.append(f'<text x="28" y="{y + 29}" class="n">{esc(note)}</text>')
        if key == "udp":
            parts.append(f'<text x="{x0}" y="{y + 20}" class="n">no reliable delivery</text>')
            y += 48
            continue
        for i, pct in enumerate([1, 5, 10]):
            v = loss[(key, "reliable", pct)]["p99_ms"]
            w = min(v, cap) / cap * bar_w
            by = y + i * 12
            fill = "url(#g)" if chosen else "#3f8f7a"
            parts.append(f'<rect x="{x0}" y="{by}" width="{bar_w}" height="8" rx="4" fill="#13201b"/>')
            parts.append(f'<rect x="{x0}" y="{by}" width="{max(w, 4):.1f}" height="8" rx="4" fill="{fill}"/>')
            cls = "p" if chosen else "v"
            more = "+" if v > cap else ""
            parts.append(f'<text x="{W - 28}" y="{by + 8}" class="{cls} t" text-anchor="end">{pct}%: {v:,.0f} ms{more}</text>')
        y += 48

    y += 18
    parts += head("Server CPU with 4,096 players", "Cores used by the server at 20 Hz: 2 x 1,000-byte snapshots down, 64-byte state up, one event a second. Lower is better.")
    y += 62
    top = 2.0
    for key, label, _ in ROWS:
        r = cpu[(key, 4096)]
        chosen = key == "quinn"
        v = r["server_cpu_cores"]
        ok = r["snapshot_delivery_pct"] >= 99 and r["server_conns"] >= 4096
        note = "kept up" if ok else f"fell behind: {r['snapshot_delivery_pct']:.0f}% of snapshots delivered"
        parts.append(f'<text x="28" y="{y + 8}" class="l">{esc(label)}</text>')
        parts.append(f'<text x="28" y="{y + 25}" class="n">{esc(note)}</text>')
        w = v / top * bar_w
        fill = "url(#g)" if chosen else ("#3f8f7a" if ok else "#6b4f2a")
        parts.append(f'<rect x="{x0}" y="{y}" width="{bar_w}" height="12" rx="6" fill="#13201b"/>')
        parts.append(f'<rect x="{x0}" y="{y}" width="{w:.1f}" height="12" rx="6" fill="{fill}"/>')
        cls = "p" if chosen else "v"
        parts.append(f'<text x="{W - 28}" y="{y + 11}" class="{cls}" text-anchor="end">{v:.2f}</text>')
        y += 40
    H = y + 16

    label = "; ".join(
        f"{l}: p99 " + ("n/a" if k == "udp" else "/".join(f"{loss[(k, 'reliable', p)]['p99_ms']:.0f}" for p in [1, 5, 10]) + " ms")
        + f", {cpu[(k, 4096)]['server_cpu_cores']:.2f} cores"
        for k, l, _ in ROWS
    )
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="{esc(label)}">',
        '<defs><linearGradient id="g" x1="0" x2="1" y1="0" y2="0"><stop offset="0" stop-color="#14e1f0"/><stop offset="1" stop-color="#7cf000"/></linearGradient></defs>',
        "<style>text{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif}.h{font-size:22px;font-weight:700;fill:#e8fff4}.s{font-size:13px;fill:#8fb3a6}.l{font-size:15px;font-weight:600;fill:#e8fff4}.n{font-size:12px;fill:#7d9a90}.p{font-size:14px;font-weight:700;fill:#a6ff4d}.v{font-size:14px;font-weight:600;fill:#cfe9df}.t{font-size:11px}</style>",
        f'<rect width="{W}" height="{H}" rx="16" fill="#060a08"/>',
        f'<rect x="1" y="1" width="{W - 2}" height="{H - 2}" rx="15" fill="none" stroke="#1d3b2f"/>',
        *parts,
        "</svg>",
    ]
    OUT.write_text("\n".join(svg) + "\n")
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
