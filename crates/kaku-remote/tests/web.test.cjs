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

async function page(t, hash = '#t=fixture', options = {}) {
  const p = await browser.newPage({ viewport: { width: 1000, height: 600 }, ...options });
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
  await p.waitForTimeout(100);
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
      probe.onHistory({ pane_id: 1, first: 0, top: 10000 - (n + 1) * 400,
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
    probe.onHistory({ pane_id: 1, first: 0, top: 400, rows: Array.from({ length: 400 }, (_, i) => '\x1b[0mrow-' + (400 + i)) });
    probe.pastScroll(1e7);
    const node = document.querySelector('#past .row span span').firstChild;
    const range = document.createRange(); range.selectNodeContents(node);
    const selection = getSelection(); selection.removeAllRanges(); selection.addRange(range);
    const before = selection.toString();
    probe.pastScroll(-20);
    const afterScroll = selection.toString();
    probe.onHistory({ pane_id: 1, first: 0, top: 0, rows: Array.from({ length: 400 }, (_, i) => '\x1b[0mrow-' + i) });
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
  const rect = await p.locator('.xterm-screen').boundingBox();
  const cols = await p.evaluate(() => probe.term.cols);
  const select = async () => {
    const cell = rect.width / cols;
    await p.mouse.move(rect.x + cell * 0.1, rect.y + 8);
    await p.mouse.down();
    await p.mouse.move(rect.x + cell * 5.8, rect.y + 8, { steps: 4 });
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
    probe.onHistory({ pane_id: 1, first: 0, top: 0,
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
    probe.onHistory({ pane_id: 1, first: 0, top: 0, rows: ['\x1b[0m中文 history'] });
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
