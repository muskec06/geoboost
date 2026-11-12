"""Render docs/images/architecture-{light,dark}.svg (README 'How it works' diagram)."""
import pathlib

THEMES = {
    "light": dict(bg="#ffffff", fg="#1f1e1c", mut="#6b6a64", box="#f6f5f1", line="#d3d1c7",
                  pc="#eeedfe", pcs="#534ab7", gs="#e1f5ee", gss="#0f6e56", car="#1d9e75", sat="#854f0b", satbg="#faeeda"),
    "dark": dict(bg="#0d1117", fg="#e6edf3", mut="#9198a1", box="#161b22", line="#30363d",
                 pc="#1e1b3a", pcs="#7f77dd", gs="#0c2b23", gss="#5dcaa5", car="#2fbf8f", sat="#fac775", satbg="#2e2210"),
}
W, H = 880, 330


def box(x, y, w, h, title, sub, t, stroke):
    return (f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="10" fill="{t["box"]}" stroke="{stroke}" stroke-width="1.2"/>'
            f'<text x="{x+w/2}" y="{y+h/2-(8 if sub else -5)}" font-size="15" font-weight="600" fill="{t["fg"]}" text-anchor="middle">{title}</text>'
            + (f'<text x="{x+w/2}" y="{y+h/2+14}" font-size="12" fill="{t["mut"]}" text-anchor="middle">{sub}</text>' if sub else ""))


def render(t):
    o = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" font-family="-apple-system,Segoe UI,Helvetica,Arial,sans-serif">',
         '<defs><marker id="a" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">'
         f'<path d="M1 1L9 5L1 9z" fill="{t["mut"]}"/></marker></defs>',
         f'<rect width="{W}" height="{H}" rx="12" fill="{t["bg"]}"/>']
    # containers
    o.append(f'<rect x="20" y="22" width="260" height="286" rx="14" fill="{t["pc"]}" stroke="{t["pcs"]}" stroke-width="1"/>')
    o.append(f'<text x="40" y="48" font-size="13" font-weight="600" fill="{t["pcs"]}">YOUR WINDOWS PC</text>')
    o.append(f'<rect x="600" y="22" width="260" height="286" rx="14" fill="{t["gs"]}" stroke="{t["gss"]}" stroke-width="1"/>')
    o.append(f'<text x="620" y="48" font-size="13" font-weight="600" fill="{t["gss"]}">YOUR GROUND SERVER</text>')
    o.append(f'<text x="620" y="66" font-size="11.5" fill="{t["mut"]}">same metro as the satellite teleport</text>')
    # boxes
    o.append(box(40, 78, 220, 54, "Browser · git · apps", "", t, t["line"]))
    o.append(f'<line x1="150" y1="134" x2="150" y2="186" stroke="{t["mut"]}" stroke-width="1.5" marker-end="url(#a)"/>')
    o.append(f'<text x="160" y="164" font-size="12" fill="{t["mut"]}">system proxy</text>')
    o.append(box(40, 190, 220, 74, "GeoBoost", "0-RTT open · 1-RTT gate · zlib", t, t["pcs"]))
    o.append(box(620, 190, 220, 74, "satmux.py", "BBR · fq · no idle slow-start", t, t["gss"]))
    o.append(f'<line x1="730" y1="188" x2="730" y2="136" stroke="{t["mut"]}" stroke-width="1.5" marker-end="url(#a)"/>')
    o.append(f'<text x="740" y="164" font-size="12" fill="{t["mut"]}">~2 ms fiber</text>')
    o.append(box(620, 78, 220, 54, "Websites · GitHub · APIs", "", t, t["line"]))
    # carrier bundle
    for i in range(12):
        y = 201 + i * 4.6
        o.append(f'<line x1="262" y1="{y:.1f}" x2="618" y2="{y:.1f}" stroke="{t["car"]}" stroke-width="1.6" stroke-linecap="round" opacity="{0.55 + 0.45 * (i % 2)}"/>')
    o.append(f'<text x="440" y="288" font-size="13" font-weight="600" fill="{t["fg"]}" text-anchor="middle">12 warm TCP carriers · port 8444</text>')
    o.append(f'<text x="440" y="305" font-size="11.5" fill="{t["mut"]}" text-anchor="middle">load-balanced · DNS on the ground</text>')
    # satellite + hop
    sx, sy = 440, 104
    o.append(f'<path d="M262 196 Q340 130 {sx-16} {sy+14}" fill="none" stroke="{t["sat"]}" stroke-width="1.3" stroke-dasharray="5 5"/>')
    o.append(f'<path d="M{sx+16} {sy+14} Q540 130 618 196" fill="none" stroke="{t["sat"]}" stroke-width="1.3" stroke-dasharray="5 5"/>')
    o.append(f'<rect x="{sx-36}" y="{sy-6}" width="22" height="12" rx="2" fill="{t["satbg"]}" stroke="{t["sat"]}"/>')
    o.append(f'<rect x="{sx+14}" y="{sy-6}" width="22" height="12" rx="2" fill="{t["satbg"]}" stroke="{t["sat"]}"/>')
    o.append(f'<line x1="{sx-14}" y1="{sy}" x2="{sx+14}" y2="{sy}" stroke="{t["sat"]}" stroke-width="1.5"/>')
    o.append(f'<rect x="{sx-9}" y="{sy-9}" width="18" height="18" rx="3" fill="{t["sat"]}"/>')
    o.append(f'<text x="{sx}" y="44" font-size="13" font-weight="600" fill="{t["sat"]}" text-anchor="middle">GEO satellite · 35,786 km</text>')
    o.append(f'<text x="{sx}" y="62" font-size="12" fill="{t["mut"]}" text-anchor="middle">~600 ms round trip, every time</text>')
    o.append('</svg>')
    return "\n".join(o)


out = pathlib.Path(__file__).resolve().parent.parent / "docs" / "images"
for name, t in THEMES.items():
    (out / f"architecture-{name}.svg").write_text(render(t), encoding="utf-8")
print("ok")
