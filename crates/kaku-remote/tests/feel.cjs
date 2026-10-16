// Opt-in measurement of how the web terminal feels, through the encrypted
// relay path the published page uses: typing, scrolling, copying, and tab
// operations, each as numbers. Same isolated setup as static_page.cjs (built
// app, loopback relay, the page as published); never touches an installed
// Kaku or a real link. The network is loopback, so these are Kaku's and the
// page's own costs; a real relay adds its round trip to echo and round-trip
// operations, not to local scrolling or copying. FEEL_RTT_MS puts that round
// trip between the page and the relay (half each way), to see which steps
// wait on it once and which wait several times.
//
//   KAKU_TEST_APP=dist/Kaku.app [FEEL_RTT_MS=150] node crates/kaku-remote/tests/feel.cjs
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const os = require('node:os');
const net = require('node:net');
const { spawn, execFileSync } = require('node:child_process');
const { chromium } = require('playwright');
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
  while (Date.now() < deadline) { const value = await fn(); if (value) return value; await sleep(20); }
  throw new Error('feel measurement timed out');
}
const stats = list => {
  const s = [...list].sort((a, b) => a - b);
  const at = p => s[Math.min(s.length - 1, Math.floor(p * s.length))];
  return { n: s.length, p50: +at(0.5).toFixed(1), p95: +at(0.95).toFixed(1), max: +s[s.length - 1].toFixed(1) };
};

// A TCP relay that holds every chunk for half the round trip each way, in
// order, like a long network path between the browser and the relay.
function delayProxy(listenPort, targetPort, rttMs) {
  const half = rttMs / 2;
  const server = net.createServer(client => {
    const upstream = net.connect(targetPort, '127.0.0.1');
    const pipe = (from, to) => {
      from.on('data', chunk => setTimeout(() => { if (!to.destroyed) to.write(chunk); }, half));
      from.on('end', () => setTimeout(() => to.end(), half));
      from.on('error', () => to.destroy());
    };
    pipe(client, upstream);
    pipe(upstream, client);
  });
  return new Promise(r => server.listen(listenPort, '127.0.0.1', () => r(server)));
}

function serveLikeVercel(dir, listenPort) {
  const config = JSON.parse(fs.readFileSync(path.join(dir, 'vercel.json')));
  const pattern = source => new RegExp('^' + source
    .replace(/:(\w+)\(([^)]+)\)/g, '(?<$1>$2)')
    .replace(/:(\w+)/g, '(?<$1>[^/]+)') + '$');
  const types = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript', '.css': 'text/css', '.png': 'image/png', '.ttf': 'font/ttf' };
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://x');
    for (const h of config.headers[0].headers) res.setHeader(h.key, h.value);
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

// In the page: the terminal's visible text, and a log of when it changed.
const WATCH = () => {
  window.__rows = () => [...document.querySelectorAll('.xterm-rows > div')].map(d => d.textContent).join('\n');
  window.__changes = [];
  new MutationObserver(() => window.__changes.push({ t: performance.now(), text: window.__rows() }))
    .observe(document.querySelector('.xterm-rows'), { subtree: true, childList: true, characterData: true });
};

(async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'kf-'));
  const children = [];
  let browser, server;
  const start = (file, args, env) => {
    const p = spawn(file, args, { env: { ...process.env, ...env }, stdio: 'ignore' });
    children.push(p); return p;
  };
  const report = {};
  try {
    const relay = await port(), local = await port(), pagePort = await port();
    const rtt = Number(process.env.FEEL_RTT_MS || 0);
    const pageRelay = rtt ? await port() : relay;
    if (rtt) await delayProxy(pageRelay, relay, rtt);
    report.rtt_ms = rtt;
    const origin = `http://127.0.0.1:${pagePort}`;
    const dir = path.resolve(execFileSync(path.join(root, 'scripts/publish_web_page.sh'), ['--dry-run'], {
      env: { ...process.env, KAKU_WEB_RELAY: `ws://127.0.0.1:${pageRelay}` },
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
    await until(() => fs.existsSync(path.join(home, 'Library/Application Support/kaku/remote-share.json')));
    const link = execFileSync(path.join(app, 'Contents/MacOS/kaku'), ['remote'], { env: { ...process.env, ...env } })
      .toString().split('\n').find(line => line.includes('#p='));
    const id = /\/([0-9a-f]{32})\//.exec(link)[1];
    await until(async () => (await fetch(`http://127.0.0.1:${relay}/${id}/status`)).ok);

    // The DOM renderer, so text and its timing are observable. WebGL draws
    // the same frames; its own cost is measured by web.test.cjs.
    browser = await chromium.launch({
      args: ['--disable-gpu'],
      ...(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE } : {}),
    });
    const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    await context.grantPermissions(['clipboard-read', 'clipboard-write'], { origin });
    const page = await context.newPage();
    await page.goto(link);
    await page.waitForSelector('body.drawn');
    await page.evaluate(WATCH);
    const textarea = page.locator('.xterm-helper-textarea');
    await textarea.focus();
    const shows = text => until(() => page.evaluate(t => window.__rows().replace(/\n/g, '').includes(t), text));

    // ── Typing: each key to its echo ──────────────────────────────────────
    await page.keyboard.type('clear\n');
    await sleep(500);
    const typed = 'kakuwebterminalfeelsliketheappquicklyandsmoothly';
    await page.evaluate(() => {
      window.__keys = [];
      document.querySelector('.xterm-helper-textarea').addEventListener('keydown', e => window.__keys.push(performance.now()), true);
    });
    for (const ch of typed) {
      await page.keyboard.press(ch);
      await sleep(90);
    }
    await shows(typed);
    const echo = await page.evaluate(typed => {
      const flat = s => s.replace(/\n/g, '');
      return window.__keys.map((t, i) => {
        const prefix = typed.slice(0, i + 1);
        const hit = window.__changes.find(c => c.t >= t && flat(c.text).includes(prefix));
        return hit ? hit.t - t : null;
      });
    }, typed);
    report.typing_echo_ms = stats(echo.filter(v => v !== null));
    report.typing_missing = echo.filter(v => v === null).length;
    await page.keyboard.press('Control+U');

    // A burst as fast as keys go: nothing lost or reordered.
    const burst = 'the quick brown fox jumps over the lazy dog 0123456789';
    const t0 = await page.evaluate(() => performance.now());
    await page.keyboard.type(burst);
    await shows(burst);
    const t1 = await page.evaluate(() => performance.now());
    report.burst = { chars: burst.length, until_all_shown_ms: +(t1 - t0).toFixed(1), intact: await page.evaluate(b => window.__rows().includes(b), burst) };
    await page.keyboard.press('Control+U');

    // ── Scrolling a long plain-shell history ──────────────────────────────
    await page.keyboard.type('seq 1 4000\n');
    await shows('4000');
    await sleep(500);
    const box = await page.locator('#stage').boundingBox();
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.evaluate(() => {
      window.__frames = [];
      let last = performance.now();
      const tick = now => { window.__frames.push(now - last); last = now; if (window.__frames.length < 400) requestAnimationFrame(tick); };
      requestAnimationFrame(tick);
    });
    for (let i = 0; i < 120; i++) {
      await page.mouse.wheel(0, i < 80 ? -100 : 100);
      await sleep(16);
    }
    await sleep(300);
    const frames = await page.evaluate(() => window.__frames.slice(5));
    report.scroll_frames_ms = { ...stats(frames), over_one_frame: frames.filter(f => f > 25).length, over_two_frames: frames.filter(f => f > 42).length };
    report.scroll_reached_history = await page.evaluate(() => {
      const past = document.querySelector('#past');
      return !!past && /\b3[0-9]{3}\b/.test(past.textContent + window.__rows());
    });
    await page.keyboard.press('Escape');
    await page.mouse.wheel(0, 100000);
    await sleep(300);

    // ── Copying a selection (copy on select, as on the Mac) ───────────────
    await page.keyboard.type('clear; echo COPY-ME-0123456789\n');
    await shows('COPY-ME-0123456789');
    await sleep(300);
    const rowBox = await page.evaluate(() => {
      const row = [...document.querySelectorAll('.xterm-rows > div')].find(d => d.textContent.trim() === 'COPY-ME-0123456789');
      const r = row.getBoundingClientRect();
      return { x: r.x, y: r.y + r.height / 2, w: row.textContent.trimEnd().length * (r.width / row.textContent.length) };
    });
    await page.evaluate(() => navigator.clipboard.writeText(''));
    await page.mouse.move(rowBox.x + 1, rowBox.y);
    await page.mouse.down();
    await page.mouse.move(rowBox.x + rowBox.w - 1, rowBox.y, { steps: 8 });
    const up = await page.evaluate(() => performance.now());
    await page.mouse.up();
    const copied = await until(async () => {
      const text = await page.evaluate(() => navigator.clipboard.readText());
      return text ? { text, at: await page.evaluate(() => performance.now()) } : null;
    }, 3000).catch(() => ({ text: '', at: NaN }));
    report.copy = { text: copied.text, exact: copied.text === 'COPY-ME-0123456789', mouseup_to_clipboard_ms: +(copied.at - up).toFixed(1) };

    // ── Tab operations: new, switch, close ───────────────────────────────
    // Timed in the page, from the click event to the change on screen, so
    // Playwright's own wait before it clicks is not counted.
    await page.evaluate(() => {
      window.__clicks = [];
      document.addEventListener('pointerdown', () => window.__clicks.push(performance.now()), true);
      window.__tabs = [];
      new MutationObserver(() => window.__tabs.push({ t: performance.now(), n: document.querySelectorAll('#bar .tab').length }))
        .observe(document.querySelector('#bar'), { subtree: true, childList: true });
    });
    const sinceClick = (pick, arg) => page.evaluate(pick, arg);
    const tabCount = () => page.evaluate(() => document.querySelectorAll('#bar .tab').length);
    const before = await tabCount();
    await page.locator('#bar .add').click();
    await until(async () => (await tabCount()) === before + 1);
    await until(() => page.evaluate(() => !window.__rows().includes('COPY-ME') && /%\s*$/m.test(window.__rows())));
    report.new_tab_ms = await sinceClick(n => {
      const click = window.__clicks.at(-1);
      const shown = window.__tabs.find(c => c.t >= click && c.n === n + 1);
      // The new tab's own prompt, not the previous tab's still on screen.
      const prompt = window.__changes.find(c => c.t >= click && !c.text.includes('COPY-ME') && /%\s*$/m.test(c.text));
      return { tab_shown: +(shown.t - click).toFixed(1), prompt_ready: +(prompt.t - click).toFixed(1) };
    }, before);
    await page.keyboard.type('echo TAB-TWO\n');
    await shows('TAB-TWO');

    const switches = [];
    for (let i = 0; i < 8; i++) {
      const target = i % 2 === 0 ? 'COPY-ME-0123456789' : 'TAB-TWO';
      const index = i % 2 === 0 ? 0 : before;
      await page.locator('#bar .tab').nth(index).click();
      await shows(target);
      switches.push(await page.evaluate(target => {
        const click = window.__clicks.at(-1);
        const hit = window.__changes.find(c => c.t >= click && c.text.includes(target));
        return hit.t - click;
      }, target));
      await sleep(150);
    }
    report.switch_tab_ms = stats(switches);

    await page.locator('#bar .tab').nth(before).hover();
    await page.locator('#bar .tab').nth(before).locator('.close').click();
    let confirmed = false;
    if (await page.locator('#dialog.show').isVisible().catch(() => false)) {
      confirmed = true;
      await page.locator('#dialog [data-act="ok"]').click();
    }
    await until(async () => (await tabCount()) === before);
    report.close_tab_ms = await sinceClick(n => {
      const click = window.__clicks.at(-1);
      const gone = window.__tabs.find(c => c.t >= click && c.n === n);
      return +(gone.t - click).toFixed(1);
    }, before);
    report.close_asked_to_confirm = confirmed;

    console.log(JSON.stringify(report, null, 2));
  } finally {
    if (browser) await browser.close().catch(() => {});
    if (server) server.close();
    for (const c of children) { try { c.kill('SIGKILL'); } catch {} }
    fs.rmSync(home, { recursive: true, force: true });
  }
})().catch(e => { console.error(e); process.exit(1); });
