"""Draws docs/assets/load.svg from the counted runs in this folder's README.md (the tables of series 2 and 3).

Usage: python3 -I chart.py

The runs are copied here by hand from the README's tables, which are the source; a run the README does not count
(a saturated generator) is left out, as there.
"""

from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE.parents[2] / "docs" / "assets" / "load.svg"

BUDGET_MS = 35.0
# (label, note, tick p99 in ms per run, snapshots per second per bot per run, server cores per run)
ROWS = [
    ("3,800 players, 1 gateway thread", "series 2, seeds 3 and 4", [24.4, 23.8], [19.3, 19.9], [1.42, 1.41]),
    ("4,096 players, 1 gateway thread", "series 3, seeds 1 and 2", [30.5, 28.4], [17.6, 18.6], [1.51, 1.48]),
    ("4,096 players, 2 gateway threads", "series 3, seeds 1 and 2", [39.1, 38.0], [20.0, 20.0], [1.86, 1.85]),
]


def span(values, fmt):
    lo, hi = min(values), max(values)
    return fmt(lo) if lo == hi else f"{fmt(lo)} to {fmt(hi)}"


def main():
    W, x0, bar_w = 880, 330, 370
    parts = []
    y = 44

    def headline(t, s):
        parts.append(f'<text x="28" y="{y}" class="h">{t}</text>')
        parts.append(f'<text x="28" y="{y + 24}" class="s">{s}</text>')

    def bars(values, top, texts, ok, mark=None, mark_label=""):
        nonlocal y
        first = y
        for (label, note, *_), v, text, good in zip(ROWS, values, texts, ok):
            parts.append(f'<text x="28" y="{y + 8}" class="l">{label}</text>')
            parts.append(f'<text x="28" y="{y + 25}" class="n">{note}</text>')
            parts.append(f'<rect x="{x0}" y="{y}" width="{bar_w}" height="12" rx="6" fill="#13201b"/>')
            fill = "url(#g)" if good else "#ff9f43"
            parts.append(f'<rect x="{x0}" y="{y}" width="{min(v / top, 1) * bar_w:.1f}" height="12" rx="6" '
                         f'fill="{fill}"/>')
            parts.append(f'<text x="{W - 28}" y="{y + 11}" class="{"p" if good else "w"}" text-anchor="end">'
                         f'{text}</text>')
            y += 40
        if mark is not None:
            x = x0 + mark / top * bar_w
            parts.append(f'<line x1="{x:.1f}" y1="{first - 8}" x2="{x:.1f}" y2="{y - 22}" stroke="#ffb454" '
                         'stroke-dasharray="4 4"/>')
            parts.append(f'<text x="{x + 4:.1f}" y="{first - 12}" class="b">{mark_label}</text>')

    headline("Tick p99 of the real server, dense crowd at 20 Hz",
             "Two counted runs per row on the project's 4-core server machine, two cores for the server. "
             "Lower is better.")
    y += 70
    p99 = [max(r[2]) for r in ROWS]
    bars(p99, 50.0, [span(r[2], lambda v: f"{v:.1f}") + " ms" for r in ROWS],
         [v <= BUDGET_MS for v in p99], mark=BUDGET_MS, mark_label=f"budget {BUDGET_MS:.0f} ms")

    y += 22
    headline("Snapshots each bot received per second",
             "20 is every tick. Fewer means the gateway dropped snapshots to keep up.")
    y += 62
    low = [min(r[3]) for r in ROWS]
    bars(low, 20.0, [span(r[3], lambda v: f"{v:.1f}") + "/s" for r in ROWS], [v >= 19.0 for v in low])

    y += 6
    cores = ", ".join(span(r[4], lambda v: f"{v:.2f}") for r in ROWS)
    parts.append(f'<text x="28" y="{y}" class="s">Server cores used, in the order above: {cores}.</text>')
    H = y + 22

    label = "; ".join(f"{r[0]}: p99 {span(r[2], lambda v: f'{v:.1f}')} ms, "
                      f"{span(r[3], lambda v: f'{v:.1f}')} snapshots per second" for r in ROWS)
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" '
        f'aria-label="The real server under load. Budget {BUDGET_MS:.0f} ms. {label}">',
        '<defs><linearGradient id="g" x1="0" x2="1" y1="0" y2="0"><stop offset="0" stop-color="#14e1f0"/>'
        '<stop offset="1" stop-color="#7cf000"/></linearGradient></defs>',
        "<style>text{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif}"
        ".h{font-size:22px;font-weight:700;fill:#e8fff4}.s{font-size:13px;fill:#8fb3a6}"
        ".l{font-size:15px;font-weight:600;fill:#e8fff4}.n{font-size:12px;fill:#7d9a90}"
        ".p{font-size:14px;font-weight:700;fill:#a6ff4d}.w{font-size:14px;font-weight:700;fill:#ff9f43}"
        ".b{font-size:12px;fill:#ffb454}</style>",
        f'<rect width="{W}" height="{H}" rx="16" fill="#060a08"/>',
        f'<rect x="1" y="1" width="{W - 2}" height="{H - 2}" rx="15" fill="none" stroke="#1d3b2f"/>',
        *parts,
        "</svg>",
    ]
    OUT.write_text("\n".join(svg) + "\n")
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
