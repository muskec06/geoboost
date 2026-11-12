"""Render docs/images/benchmark-{light,dark}.svg from the latency benchmark results."""
import pathlib

ROWS = [  # (scenario, direct_s, satmux_s, satmux_mode)
    ("HTTPS small page", 1.76, 1.18, "tunnel"),
    ("6 parallel HTTPS requests", 1.78, 1.21, "tunnel"),
    ("HTTP small page", 1.17, 0.59, "tunnel / 1-RTT"),
    ("HTTP 3 MB text", 6.45, 1.21, "tunnel / 1-RTT, zlib"),
    ("HTTPS 3 MB text", 7.04, 6.46, "tunnel"),
    ("HTTPS 2 MB incompressible", 5.12, 4.55, "tunnel"),
]
THEMES = {
    "light": dict(bg="#ffffff", fg="#1f1e1c", mut="#6b6a64", grid="#e8e6df", direct="#b4b2a9", sat="#1d9e75", accent="#0f6e56"),
    "dark": dict(bg="#0d1117", fg="#e6edf3", mut="#9198a1", grid="#262c36", direct="#5f5e5a", sat="#2fbf8f", accent="#5dcaa5"),
}
W, LBL, X0, X1 = 860, 230, 250, 760
ROW_H, TOP = 62, 92
MAXV = 8.0


def render(t):
    H = TOP + ROW_H * len(ROWS) + 70
    sx = lambda v: X0 + (X1 - X0) * v / MAXV
    o = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" font-family="-apple-system,Segoe UI,Helvetica,Arial,sans-serif">',
         f'<rect width="{W}" height="{H}" rx="12" fill="{t["bg"]}"/>',
         f'<text x="28" y="40" font-size="19" font-weight="600" fill="{t["fg"]}">Response time on an emulated GEO satellite link</text>',
         f'<text x="28" y="64" font-size="13" fill="{t["mut"]}">583 ms RTT · 5 Mbps per-seat cap · median of 3 runs · shorter is better</text>']
    for v in range(0, int(MAXV) + 1):
        x = sx(v)
        o.append(f'<line x1="{x:.1f}" y1="{TOP-8}" x2="{x:.1f}" y2="{TOP + ROW_H*len(ROWS) - 8}" stroke="{t["grid"]}" stroke-width="1"/>')
        o.append(f'<text x="{x:.1f}" y="{TOP + ROW_H*len(ROWS) + 10}" font-size="11" fill="{t["mut"]}" text-anchor="middle">{v} s</text>')
    for i, (name, d, s, mode) in enumerate(ROWS):
        y = TOP + i * ROW_H
        o.append(f'<text x="28" y="{y+14}" font-size="14" font-weight="600" fill="{t["fg"]}">{name}</text>')
        o.append(f'<text x="28" y="{y+33}" font-size="12" fill="{t["mut"]}">GeoBoost: {mode}</text>')
        o.append(f'<rect x="{X0}" y="{y}" width="{sx(d)-X0:.1f}" height="18" rx="3" fill="{t["direct"]}"/>')
        o.append(f'<text x="{sx(d)+6:.1f}" y="{y+13}" font-size="12" fill="{t["mut"]}">{d:.2f} s</text>')
        o.append(f'<rect x="{X0}" y="{y+22}" width="{sx(s)-X0:.1f}" height="18" rx="3" fill="{t["sat"]}"/>')
        k = d / s
        o.append(f'<text x="{sx(s)+6:.1f}" y="{y+35}" font-size="12" fill="{t["fg"]}"><tspan font-weight="600">{s:.2f} s</tspan>'
                 f'<tspan fill="{t["accent"]}" font-weight="700" dx="8">{k:.1f}× faster</tspan></text>')
    ly = H - 26
    o.append(f'<rect x="28" y="{ly-10}" width="14" height="12" rx="2" fill="{t["direct"]}"/><text x="48" y="{ly}" font-size="12" fill="{t["mut"]}">Direct connection</text>')
    o.append(f'<rect x="190" y="{ly-10}" width="14" height="12" rx="2" fill="{t["sat"]}"/><text x="210" y="{ly}" font-size="12" fill="{t["mut"]}">GeoBoost</text>')
    o.append('</svg>')
    return "\n".join(o)


out = pathlib.Path(__file__).resolve().parent.parent / "docs" / "images"
for name, t in THEMES.items():
    (out / f"benchmark-{name}.svg").write_text(render(t), encoding="utf-8")
print("ok")
