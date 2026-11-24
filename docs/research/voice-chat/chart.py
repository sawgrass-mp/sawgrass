"""Draws docs/assets/voice.svg from results/relay.jsonl, results/rooms.jsonl, results/world.jsonl and
results/codec.jsonl, and prints the computed figures the README quotes (server mixing, bytes on the wire).

Usage: python3 -I chart.py
"""

import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE.parents[2] / "docs" / "assets" / "voice.svg"
QUIC = 38 + 28  # QUIC (measured in NET-5) plus IPv4 and UDP headers, per datagram
FRAME_HEADER = 6  # speaker, sequence and flags (frame); speaker, sequence, flags and length (in a batch)
OPUS = 59  # mean Opus frame at 24 kbit/s, 20 ms (results/codec.jsonl)


def c2(x):
    """Two decimals, halves rounded up as the tables do."""
    return f"{x + 1e-9:.2f}"


def load(name):
    return [json.loads(l) for l in open(HERE / "results" / f"{name}.jsonl")]


def relay(rows, server, players, share, flush=20, label=""):
    for r in rows:
        if (r["server"], int(r["players"]), r["share"], r["label"]) == (server, players, share, label):
            if server != "quinn-batch" or r["flush_ms"] == flush:
                return r
    raise KeyError((server, players, share, flush, label))


def mixing(world, codec, batched_cpu):
    """Cores to mix on the server: decode every frame once, add each into its listeners' mixes, encode one
    frame per listener per 20 ms, then send what batching sends."""
    dec = next(r for r in codec if r["kind"] == "codec" and r["bitrate"] == 24000 and r["frame_ms"] == 20
               and r["complexity"] == 10 and r["label"] == "plain")["decode_ns"]
    mix = next(r for r in codec if r["kind"] == "mix")["ns_per_source_frame"]
    enc = next(r for r in codec if r["kind"] == "mix_encode" and r["complexity"] == 10)["ns_per_frame"]
    return (world["frames_in_per_s"] * dec + world["frames_out_per_s"] * mix
            + world["batch20_datagrams_per_s"] * enc) / 1e9 + batched_cpu


def main():
    rel, rooms, world, codec = load("relay"), load("rooms"), load("world"), load("codec")
    w = {(int(r["players"]), r["share"]): r for r in world}

    city = []
    for share in (0.10, 0.20):
        b = relay(rel, "quinn-batch", 4096, share)
        f = relay(rel, "quinn-frame", 4096, share)
        m = mixing(w[(4096, share)], codec, b["server_cpu_cores"])
        city.append((b, f, m))
    room = {(r["server"], int(r["players"])): r for r in rooms}

    W, x0, bar_w = 880, 330, 370
    parts = []
    y = 44

    def head(t, s):
        parts.append(f'<text x="28" y="{y}" class="h">{t}</text>')
        parts.append(f'<text x="28" y="{y + 24}" class="s">{s}</text>')

    def bars(rows, top):
        nonlocal y
        for label, note, v, text, chosen in rows:
            parts.append(f'<text x="28" y="{y + 8}" class="l">{label}</text>')
            parts.append(f'<text x="28" y="{y + 25}" class="n">{note}</text>')
            parts.append(f'<rect x="{x0}" y="{y}" width="{bar_w}" height="12" rx="6" fill="#13201b"/>')
            fill = "url(#g)" if chosen else "#3f8f7a"
            parts.append(f'<rect x="{x0}" y="{y}" width="{min(v / top, 1) * bar_w:.1f}" height="12" rx="6" fill="{fill}"/>')
            cls = "p" if chosen else "v"
            parts.append(f'<text x="{W - 28}" y="{y + 11}" class="{cls}" text-anchor="end">{text}</text>')
            y += 40

    (b10, f10, m10), (b20, f20, m20) = city
    head("Server CPU, 4,096 players, 10% speaking", "City model; text gives 10% / 20% speaking. Mixing is computed from measured codec costs.")
    y += 62
    # The per-frame run at 4,096 and 20% saturated the load generator too, so its CPU is not established;
    # computed instead from the cost per datagram at 2,048 players and 20%, where neither side was near its cores.
    f2048 = relay(rel, "quinn-frame", 2048, 0.20)
    f20_est = w[(4096, 0.20)]["frames_out_per_s"] * f2048["server_cpu_cores"] / f2048["server_datagrams_out_per_s"]
    f20_text = f"about {f20_est:.1f} (computed)"
    bars([
        ("Our relay, batched per listener", "one datagram per listener every 20 ms", b10["server_cpu_cores"],
         f"{b10['server_cpu_cores']:.2f} / {c2(b20['server_cpu_cores'])} cores", True),
        ("Our relay, one datagram per frame", "lowest latency, most packets; 20% computed", f10["server_cpu_cores"],
         f"{f10['server_cpu_cores']:.2f} / ~{f20_est:.1f}", False),
        ("Mixing on the server", "naive mono mixer, upper bound; bar cut at 4", m10, f"up to {m10:.1f} / {m20:.1f} cores", False),
    ], 4.0)

    y += 22
    head("Same load, two servers", "Rooms of one speaker and ten listeners; text gives 1,100 / 2,200 players.")
    y += 62
    q1, q2 = room[("quinn-frame", 1100)], room[("quinn-frame", 2200)]
    l1, l2 = room[("livekit", 1100)], room[("livekit", 2200)]
    bars([
        ("Our relay over quinn", f"{q1['server_rss_mb']:.0f} / {q2['server_rss_mb']:.0f} MB of memory", q2["server_cpu_cores"],
         f"{q1['server_cpu_cores']:.2f} / {q2['server_cpu_cores']:.2f} cores", True),
        ("LiveKit", f"{l1['server_rss_mb'] / 1024:.1f} / {l2['server_rss_mb'] / 1024:.1f} GB of memory", l2["server_cpu_cores"],
         f"{l1['server_cpu_cores']:.2f} / {l2['server_cpu_cores']:.2f} cores", False),
    ], 2.0)
    H = y + 16

    label = (
        f"Voice server CPU at 4,096 players, 10% and 20% speaking: our relay batched {b10['server_cpu_cores']:.2f} and "
        f"{c2(b20['server_cpu_cores'])} cores; one datagram per frame {f10['server_cpu_cores']:.2f} and {f20_text}; "
        f"mixing on the server up to {m10:.1f} and {m20:.1f} cores (computed). Same rooms load at 1,100 and 2,200 players: "
        f"our relay {q1['server_cpu_cores']:.2f} and {q2['server_cpu_cores']:.2f} cores, LiveKit "
        f"{l1['server_cpu_cores']:.2f} and {l2['server_cpu_cores']:.2f}"
    )
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="{label}">',
        '<defs><linearGradient id="g" x1="0" x2="1" y1="0" y2="0"><stop offset="0" stop-color="#14e1f0"/><stop offset="1" stop-color="#7cf000"/></linearGradient></defs>',
        "<style>text{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif}.h{font-size:22px;font-weight:700;fill:#e8fff4}.s{font-size:13px;fill:#8fb3a6}.l{font-size:15px;font-weight:600;fill:#e8fff4}.n{font-size:12px;fill:#7d9a90}.p{font-size:14px;font-weight:700;fill:#a6ff4d}.v{font-size:14px;font-weight:600;fill:#cfe9df}</style>",
        f'<rect width="{W}" height="{H}" rx="16" fill="#060a08"/>',
        f'<rect x="1" y="1" width="{W - 2}" height="{H - 2}" rx="15" fill="none" stroke="#1d3b2f"/>',
        *parts,
        "</svg>",
    ]
    OUT.write_text("\n".join(svg) + "\n")
    print(f"wrote {OUT}")
    print(label)

    # Bytes on the wire for the README, from the measured frame and datagram rates.
    for share in (0.05, 0.10, 0.20):
        b = relay(rel, "quinn-batch", 4096, share)
        f = relay(rel, "quinn-frame", 4096, share)
        ww = w[(4096, share)]
        frames_out = ww["frames_out_per_s"]
        per_frame = frames_out * (FRAME_HEADER + OPUS + QUIC)
        batched = frames_out * (FRAME_HEADER + OPUS) + ww["batch20_datagrams_per_s"] * (2 + QUIC)
        webrtc = frames_out * (12 + OPUS + 16 + 28)
        mixed = ww["batch20_datagrams_per_s"] * (FRAME_HEADER + OPUS + QUIC)
        upstream = ww["frames_in_per_s"] * (FRAME_HEADER + OPUS + QUIC)
        print(
            f"share={share} frames_in={ww['frames_in_per_s']:.0f} frames_out={frames_out:.0f} "
            f"batch20_datagrams={ww['batch20_datagrams_per_s']:.0f} down_MBps per_frame={per_frame / 1e6:.1f} "
            f"batched={batched / 1e6:.1f} webrtc={webrtc / 1e6:.1f} mixed={mixed / 1e6:.1f} up_MBps={upstream / 1e6:.1f} "
            f"mixing_cores={mixing(ww, codec, b['server_cpu_cores']):.1f} measured_batch_cpu={b['server_cpu_cores']} "
            f"measured_frame_cpu={f['server_cpu_cores']}"
        )


if __name__ == "__main__":
    main()
