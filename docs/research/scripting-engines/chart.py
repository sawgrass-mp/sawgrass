"""Draws docs/assets/scripting.svg from results/speed.jsonl (median of the passes) and results/isolation.jsonl.

Usage: python3 -I chart.py
"""

import json
import math
import statistics
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE.parents[2] / "docs" / "assets" / "scripting.svg"
PLAYERS = 4096
TICK_MS = 50

# (engine, label, note)
ROWS = [
    ("wasm", "WebAssembly (wasmtime)", "guest in Rust, compiled ahead of time"),
    ("v8", "JavaScript, V8", "JIT; 1.5 MB per isolate"),
    ("luajit", "LuaJIT", "JIT on: an endless loop is never stopped"),
    ("luau-jit", "Luau, native code", "recommended: mlua with luau-jit"),
    ("luau", "Luau, interpreter", "mlua, the roadmap's proposal"),
    ("luajit-interp", "LuaJIT, JIT off", "still hangs inside string.find"),
    ("lua54", "Lua 5.4", "hangs inside string.find"),
    ("quickjs", "JavaScript, QuickJS-ng", "rquickjs"),
    ("rhai", "Rhai", "pure Rust; limits per value, not in total"),
]
RECOMMENDED = "luau-jit"


def load(name):
    return [json.loads(l) for l in open(HERE / "results" / f"{name}.jsonl")]


def main():
    runs = {}
    for r in load("speed"):
        if r["limits"]:
            runs.setdefault((r["engine"], r["workload"]), []).append(r["ns"])
    speed = {k: statistics.median(v) for k, v in runs.items()}
    iso = {(r["engine"], r["case"]): r for r in load("isolation")}

    W, x0, bar_w = 960, 330, 330
    lo, hi = 0.5, 500.0  # log axis, milliseconds
    parts = []
    y = 44
    parts.append(f'<text x="28" y="{y}" class="h">Script time per tick for {PLAYERS:,} players</text>')
    parts.append(f'<text x="28" y="{y + 24}" class="s">One zone check per player (64 zones) on one core, limits on; '
                 f'log scale, the dashed line is the whole {TICK_MS} ms tick.</text>')
    y += 62
    for i, head in enumerate(("loop", "library call", "memory")):
        parts.append(f'<text x="{W - 170 + i * 66}" y="{y}" class="s" text-anchor="middle">{head}</text>')
    parts.append(f'<text x="{W - 104}" y="{y - 18}" class="s" text-anchor="middle">stops or caps</text>')
    y += 22

    def x_of(ms):
        return x0 + (math.log10(max(ms, lo)) - math.log10(lo)) / (math.log10(hi) - math.log10(lo)) * bar_w

    top = y
    labels = []
    for engine, label, note in ROWS:
        ms = speed[(engine, "zone")] * PLAYERS / 1e6
        chosen = engine == RECOMMENDED
        stops = iso[(engine, "spin")]["outcome"] == "error"
        builtin = iso[(engine, "slow_builtin")]["outcome"]
        caps = iso[(engine, "bomb")].get("memory_capped", False)
        parts.append(f'<text x="28" y="{y + 8}" class="l">{label}</text>')
        parts.append(f'<text x="28" y="{y + 25}" class="n">{note}</text>')
        parts.append(f'<rect x="{x0}" y="{y}" width="{bar_w}" height="12" rx="6" fill="#13201b"/>')
        fill = "url(#g)" if chosen else "#3f8f7a"
        parts.append(f'<rect x="{x0}" y="{y}" width="{x_of(ms) - x0:.1f}" height="12" rx="6" fill="{fill}"/>')
        cls = "p" if chosen else "v"
        parts.append(f'<text x="{x0 + bar_w + 12}" y="{y + 11}" class="{cls}">{ms:,.1f} ms</text>')
        marks = [stops, None if builtin == "not applicable" else builtin == "error", caps]
        for i, ok in enumerate(marks):
            cx = W - 170 + i * 66
            mark, c = {True: ("✓", "ok"), False: ("✗", "bad"), None: ("–", "n")}[ok]
            parts.append(f'<text x="{cx}" y="{y + 12}" class="{c}" text-anchor="middle">{mark}</text>')
        lib = {True: "stops a long library call", False: "does not stop a long library call",
               None: "no library call to test"}[marks[1]]
        labels.append(f"{label}: {ms:.1f} ms, {'stops' if stops else 'does not stop'} an endless loop, "
                      f"{lib}, {'caps' if caps else 'does not cap'} memory")
        y += 40
    tx = x_of(TICK_MS)
    parts.append(f'<line x1="{tx:.1f}" x2="{tx:.1f}" y1="{top - 6}" y2="{y - 22}" stroke="#a6ff4d" '
                 f'stroke-dasharray="4 4" stroke-width="1.5"/>')
    parts.append(f'<text x="{tx:.1f}" y="{top - 10}" class="n" text-anchor="middle">{TICK_MS} ms tick</text>')
    H = y + 6

    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" '
        f'aria-label="Scripting engines. {"; ".join(labels)}">',
        '<defs><linearGradient id="g" x1="0" x2="1" y1="0" y2="0"><stop offset="0" stop-color="#14e1f0"/>'
        '<stop offset="1" stop-color="#7cf000"/></linearGradient></defs>',
        "<style>text{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif}"
        ".h{font-size:22px;font-weight:700;fill:#e8fff4}.s{font-size:13px;fill:#8fb3a6}"
        ".l{font-size:15px;font-weight:600;fill:#e8fff4}.n{font-size:12px;fill:#7d9a90}"
        ".p{font-size:14px;font-weight:700;fill:#a6ff4d}.v{font-size:14px;font-weight:600;fill:#cfe9df}"
        ".ok{font-size:16px;font-weight:700;fill:#a6ff4d}.bad{font-size:16px;font-weight:700;fill:#ff6b6b}</style>",
        f'<rect width="{W}" height="{H}" rx="16" fill="#060a08"/>',
        f'<rect x="1" y="1" width="{W - 2}" height="{H - 2}" rx="15" fill="none" stroke="#1d3b2f"/>',
        *parts,
        "</svg>",
    ]
    OUT.write_text("\n".join(svg) + "\n")
    print(f"wrote {OUT}")
    for line in labels:
        print(line)


if __name__ == "__main__":
    main()
