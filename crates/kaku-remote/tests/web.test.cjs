// Run with node --test; Playwright is a test-only dependency supplied by CI.
const { test, before, after } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { chromium } = require('playwright');
const root = path.resolve(__dirname, '../../..');
let browser;
before(async () => {
  browser = await chromium.launch(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE
    ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE } : {});
});
after(async () => { await browser?.close(); });

async function page(t, hash = '#t=fixture', options = {}, context = browser) {
  const p = await context.newPage({ viewport: { width: 1000, height: 600 }, ...options });
  t.after(() => p.close());
  const errors = [];
  p.on('pageerror', e => errors.push(e.message));
  await p.addInitScript(() => {
    window.messages = [];
    window.connections = [];
    class Socket extends EventTarget {
      static OPEN = 1; static CLOSED = 3; static CLOSING = 2;
      constructor() { super(); this.readyState = 1; connections.push(this); setTimeout(() => this.onopen?.(), 0); }
      send(data) { if (typeof data === 'string') messages.push(JSON.parse(data)); }
      close(code = 1006) {
        this.readyState = 3;
        const event = new CloseEvent('close', { code });
        this.onclose?.(event); this.dispatchEvent(event);
      }
    }
    window.WebSocket = Socket;
  });
  const html = fs.readFileSync(process.env.KAKU_WEB_SOURCE || path.join(root, 'crates/kaku-remote/assets/web/index.html'), 'utf8')
    .replace('term.registerLinkProvider({', 'term.registerLinkProvider(window.testLinkProvider = {')
    .replace(/\}\)\(\);\s*<\/script>/, 'window.probe = {term, handle, attach, openPast, onHistory, pastScroll, sendFile, sendFiles, snapshots};\n})();\n</script>');
  await p.route('http://localhost:9988/**', route => {
    const name = new URL(route.request().url()).pathname.split('/').pop();
    if (!name) return route.fulfill({ contentType: 'text/html', body: html });
    const file = name === 'font-regular.ttf' || name === 'font-medium.ttf'
      ? path.join(root, 'assets/fonts', name === 'font-regular.ttf' ? 'JetBrainsMono-Regular.ttf' : 'JetBrainsMono-Medium.ttf')
      : path.join(root, 'crates/kaku-remote/assets/web', name);
    return fs.existsSync(file) ? route.fulfill({ path: file }) : route.fulfill({ status: 404, body: '' });
  });
  await p.goto('http://localhost:9988/' + hash);
  await p.waitForTimeout(100);
  return { p, errors };
}
async function screen(p, text = 'Password: ', pane = 1) {
  await p.evaluate(({ text, pane }) => {
    probe.handle({ type: 'tabs', tabs: [{ tab_id: 1, pane_id: 1, panes: [], title: 'one', active: true }, { tab_id: 2, pane_id: 2, panes: [], title: 'two' }] });
    probe.handle({ type: 'frame', seq: 1, pane_id: pane, cols: 80, rows: 24, local_scroll: true,
      data: '\x1b[2J\x1b[1;1H' + text + '\x1b[1;' + (text.length + 1) + 'H\x1b[1 q\x1b[?25h' });
  }, { text, pane });
  await p.waitForFunction(expected =>
    [...document.querySelectorAll('.xterm-rows')].some(rows => rows.textContent.includes(expected)), text);
  await p.evaluate(() => probe.term.focus());
}
const text = p => p.locator('.xterm-rows').innerText();

test('only authoritative output is visible, including secret and offline input', async t => {
  const { p, errors } = await page(t);
  await screen(p);
  assert.match(await text(p), /Password:/);
  await p.keyboard.type('demo-secret');
  await p.waitForTimeout(80);
  assert.equal((await p.evaluate(() => messages.filter(m => m.type === 'text'))).length, 11);
  assert.doesNotMatch(await text(p), /demo-secret/);
  await screen(p, 'confirmed-visible');
  assert.match(await text(p), /confirmed-visible/);
  await p.evaluate(() => { connections.at(-1).readyState = 3; messages.length = 0; });
  await p.keyboard.type('LOST'); await p.waitForTimeout(80);
  assert.doesNotMatch(await text(p), /LOST/);
  assert.equal(await p.evaluate(() => messages.length), 0);
  assert.deepEqual(errors, []);
});

test('malformed link fails visibly without caching a bad key or throwing', async t => {
  for (const hash of ['#k=bad%25key', '#k=YQ']) {
    const { p, errors } = await page(t, hash);
    assert.match(await p.locator('#stage').innerText(), /link.*invalid|invalid.*link/i);
    assert.deepEqual(errors, []);
    assert.equal(await p.evaluate(() => sessionStorage.getItem('kaku.link:/')), null);
  }
});

// Act as the Mac does on a new relay connection: send the challenge, then a
// real sealed message bound to it. Opening the socket alone must not
// remember a key.
async function authenticate(p, keyText) {
  await p.evaluate(async keyText => {
    const raw = Uint8Array.from(atob(keyText.replace(/-/g, '+').replace(/_/g, '/')), c => c.charCodeAt(0));
    const key = await crypto.subtle.importKey('raw', raw, 'AES-GCM', false, ['encrypt']);
    const challenge = crypto.getRandomValues(new Uint8Array(16));
    const hello = new Uint8Array(20); hello.set(new TextEncoder().encode('KCH1')); hello.set(challenge, 4);
    connections.at(-1).onmessage({ data: hello.buffer });
    const label = new TextEncoder().encode('kaku-web h2c');
    const additionalData = new Uint8Array(label.length + 24);
    additionalData.set(label); additionalData.set(challenge, label.length);
    new DataView(additionalData.buffer).setBigUint64(label.length + 16, 0n);
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const data = new Uint8Array(await crypto.subtle.encrypt({ name: 'AES-GCM', iv, additionalData }, key,
      new TextEncoder().encode(JSON.stringify({ type: 'pong' }))));
    const frame = new Uint8Array(12 + data.length); frame.set(iv); frame.set(data, 12);
    connections.at(-1).onmessage({ data: frame.buffer });
  }, keyText);
  await p.waitForTimeout(100);
}

test('verified relay link survives a fresh browser session and rejection forgets it', async t => {
  const key = Buffer.alloc(32, 7).toString('base64url');
  const { p, errors } = await page(t, '#k=' + key);
  assert.equal(await p.evaluate(() => location.hash), '');
  assert.equal(await p.evaluate(() => localStorage.getItem('kaku.link:/')), null);
  await authenticate(p, key);
  assert.equal(await p.evaluate(() => JSON.parse(localStorage.getItem('kaku.link:/')).k), key);
  assert.match(await p.locator('#toast').innerText(), /Bookmark/);
  // Drop all tab storage, as when every tab closes, retaining only persistent storage.
  await p.evaluate(() => sessionStorage.clear());
  await p.reload();
  await p.waitForTimeout(100);
  assert.equal(await p.evaluate(() => connections.length), 1);
  await authenticate(p, key);
  assert.deepEqual(errors, []);
  await p.evaluate(() => connections.at(-1).close(4403));
  assert.equal(await p.evaluate(() => localStorage.getItem('kaku.link:/')), null);
  await p.reload();
  assert.match(await p.locator('#stage').innerText(), /complete link/i);
});

test('remembered relay link opens in a new tab and a restarted browser context', async t => {
  const key = Buffer.alloc(32, 9).toString('base64url');
  const context = await browser.newContext();
  t.after(() => context.close());
  const { p } = await page(t, '#k=' + key, {}, context);
  await authenticate(p, key);
  const next = await page(t, '', {}, context);
  assert.equal(await next.p.evaluate(() => connections.length), 1);
  const state = await context.storageState();
  const restarted = await browser.newContext({ storageState: state });
  t.after(() => restarted.close());
  const restored = await page(t, '', {}, restarted);
  assert.equal(await restored.p.evaluate(() => connections.length), 1);
  await authenticate(restored.p, key);
  assert.deepEqual(restored.errors, []);
});

test('remembered connection survives closing and relaunching the browser process', async t => {
  const profile = fs.mkdtempSync(path.join(require('node:os').tmpdir(), 'kaku-browser-'));
  const options = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE
    ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE } : {};
  let context;
  t.after(async () => { await context?.close(); fs.rmSync(profile, { recursive: true, force: true }); });
  context = await chromium.launchPersistentContext(profile, options);
  const key = Buffer.alloc(32, 14).toString('base64url');
  const first = await page(t, '#k=' + key, {}, context);
  await authenticate(first.p, key);
  await context.close();
  context = await chromium.launchPersistentContext(profile, options);
  const next = await page(t, '', {}, context);
  assert.equal(await next.p.evaluate(() => connections.length), 1);
  await authenticate(next.p, key);
  assert.deepEqual(next.errors, []);
});

test('blocked persistent storage keeps a usable tab connection and explains the limit', async t => {
  const context = await browser.newContext();
  t.after(() => context.close());
  await context.addInitScript(() => {
    Object.defineProperty(window, 'localStorage', { get() { throw new DOMException('blocked', 'SecurityError'); } });
  });
  const key = Buffer.alloc(32, 10).toString('base64url');
  const { p, errors } = await page(t, '#k=' + key, {}, context);
  await authenticate(p, key);
  assert.match(await p.locator('#toast').innerText(), /Not saved/);
  await p.reload();
  await p.waitForTimeout(100);
  assert.equal(await p.evaluate(() => connections.length), 1);
  assert.deepEqual(errors, []);
});

test('blocked browser storage preserves the complete link across reloads', async t => {
  const context = await browser.newContext();
  t.after(() => context.close());
  await context.addInitScript(() => {
    for (const name of ['localStorage', 'sessionStorage']) {
      Object.defineProperty(window, name, { get() { throw new DOMException('blocked', 'SecurityError'); } });
    }
  });
  const key = Buffer.alloc(32, 11).toString('base64url');
  const { p, errors } = await page(t, '#k=' + key, {}, context);
  await authenticate(p, key);
  assert.equal(await p.evaluate(() => location.hash), '#k=' + key);
  await p.reload();
  await p.waitForTimeout(100);
  assert.equal(await p.evaluate(() => connections.length), 1);
  assert.deepEqual(errors, []);
});

test('existing tab credentials become remembered after a verified response', async t => {
  const context = await browser.newContext();
  t.after(() => context.close());
  const key = Buffer.alloc(32, 12).toString('base64url');
  await context.addInitScript(key => sessionStorage.setItem('kaku.link:/', JSON.stringify({ k: key })), key);
  const { p } = await page(t, '', {}, context);
  await authenticate(p, key);
  assert.equal(await p.evaluate(() => JSON.parse(localStorage.getItem('kaku.link:/')).k), key);
});

test('connection help and first-use notice fit narrow and short viewports', async t => {
  const key = Buffer.alloc(32, 13).toString('base64url');
  const { p } = await page(t, '#k=' + key);
  await authenticate(p, key);
  for (const width of [375, 1280]) for (const height of [240, 900]) {
    await p.setViewportSize({ width, height });
    await p.locator('button[title^="Shortcuts"]').click();
    const box = await p.locator('#help .card').boundingBox();
    assert.ok(box.x >= 15 && box.x + box.width <= width - 15);
    assert.ok(box.y >= 15 && box.y + box.height <= height - 15);
    assert.ok(await p.locator('.connection-note').count());
    await p.keyboard.press('Escape');
    assert.equal(await p.locator('#toast').evaluate(el => el.scrollWidth <= el.clientWidth), true);
  }
});

test('local LAN token stays tab-only', async t => {
  const { p } = await page(t);
  assert.equal(await p.evaluate(() => localStorage.getItem('kaku.link:/')), null);
  assert.ok(await p.evaluate(() => sessionStorage.getItem('kaku.link:/')));
});

test('authentication rejection stops reconnecting', async t => {
  const { p } = await page(t);
  await screen(p);
  await p.evaluate(() => connections.at(-1).close(4403));
  await p.waitForTimeout(900);
  assert.match(await p.locator('#notice').innerText(), /link/i);
  assert.equal(await p.evaluate(() => connections.length), 1);
});

test('returning to a cached tab attaches before claiming its new size', async t => {
  const { p } = await page(t);
  await screen(p);
  await p.evaluate(() => probe.attach({ pane_id: 2, tab_id: 2 }));
  await screen(p, 'second', 2);
  await p.evaluate(() => { messages.length = 0; probe.attach({ pane_id: 1, tab_id: 1 }); });
  const commands = await p.evaluate(() => messages.filter(m => ['attach', 'resize'].includes(m.type)));
  assert.equal(commands[0].type, 'attach');
  assert.equal(commands[0].pane_id, 1);
  assert.ok(commands.some(m => m.type === 'resize'));
});

test('renaming from a menu opened before the tab bar was rebuilt still edits the tab', async t => {
  const { p } = await page(t);
  await screen(p);
  await p.locator('#bar .tab').first().click({ button: 'right' });
  // A title update rebuilds every tab button while the menu is open.
  await p.evaluate(() => probe.handle({ type: 'tabs', tabs: [{ tab_id: 1, pane_id: 1, panes: [], title: 'one*', active: true }, { tab_id: 2, pane_id: 2, panes: [], title: 'two' }] }));
  await p.locator('#menu button', { hasText: 'Rename Tab' }).click();
  await p.keyboard.press('Meta+a');
  await p.keyboard.type('renamed');
  await p.keyboard.press('Enter');
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'rename_tab').map(m => [m.tab_id, m.title])), [[1, 'renamed']]);
  // The bar is not stuck: the next update shows.
  await p.evaluate(() => probe.handle({ type: 'tabs', tabs: [{ tab_id: 1, pane_id: 1, panes: [], title: 'renamed', active: true }, { tab_id: 2, pane_id: 2, panes: [], title: 'three' }] }));
  assert.match(await p.locator('#bar').innerText(), /three/);
});

test('presses on controls over the terminal are not clicks for the program', async t => {
  // Narrow enough that the grid runs under the find bar.
  const { p } = await page(t, '#t=fixture', { viewport: { width: 700, height: 600 } });
  await screen(p);
  await p.keyboard.press(process.platform === 'darwin' ? 'Meta+f' : 'Control+Shift+F');
  await p.locator('#find button[data-dir="older"]').click();
  assert.equal(await p.evaluate(() => messages.filter(m => m.type === 'click').length), 0);
});

test('a late history reply from before the view reopened is dropped', async t => {
  const { p } = await page(t);
  await screen(p);
  await p.evaluate(() => {
    probe.openPast();
    const stale = messages.filter(m => m.type === 'history').at(-1).request;
    // The view closes and reopens before that reply arrives.
    probe.term.focus();
    probe.handle({ type: 'frame', seq: 2, pane_id: 1, cols: 81, rows: 24, local_scroll: true, data: '' });
    probe.openPast();
    probe.onHistory({ request: stale, pane_id: 1, first: 0, top: 0, rows: ['\x1b[0mstale-row'] });
  });
  assert.equal(await p.evaluate(() => document.getElementById('past').classList.contains('show')), false);
});

test('oversized UTF-8 paste is rejected visibly and a normal paste is preserved', async t => {
  const { p } = await page(t);
  await screen(p);
  const paste = s => p.evaluate(s => {
    const data = new DataTransfer(); data.setData('text/plain', s);
    document.querySelector('.xterm-helper-textarea').dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true }));
  }, s);
  await paste('中文');
  assert.equal(await p.evaluate(() => messages.filter(m => m.type === 'paste').at(-1).data), '中文');
  await paste('中'.repeat(23000));
  assert.equal(await p.evaluate(() => messages.filter(m => m.type === 'paste').length), 1);
  assert.match(await p.locator('#toast').innerText(), /large|64 KB/i);
});

test('history retains rows while limiting rendered DOM', async t => {
  const { p } = await page(t);
  await screen(p);
  const result = await p.evaluate(() => {
    probe.openPast();
    const times = [];
    for (let n = 0; n < 25; n++) {
      const start = performance.now();
      probe.onHistory({ request: messages.filter(m => m.type === 'history').at(-1)?.request, pane_id: 1, first: 0, top: 10000 - (n + 1) * 400,
        rows: Array.from({ length: 400 }, (_, i) => '\x1b[0mhistory-' + (10000 - (n + 1) * 400 + i)) });
      probe.pastScroll(1e7);
      times.push(performance.now() - start);
    }
    return { nodes: document.querySelectorAll('#past .row').length, text: document.querySelector('#past').textContent,
      maxBatchMs: Math.max(...times) };
  });
  t.diagnostic(JSON.stringify({ nodes: result.nodes, maxBatchMs: result.maxBatchMs }));
  assert.ok(result.nodes > 0);
  assert.ok(result.nodes < 200, 'render visible rows and a small overscan, not every loaded row');
  assert.match(result.text, /history-0\D/);
});

test('history selection survives scrolling and older-page insertion', async t => {
  const { p } = await page(t);
  await screen(p);
  const result = await p.evaluate(() => {
    probe.openPast();
    probe.onHistory({ request: messages.filter(m => m.type === 'history').at(-1)?.request, pane_id: 1, first: 0, top: 400, rows: Array.from({ length: 400 }, (_, i) => '\x1b[0mrow-' + (400 + i)) });
    probe.pastScroll(1e7);
    const node = document.querySelector('#past .row span span').firstChild;
    const range = document.createRange(); range.selectNodeContents(node);
    const selection = getSelection(); selection.removeAllRanges(); selection.addRange(range);
    const before = selection.toString();
    probe.pastScroll(-20);
    const afterScroll = selection.toString();
    probe.onHistory({ request: messages.filter(m => m.type === 'history').at(-1)?.request, pane_id: 1, first: 0, top: 0, rows: Array.from({ length: 400 }, (_, i) => '\x1b[0mrow-' + i) });
    return { before, afterScroll, afterPrepend: selection.toString() };
  });
  assert.ok(result.before.length > 0);
  assert.equal(result.afterScroll, result.before);
  assert.equal(result.afterPrepend, result.before);
});

test('a multi-file drop stays on its original pane and retains the upload window', async t => {
  const { p } = await page(t);
  await screen(p);
  const result = await p.evaluate(async () => {
    const socket = connections.at(-1);
    const original = socket.send.bind(socket);
    const totals = new Map();
    let changed = false, outstanding = 0, peak = 0;
    socket.send = data => {
      original(data);
      const m = JSON.parse(data);
      if (m.type === 'upload_start') {
        totals.set(m.id, 0);
        if (!changed) { changed = true; probe.attach({ tab_id: 2, pane_id: 2 }); }
      }
      if (m.type === 'upload_chunk') {
        const bytes = atob(m.data).length;
        totals.set(m.id, totals.get(m.id) + bytes);
        outstanding += bytes; peak = Math.max(peak, outstanding);
        const received = totals.get(m.id);
        setTimeout(() => { outstanding -= bytes; probe.handle({ type: 'upload_ack', id: m.id, received }); }, 50);
      }
      if (m.type === 'upload_end') setTimeout(() => probe.handle({ type: 'uploaded', id: m.id, ok: true, detail: 'fixture' }), 10);
    };
    await probe.sendFiles([new File([new Uint8Array(1024 * 1024)], 'one.bin'), new File(['two'], 'two.txt')], false);
    return { starts: messages.filter(m => m.type === 'upload_start'), peak,
      chunks: messages.filter(m => m.type === 'upload_chunk').map(m => atob(m.data).length) };
  });
  assert.equal(result.starts.length, 2);
  assert.deepEqual(result.starts.map(m => m.pane_id), [1, 1]);
  assert.equal(new Set(result.starts.map(m => m.transfer)).size, 2);
  assert.ok(result.peak > 65536, 'multiple chunks in flight avoid one RTT per small chunk');
  assert.ok(result.peak <= 512 * 1024);
  assert.ok(result.chunks.every(n => n <= 65536));
});

test('frame acknowledgments include discarded panes and follow xterm parsing', async t => {
  const { p } = await page(t);
  await screen(p);
  assert.ok(await p.evaluate(() => messages.some(m => m.type === 'frame_ack' && m.seq === 1)));
  await p.evaluate(() => {
    probe.handle({ type: 'frame', seq: 2, pane_id: 99, cols: 80, rows: 24, data: 'MUST NOT DRAW' });
  });
  assert.ok(await p.evaluate(() => messages.some(m => m.type === 'frame_ack' && m.seq === 2)));
  assert.doesNotMatch(await text(p), /MUST NOT DRAW/);
});

test('phone, tablet and desktop grids stay within their viewports', async t => {
  for (const [width, height, touch] of [[375, 667, true], [768, 1024, true], [844, 499, true], [844, 501, true], [1280, 720, false]]) {
    const { p, errors } = await page(t, '#t=fixture', { viewport: { width, height }, hasTouch: touch, isMobile: touch });
    await screen(p, 'terminal');
    // Apply the grid requested from the Mac, as a real size acknowledgment.
    await p.evaluate(() => {
      const size = messages.filter(m => m.type === 'resize').at(-1);
      if (!size) throw new Error('no requested grid');
      probe.handle({ type: 'frame', seq: 2, pane_id: 1, ...size, type: 'frame', data: '\x1b[1;1Hterminal' });
    });
    await p.waitForTimeout(120);
    const geometry = await p.evaluate(() => {
      const screen = document.querySelector('.xterm-screen').getBoundingClientRect();
      const stage = document.querySelector('#stage').getBoundingClientRect();
      return { overflow: document.documentElement.scrollWidth > innerWidth, right: screen.right - stage.right, bottom: screen.bottom - stage.bottom };
    });
    assert.equal(geometry.overflow, false, JSON.stringify({ width, height, geometry }));
    assert.ok(geometry.right <= 1 && geometry.bottom <= 1, JSON.stringify({ width, height, geometry }));
    assert.deepEqual(errors, []);
  }
});

async function captureClipboard(p, deny = false) {
  await p.evaluate(deny => {
    window.copied = [];
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: {
      writeText: async text => { copied.push(text); if (deny) throw new DOMException('Denied', 'NotAllowedError'); }
    } });
  }, deny);
}

test('mouse selection copies on release and respects the Mac setting', async t => {
  const { p } = await page(t);
  await screen(p, 'hello world');
  await captureClipboard(p);
  await p.evaluate(() => document.fonts.ready);
  const initial = await p.locator('.xterm-screen').boundingBox();
  await p.mouse.click(initial.x + initial.width - 4, initial.y + initial.height - 4);
  const rect = await p.locator('.xterm-screen').boundingBox();
  const cols = await p.evaluate(() => probe.term.cols);
  const select = async () => {
    await p.mouse.move(rect.x + rect.width / cols * 0.1, rect.y + 8);
    await p.mouse.down();
    // pointerdown re-fits the font. Measure that grid, then end just beyond
    // the fifth boundary rather than rounding into the following space.
    const current = await p.locator('.xterm-screen').boundingBox();
    const cell = current.width / await p.evaluate(() => probe.term.cols);
    await p.mouse.move(current.x + cell * 5.1, current.y + 8, { steps: 4 });
    assert.equal(await p.evaluate(() => copied.length), 0, 'do not copy intermediate drag selections');
    await p.mouse.up();
  };
  await select();
  assert.deepEqual(await p.evaluate(() => copied), ['hello']);
  await p.evaluate(() => { copied.length = 0; probe.handle({ type: 'config', copy_on_select: false }); });
  await select();
  assert.deepEqual(await p.evaluate(() => copied), []);
});

test('history auto-copy and manual copy preserve Unicode, newlines and virtual gaps', async t => {
  const { p } = await page(t);
  await screen(p);
  await captureClipboard(p);
  const result = await p.evaluate(() => {
    probe.openPast();
    probe.onHistory({ request: messages.filter(m => m.type === 'history').at(-1)?.request, pane_id: 1, first: 0, top: 0,
      rows: Array.from({ length: 400 }, (_, i) => '\x1b[0;38;2;10;20;30m中文-' + i) });
    const last = document.querySelector('#past .row:last-child span span').firstChild;
    // Keep an endpoint at the end while materializing the top; the middle
    // remains virtual and must still be included in the copied text.
    const selection = getSelection(), range = document.createRange();
    range.selectNodeContents(last); selection.removeAllRanges(); selection.addRange(range);
    probe.pastScroll(1e7);
    const first = document.querySelector('#past .row span span').firstChild;
    range.setStart(first, 0); range.setEnd(last, last.length);
    const expected = Array.from({ length: 400 }, (_, i) => '中文-' + i).join('\n');
    document.querySelector('#past').dispatchEvent(new PointerEvent('pointerdown', { button: 0, bubbles: true }));
    window.dispatchEvent(new MouseEvent('mouseup', { button: 0 }));
    const data = new DataTransfer();
    document.dispatchEvent(new ClipboardEvent('copy', { clipboardData: data, cancelable: true }));
    return { automatic: copied.at(-1), manual: data.getData('text/plain'), expected };
  });
  assert.equal(result.automatic, result.expected);
  assert.equal(result.manual, result.expected);
});

test('denied auto-copy never queues stale text for a later unrelated click', async t => {
  const { p, errors } = await page(t);
  await screen(p, 'hello world');
  await captureClipboard(p, true);
  await p.evaluate(() => {
    probe.term.select(0, 0, 5);
    document.querySelector('#term').dispatchEvent(new PointerEvent('pointerdown', { button: 0, bubbles: true }));
    window.dispatchEvent(new MouseEvent('mouseup', { button: 0 }));
  });
  await p.waitForTimeout(20);
  await p.evaluate(() => { window.dispatchEvent(new Event('focus')); document.body.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true })); });
  assert.deepEqual(await p.evaluate(() => copied), ['hello']);
  assert.deepEqual(errors, []);
});

test('touch history selection changes copy locally without copying page labels', async t => {
  const { p } = await page(t, '#t=fixture', { hasTouch: true, viewport: { width: 375, height: 667 } });
  await screen(p);
  await captureClipboard(p);
  await p.evaluate(() => {
    probe.openPast();
    probe.onHistory({ request: messages.filter(m => m.type === 'history').at(-1)?.request, pane_id: 1, first: 0, top: 0, rows: ['\x1b[0m中文 history'] });
    const range = document.createRange();
    range.selectNodeContents(document.querySelector('#past .row span span'));
    getSelection().removeAllRanges(); getSelection().addRange(range);
  });
  await p.waitForTimeout(40);
  assert.deepEqual(await p.evaluate(() => copied), ['中文 history']);
  await p.evaluate(() => {
    const range = document.createRange(); range.selectNodeContents(document.querySelector('#bar'));
    getSelection().removeAllRanges(); getSelection().addRange(range);
  });
  await p.waitForTimeout(40);
  assert.deepEqual(await p.evaluate(() => copied), ['中文 history']);
});

test('links exclude sentence punctuation and retain balanced paths and Unicode cell ranges', async t => {
  const { p } = await page(t);
  const issue = 'https://github.com/tw93/Mole/issues/1734#issuecomment-6140809506';
  const cases = [
    [`#1734 (${issue})，并以`, issue],
    ['见 https://example.com/a，后续说明', 'https://example.com/a'],
    ['(https://example.com/Function_(math)).', 'https://example.com/Function_(math)'],
    ['[https://example.com/a?x=1&y=2#section]。', 'https://example.com/a?x=1&y=2#section'],
    ['😀 https://example.com/中文', 'https://example.com/中文'],
    ['https://example.com/a%29?q=%E4%B8%AD', 'https://example.com/a%29?q=%E4%B8%AD'],
  ];
  for (const [source, expected] of cases) {
    const found = await p.evaluate(async source => {
      probe.term.resize(180, 24);
      await new Promise(resolve => probe.term.write('\x1b[?7l\x1b[2J\x1b[H' + source, resolve));
      const links = await new Promise(resolve => testLinkProvider.provideLinks(1, resolve));
      const opened = []; window.open = url => opened.push(url);
      for (const link of links || []) link.activate({ ctrlKey: true }, link.text);
      return { links: (links || []).map(l => ({ text: l.text, range: l.range })), opened };
    }, source);
    assert.deepEqual(found.opened, [expected]);
    if (source.startsWith('😀')) {
      // Bundled xterm Unicode table gives this emoji one cell, but two UTF-16 units.
      assert.equal(found.links[0].range.start.x, 3);
      assert.equal(found.links[0].range.end.x, 26);
    }
  }
});

test('middle click closes the clicked tab through the same confirmation path', async t => {
  const { p } = await page(t); await screen(p);
  await p.evaluate(() => { messages.length = 0; probe.handle({ type: 'tabs', tabs: [
    { tab_id: 1, pane_id: 1, title: 'one', active: true },
    { tab_id: 2, pane_id: 2, title: 'two', confirm_close: true },
  ] }); });
  await p.locator('#bar .tab').nth(1).click({ button: 'middle' });
  assert.equal(await p.locator('#dialog.show').count(), 1);
  assert.equal(await p.evaluate(() => messages.some(m => m.type === 'close_tab')), false);
  await p.locator('#dialog [data-act="cancel"]').click();
  assert.equal(await p.evaluate(() => messages.some(m => m.type === 'close_tab')), false);
  await p.locator('#bar .tab').nth(1).click({ button: 'middle' });
  await p.locator('#dialog [data-act="ok"]').click();
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'close_tab')), [{ type: 'close_tab', tab_id: 2 }]);
  assert.equal(await p.evaluate(() => messages.some(m => m.type === 'attach' && m.pane_id === 2)), false);
});

test('the close confirmation answers to N, Y and clicks outside it, like Kaku', async t => {
  const { p } = await page(t); await screen(p);
  await p.evaluate(() => { messages.length = 0; probe.handle({ type: 'tabs', tabs: [
    { tab_id: 1, pane_id: 1, title: 'one', active: true },
    { tab_id: 2, pane_id: 2, title: 'two', confirm_close: true },
  ] }); });
  const closes = () => p.evaluate(() => messages.filter(m => m.type === 'close_tab').length);
  const ask = () => p.locator('#bar .tab').nth(1).click({ button: 'middle' });
  await ask();
  assert.equal(await p.locator('#dialog .title').innerText(), 'Close this tab?');
  assert.equal(await p.locator('#dialog [data-act="ok"]').innerText(), '[Y] Confirm');
  await p.keyboard.press('n');
  assert.equal(await p.locator('#dialog.show').count(), 0);
  await ask();
  await p.mouse.click(5, 300);
  assert.equal(await p.locator('#dialog.show').count(), 0);
  assert.equal(await closes(), 0);
  await ask();
  await p.keyboard.press('y');
  assert.equal(await closes(), 1);
});

test('terminal menu copies its selection and shares paste limits and pane guards', async t => {
  const { p } = await page(t); await screen(p, 'hello world'); await captureClipboard(p);
  await p.evaluate(() => { probe.term.select(0, 0, 5); navigator.clipboard.readText = async () => 'pasted'; });
  const menu = async () => p.locator('.xterm-screen').click({ button: 'right', position: { x: 20, y: 10 } });
  await menu(); await p.locator('#menu button').getByText('Copy', { exact: true }).click();
  assert.deepEqual(await p.evaluate(() => copied), ['hello']);
  await menu(); await p.locator('#menu button').getByText('Paste', { exact: true }).click();
  assert.equal(await p.evaluate(() => messages.filter(m => m.type === 'paste').at(-1).data), 'pasted');
  await p.evaluate(() => { navigator.clipboard.readText = async () => '中'.repeat(23000); });
  await menu(); await p.locator('#menu button').getByText('Paste', { exact: true }).click();
  assert.equal(await p.evaluate(() => messages.filter(m => m.type === 'paste').length), 1);
  await p.evaluate(() => { navigator.clipboard.readText = () => new Promise(resolve => { window.finishRead = resolve; }); });
  await menu(); await p.locator('#menu button').getByText('Paste', { exact: true }).click();
  await p.evaluate(() => { probe.attach({ tab_id: 2, pane_id: 2 }, true); finishRead('wrong pane'); });
  await p.waitForTimeout(20);
  assert.equal(await p.evaluate(() => messages.filter(m => m.type === 'paste').length), 1);
});

test('pane actions retain the menu target and close confirmation names a pane', async t => {
  const { p } = await page(t); await screen(p);
  await p.evaluate(() => probe.handle({ type: 'tabs', tabs: [
    { tab_id: 1, pane_id: 1, title: 'split', panes: [
      { pane_id: 1, left: 0, top: 0, width: 40, height: 24, confirm_close: true },
      { pane_id: 3, left: 40, top: 0, width: 40, height: 24, confirm_close: false },
    ] }, { tab_id: 2, pane_id: 2, title: 'two' },
  ] }));
  await p.locator('.xterm-screen').click({ button: 'right', position: { x: 20, y: 10 } });
  await p.locator('#menu button').getByText('Split Right', { exact: true }).click();
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'split_pane')), [{ type: 'split_pane', pane_id: 1, direction: 'Right' }]);
  await p.locator('.xterm-screen').click({ button: 'right', position: { x: 20, y: 10 } });
  await p.locator('#menu button').getByText('Close Pane', { exact: true }).click();
  assert.equal(await p.locator('#dialog .title').innerText(), 'Close this pane?');
  await p.evaluate(() => probe.attach({ tab_id: 2, pane_id: 2 }, true));
  await p.locator('#dialog [data-act="ok"]').click();
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'close_pane')), [{ type: 'close_pane', pane_id: 1 }]);
});

test('terminal menu fits narrow and short viewport corners', async t => {
  const { p } = await page(t); await screen(p);
  for (const [width, height] of [[375, 240], [375, 900], [1280, 900], [1280, 240]]) {
    await p.setViewportSize({ width, height });
    await p.evaluate(() => document.querySelector('#term').dispatchEvent(new MouseEvent('contextmenu', {
      clientX: innerWidth - 2, clientY: innerHeight - 2, bubbles: true, cancelable: true,
    })));
    const bounds = await p.locator('#menu').boundingBox();
    assert.ok(bounds.x >= 0 && bounds.y >= 0);
    assert.ok(bounds.x + bounds.width <= width && bounds.y + bounds.height <= height);
    if (process.env.KAKU_TEST_SCREENSHOTS) {
      fs.mkdirSync(process.env.KAKU_TEST_SCREENSHOTS, { recursive: true });
      await p.screenshot({ path: path.join(process.env.KAKU_TEST_SCREENSHOTS, `menu-${width}-${height}.png`) });
    }
    await p.keyboard.press('Escape');
    assert.equal(await p.locator('#menu.show').count(), 0);
  }
});
