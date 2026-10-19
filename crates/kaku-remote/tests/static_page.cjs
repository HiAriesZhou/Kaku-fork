// Opt-in smoke for the page on its own origin, as on t.kaku.fun: the page
// built by scripts/publish_web_page.sh behind a server that applies its
// vercel.json, a loopback relay told that origin, and an isolated built app.
// Never reads an installed Kaku's configuration, link, or terminal content.
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const os = require('node:os');
const net = require('node:net');
const { spawn, execFileSync } = require('node:child_process');
const assert = require('node:assert/strict');
const { chromium } = require('playwright');
const WebSocket = require(path.join(path.dirname(require.resolve('playwright-core')), 'lib/utilsBundle.js')).ws;
const root = path.resolve(__dirname, '../../..');
const sleep = ms => new Promise(r => setTimeout(r, ms));
async function port() {
  const server = net.createServer();
  await new Promise(r => server.listen(0, '127.0.0.1', r));
  const p = server.address().port;
  await new Promise(r => server.close(r));
  return p;
}
async function until(fn, ms = 15000) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) { const value = await fn(); if (value) return value; await sleep(50); }
  throw new Error('static page smoke timed out');
}

// Serve `dir` the way Vercel applies its vercel.json: redirects, rewrites
// (path-to-regexp `:name(regex)` segments), then headers on every response.
function serveLikeVercel(dir, listenPort) {
  const config = JSON.parse(fs.readFileSync(path.join(dir, 'vercel.json')));
  const pattern = source => new RegExp('^' + source
    .replace(/:(\w+)\(([^)]+)\)/g, '(?<$1>$2)')
    .replace(/:(\w+)/g, '(?<$1>[^/]+)') + '$');
  const types = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript', '.css': 'text/css', '.png': 'image/png', '.woff2': 'font/woff2' };
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://x');
    for (const [key, value] of config.headers[0].headers.map(h => [h.key, h.value])) res.setHeader(key, value);
    for (const r of config.redirects) {
      if (pattern(r.source).test(url.pathname)) { res.writeHead(307, { Location: r.destination }); return res.end(); }
    }
    let file = url.pathname;
    for (const r of config.rewrites) {
      const m = pattern(r.source).exec(url.pathname);
      if (m) { file = r.destination.replace(/:(\w+)/g, (_, k) => m.groups[k]); break; }
    }
    const full = path.join(dir, path.normalize(file));
    if (!full.startsWith(dir) || !fs.existsSync(full) || fs.statSync(full).isDirectory()) { res.writeHead(404); return res.end(); }
    res.writeHead(200, { 'Content-Type': types[path.extname(full)] || 'application/octet-stream' });
    res.end(fs.readFileSync(full));
  });
  return new Promise(r => server.listen(listenPort, '127.0.0.1', () => r(server)));
}

(async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'ks-'));
  const children = [], sockets = [];
  let browser, server;
  const start = (file, args, env) => {
    const p = spawn(file, args, { env: { ...process.env, ...env }, stdio: 'ignore' });
    children.push(p); return p;
  };
  try {
    const relay = await port(), local = await port(), pagePort = await port();
    const origin = `http://127.0.0.1:${pagePort}`;
    const dir = path.resolve(execFileSync(path.join(root, 'scripts/publish_web_page.sh'), ['--dry-run'], {
      env: { ...process.env, KAKU_WEB_RELAY: `ws://127.0.0.1:${relay}` },
    }).toString().trim());
    server = await serveLikeVercel(dir, pagePort);
    start(path.join(root, 'crates/kaku-relay/target/debug/kaku-relay'), [], { PORT: String(relay), KAKU_RELAY_PAGE_ORIGIN: origin });
    await until(async () => { try { return (await fetch(`http://127.0.0.1:${relay}/health`)).ok; } catch { return false; } });

    fs.mkdirSync(path.join(home, '.config/kaku'), { recursive: true });
    fs.mkdirSync(path.join(home, 'tmp'));
    const config = path.join(home, '.config/kaku/kaku.lua');
    fs.writeFileSync(config, `return {default_prog={'/bin/zsh','-f'}, default_cwd=${JSON.stringify(home)}, restore_previous_session=false, remote={enabled=true, port=${local}, tunnel_url='ws://127.0.0.1:${relay}'}}`);
    const app = process.env.KAKU_TEST_APP || path.join(root, 'dist/Kaku.app');
    const env = { HOME: home, XDG_CONFIG_HOME: path.join(home, '.config'), TMPDIR: path.join(home, 'tmp/') };
    start(path.join(app, 'Contents/MacOS/kaku-gui'), ['--config-file', config, 'start', '--always-new-process'], env);
    const cli = (...args) => execFileSync(path.join(app, 'Contents/MacOS/kaku'), args, { env: { ...process.env, ...env } }).toString();

    // Wait for the Mac on the relay, through the route the page asks.
    await until(() => fs.existsSync(path.join(home, 'Library/Application Support/kaku/remote-share.json')));
    const link = cli('remote').split('\n').find(line => /#[A-Za-z0-9_-]{22}$/.test(line));
    const id = /\/([0-9a-f]{32})\//.exec(link)[1];
    const status = () => fetch(`http://127.0.0.1:${relay}/${id}/status`);
    await until(async () => (await status()).ok);
    assert.equal((await status()).headers.get('access-control-allow-origin'), origin);

    // This smoke reads DOM rows; --disable-gpu alone still permits WebGL.
    // Headless Helium on the GPU aborts later page loads at random (the
    // bookmark visits below, before any response), so it is off as well.
    // The dedicated browser suite covers WebGL rendering and recovery.
    browser = await chromium.launch({
      args: ['--disable-gpu', '--disable-webgl', '--disable-3d-apis'],
      ...(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE } : {}),
    });
    const context = await browser.newContext();
    const page = await context.newPage();
    const errors = [], blocked = [];
    page.on('pageerror', e => errors.push(e.message));
    page.on('console', m => { if (/Content Security Policy|Refused to/.test(m.text())) blocked.push(m.text()); });

    // The link from Kaku still names the relay; the relay sends it on to the
    // page's origin and the browser keeps the fragment.
    assert.ok(link.startsWith(`http://127.0.0.1:${relay}/${id}/#`), link);
    await page.goto(link);
    await page.waitForSelector('body.drawn');
    assert.equal(new URL(page.url()).origin, origin);
    assert.equal(new URL(page.url()).hash, '');
    await page.locator('.xterm-helper-textarea').focus();
    await page.keyboard.type('echo STATIC-OK\n');
    // waitForFunction evaluates a string in the page, which its CSP refuses;
    // evaluate does not, so poll with it.
    await until(() => page.evaluate(() => [...document.querySelectorAll('.xterm-rows > div')].some(row => row.textContent.trim() === 'STATIC-OK')));

    // A paired browser comes back by its bookmark, with or without the slash.
    for (const address of [`${origin}/${id}/`, `${origin}/${id}`]) {
      const again = await context.newPage();
      again.on('console', m => { if (/Content Security Policy|Refused to/.test(m.text())) blocked.push(m.text()); });
      await again.goto(address);
      await again.waitForSelector('body.drawn');
      await again.close();
    }

    // Another site cannot open a browser socket to this Mac.
    const foreign = new WebSocket(`ws://127.0.0.1:${relay}/${id}/ws`, { headers: { Origin: 'https://evil.example' } });
    sockets.push(foreign);
    // It is closed at once with 4426, which tells an old page to reload.
    const refused = await new Promise(resolve => {
      foreign.once('close', code => resolve(code));
      foreign.once('unexpected-response', (_, res) => resolve('http ' + res.statusCode));
    });
    assert.equal(refused, 4426);

    // The site root is not a page.
    const rootResponse = await fetch(`${origin}/`, { redirect: 'manual' });
    assert.equal(rootResponse.status, 307);

    assert.deepEqual(errors, []);
    assert.deepEqual(blocked, []);
    console.log(JSON.stringify({ redirectToPageOrigin: 'pass', pairedOnPageOrigin: 'pass', bookmarkReturns: 'pass', foreignOriginRefused: 'pass', cspClean: 'pass' }));
  } finally {
    for (const s of sockets) { try { s.terminate(); } catch {} }
    if (browser) await browser.close().catch(() => {});
    if (server) server.close();
    for (const c of children) { try { c.kill('SIGKILL'); } catch {} }
    fs.rmSync(home, { recursive: true, force: true });
  }
})().catch(e => { console.error(e); process.exit(1); });
