#!/usr/bin/env bash
# Publish the web terminal page to its own origin (t.kaku.fun), apart from
# the relay, which then only forwards ciphertext and redirects old links.
#
#   scripts/publish_web_page.sh            build and deploy to production
#   scripts/publish_web_page.sh --dry-run  build only, print the directory
#
# The page goes to its own Vercel project (KAKU_WEB_PROJECT, default
# kaku-web) that holds nothing else, so no other page shares its origin and
# the browser keys stored there. One-time setup: `vercel project add
# kaku-web`, then `vercel domains add t.kaku.fun kaku-web`.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RELAY="${KAKU_WEB_RELAY:-wss://r.kaku.fun}"
PROJECT="${KAKU_WEB_PROJECT:-kaku-web}"
WEB="$ROOT/crates/kaku-remote/assets/web"

OUT="$(mktemp -d "${TMPDIR:-/tmp}/kaku-web.XXXXXX")"
mkdir -p "$OUT/assets"
for name in xterm.js xterm-webgl.js xterm-unicode11.js xterm.css icon.png \
	LICENSE-xterm LICENSE-xterm-webgl LICENSE-xterm-unicode11 \
	font-regular.woff2 font-medium.woff2 font-symbols.woff2; do
	cp "$WEB/$name" "$OUT/assets/$name"
done

python3 -I - "$WEB/index.html" "$OUT" "$RELAY" <<'PY'
import base64, hashlib, json, re, sys

source, out, relay = sys.argv[1], sys.argv[2], sys.argv[3]
html = open(source, encoding="utf-8").read()
empty = '<meta name="kaku-relay" content="">'
if html.count(empty) != 1:
    sys.exit("index.html must hold exactly one empty kaku-relay meta tag")
html = html.replace(empty, '<meta name="kaku-relay" content="%s">' % relay)
inline = re.findall(r"<script>(.*?)</script>", html, re.S)
if len(inline) != 1:
    sys.exit("expected exactly one inline script, found %d" % len(inline))
digest = base64.b64encode(hashlib.sha256(inline[0].encode("utf-8")).digest()).decode()
open(out + "/index.html", "w", encoding="utf-8").write(html)

http = re.sub(r"^ws", "http", relay)
csp = "; ".join([
    "default-src 'none'",
    "script-src 'self' 'sha256-%s'" % digest,
    # xterm sets element styles directly.
    "style-src 'self' 'unsafe-inline'",
    "font-src 'self'",
    "img-src 'self' data: blob:",
    "connect-src 'self' %s %s" % (relay, http),
    "base-uri 'none'",
    "form-action 'none'",
    "frame-ancestors 'none'",
])
hex_id = "([0-9a-f]{32})"
config = {
    "redirects": [{"source": "/", "destination": "https://kaku.fun", "permanent": False}],
    "rewrites": [
        {"source": "/:id%s/assets/:name" % hex_id, "destination": "/assets/:name"},
        {"source": "/:id%s" % hex_id, "destination": "/index.html"},
        {"source": "/:id%s/" % hex_id, "destination": "/index.html"},
    ],
    "headers": [{
        "source": "/(.*)",
        "headers": [
            {"key": "Content-Security-Policy", "value": csp},
            {"key": "X-Content-Type-Options", "value": "nosniff"},
            {"key": "Referrer-Policy", "value": "no-referrer"},
            {"key": "X-Frame-Options", "value": "DENY"},
            {"key": "Cache-Control", "value": "no-cache"},
        ],
    }],
}
json.dump(config, open(out + "/vercel.json", "w"), indent=2)
PY

if [[ "${1:-}" == "--dry-run" ]]; then
	echo "$OUT"
	exit 0
fi

cd "$OUT"
vercel link --yes --project "$PROJECT" >/dev/null
vercel deploy --prod --yes
