"""Convert a 24x24 single-color SVG icon into Kaku PolyCommand Rust source.

Every path becomes absolute MoveTo/LineTo/CubicTo/Close; arcs are turned into
cubics. Coordinates are stored as BlockCoord::Frac(n, 120), a 0.2 unit grid,
which is under half a pixel at the size the agent marks are drawn.

Usage: python3 scripts/svg_to_poly.py CLAUDE_MARK claude.svg
Used for kaku-gui/src/termwindow/render/agent_marks.rs.
"""
import math, re, sys

def tokens(d):
    for m in re.finditer(r'[A-Za-z]|-?(?:\d+\.?\d*|\.\d+)(?:e-?\d+)?', d):
        yield m.group()

def parse(d):
    toks = list(tokens(d)); i = 0; cmd = None; out = []
    cx = cy = sx = sy = 0.0; last_c2 = None; last_q = None
    def num():
        nonlocal i; v = float(toks[i]); i += 1; return v
    def flag():
        # Arc flags are one digit and may be glued to what follows: "01.2".
        nonlocal i
        t = toks[i]
        if len(t) > 1:
            toks[i] = t[1:]
        else:
            i += 1
        return int(t[0])
    while i < len(toks):
        if re.match(r'[A-Za-z]', toks[i]):
            cmd = toks[i]; i += 1
            if cmd in 'Zz':
                out.append(('Z',)); cx, cy = sx, sy; last_c2 = last_q = None; continue
        rel = cmd.islower(); C = cmd.upper()
        ox, oy = (cx, cy) if rel else (0.0, 0.0)
        if C == 'M':
            x, y = num()+ox, num()+oy; out.append(('M', x, y)); cx, cy = sx, sy = x, y
            cmd = 'l' if rel else 'L'; last_c2 = last_q = None
        elif C == 'L':
            x, y = num()+ox, num()+oy; out.append(('L', x, y)); cx, cy = x, y; last_c2 = last_q = None
        elif C == 'H':
            x = num()+(cx if rel else 0); out.append(('L', x, cy)); cx = x; last_c2 = last_q = None
        elif C == 'V':
            y = num()+(cy if rel else 0); out.append(('L', cx, y)); cy = y; last_c2 = last_q = None
        elif C == 'C':
            x1, y1, x2, y2, x, y = num()+ox, num()+oy, num()+ox, num()+oy, num()+ox, num()+oy
            out.append(('C', x1, y1, x2, y2, x, y)); last_c2 = (x2, y2); cx, cy = x, y; last_q = None
        elif C == 'S':
            x1, y1 = (2*cx-last_c2[0], 2*cy-last_c2[1]) if last_c2 else (cx, cy)
            x2, y2, x, y = num()+ox, num()+oy, num()+ox, num()+oy
            out.append(('C', x1, y1, x2, y2, x, y)); last_c2 = (x2, y2); cx, cy = x, y; last_q = None
        elif C in 'QT':
            if C == 'Q':
                qx, qy = num()+ox, num()+oy
            else:
                qx, qy = (2*cx-last_q[0], 2*cy-last_q[1]) if last_q else (cx, cy)
            x, y = num()+ox, num()+oy
            out.append(('C', cx+2/3*(qx-cx), cy+2/3*(qy-cy), x+2/3*(qx-x), y+2/3*(qy-y), x, y))
            last_q = (qx, qy); cx, cy = x, y; last_c2 = None
        elif C == 'A':
            rx, ry, phi = num(), num(), num(); fa = flag(); fs = flag()
            x, y = num()+ox, num()+oy
            out.extend(arc(cx, cy, rx, ry, phi, fa, fs, x, y)); cx, cy = x, y; last_c2 = last_q = None
        else:
            raise ValueError(C)
    return out

def arc(x1, y1, rx, ry, phi, fa, fs, x2, y2):
    if rx == 0 or ry == 0:
        return [('L', x2, y2)]
    p = math.radians(phi); cp, sp = math.cos(p), math.sin(p)
    dx, dy = (x1-x2)/2, (y1-y2)/2
    x1p, y1p = cp*dx+sp*dy, -sp*dx+cp*dy
    rx, ry = abs(rx), abs(ry)
    lam = x1p**2/rx**2 + y1p**2/ry**2
    if lam > 1:
        rx *= math.sqrt(lam); ry *= math.sqrt(lam)
    num_ = rx*rx*ry*ry - rx*rx*y1p*y1p - ry*ry*x1p*x1p
    den = rx*rx*y1p*y1p + ry*ry*x1p*x1p
    co = math.sqrt(max(0, num_/den)) * (-1 if fa == fs else 1)
    cxp, cyp = co*rx*y1p/ry, -co*ry*x1p/rx
    cx, cy = cp*cxp-sp*cyp+(x1+x2)/2, sp*cxp+cp*cyp+(y1+y2)/2
    def ang(ux, uy, vx, vy):
        a = math.atan2(ux*vy-uy*vx, ux*vx+uy*vy); return a
    t1 = ang(1, 0, (x1p-cxp)/rx, (y1p-cyp)/ry)
    dt = ang((x1p-cxp)/rx, (y1p-cyp)/ry, (-x1p-cxp)/rx, (-y1p-cyp)/ry)
    if not fs and dt > 0: dt -= 2*math.pi
    if fs and dt < 0: dt += 2*math.pi
    n = max(1, math.ceil(abs(dt)/(math.pi/2))); d = dt/n; out = []
    k = 4/3*math.tan(d/4)
    for s in range(n):
        a1 = t1 + s*d; a2 = a1 + d
        e1 = (math.cos(a1), math.sin(a1)); e2 = (math.cos(a2), math.sin(a2))
        pts = [(e1[0]-k*e1[1], e1[1]+k*e1[0]), (e2[0]+k*e2[1], e2[1]-k*e2[0]), e2]
        tr = [(cx+cp*rx*px-sp*ry*py, cy+sp*rx*px+cp*ry*py) for px, py in pts]
        out.append(('C', *tr[0], *tr[1], *tr[2]))
    return out

def q(v):
    n = round(v*5)
    assert -128 <= n <= 127, v
    return f"Frac({n}, 120)"

def rust(name, paths):
    lines = ["#[rustfmt::skip]", f"pub(crate) const {name}: &[PolyCommand] = &["]
    for d in paths:
        for c in parse(d):
            if c[0] == 'M': lines.append(f"    MoveTo({q(c[1])}, {q(c[2])}),")
            elif c[0] == 'L': lines.append(f"    LineTo({q(c[1])}, {q(c[2])}),")
            elif c[0] == 'C':
                lines.append(f"    CubicTo {{ control1: ({q(c[1])}, {q(c[2])}), control2: ({q(c[3])}, {q(c[4])}), to: ({q(c[5])}, {q(c[6])}) }},")
            else: lines.append("    Close,")
    lines.append("];")
    return "\n".join(lines)

if __name__ == '__main__':
    name, path = sys.argv[1], sys.argv[2]
    svg = open(path).read()
    print(rust(name, re.findall(r'\sd="([^"]+)"', svg)))
