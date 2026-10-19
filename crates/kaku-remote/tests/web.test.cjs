// Run with node --test; Playwright is a test-only dependency supplied by CI.
const { test, before, after } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { chromium } = require('playwright');
const root = path.resolve(__dirname, '../../..');
let browser;
const launchOptions = (args = []) => ({ args,
  ...(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE } : {}) });
// Most tests read the DOM renderer's rows, so this browser has no WebGL and
// also covers the page's fallback; the WebGL test launches its own.
before(async () => {
  browser = await chromium.launch(launchOptions(['--disable-webgl', '--disable-3d-apis']));
});
after(async () => { await browser?.close(); });

async function page(t, hash = '#t=fixture', options = {}, context = browser) {
  const { platform, noConfig, ...pageOptions } = options;
  const p = await context.newPage({ viewport: { width: 1000, height: 600 }, ...pageOptions });
  if (platform) await p.addInitScript(value => Object.defineProperty(navigator, 'platform', { value }), platform);
  t.after(() => p.close());
  const errors = [];
  p.on('pageerror', e => errors.push(e.message));
  await p.addInitScript(() => {
    window.messages = [];
    window.connections = [];
    class Socket extends EventTarget {
      static OPEN = 1; static CLOSED = 3; static CLOSING = 2;
      constructor() { super(); this.readyState = 1; connections.push(this); setTimeout(() => this.onopen?.(), 0); }
      send(data) { if (typeof data === 'string') messages.push(JSON.parse(data)); else (this.binary ||= []).push(new Uint8Array(data)); }
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
    .replace(/\}\)\(\);\s*<\/script>/, 'window.probe = {term, handle, attach, openPast, onHistory, pastScroll, sendFile, sendFiles, snapshots, download};\n})();\n</script>');
  await p.route('http://localhost:9988/**', route => {
    const name = new URL(route.request().url()).pathname.split('/').pop();
    if (!name) return route.fulfill({ contentType: 'text/html', body: html });
    const file = path.join(root, 'crates/kaku-remote/assets/web', name);
    return fs.existsSync(file) ? route.fulfill({ path: file }) : route.fulfill({ status: 404, body: '' });
  });
  await p.goto('http://localhost:9988/' + hash);
  await p.waitForTimeout(100);
  // Kaku sends its config first on every session; the page claims no size
  // before it and the bundled font are in.
  if (!noConfig) await p.evaluate(() => { if (!window.probe) return; probe.handle({ type: 'config' }); return document.fonts.load('16px "Kaku Mono"'); });
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

// A Mac for relay tests: its signing key, made here as Kaku makes its own.
const { webcrypto } = require('node:crypto');
async function newMac() {
  const keys = await webcrypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, true, ['sign', 'verify']);
  return {
    pkcs8: Buffer.from(await webcrypto.subtle.exportKey('pkcs8', keys.privateKey)).toString('base64'),
    pub: Buffer.from(await webcrypto.subtle.exportKey('raw', keys.publicKey)),
  };
}
// The form Kaku prints: only the one-time secret. Kaku sends its key in the
// handshake.
const pairingLink = (mac, secret) => `#${secret.toString('base64url')}`;
// Earlier Kaku put its key in the link too.
const legacyLink = (mac, secret) => `#p=${secret.toString('base64url')}&m=${mac.pub.toString('base64url')}`;

// Act as Kaku on the page's newest relay connection: exchange keys, sign the
// exchange as `signer`, send its key with a tag under `tag` (Kaku holds the
// link's secret while the link is fresh; defaults to `secret`), read the
// page's proof, and (if `admit`) answer it as Kaku does once a device is in.
// Returns the proof and whether its signature, and its pairing proof for
// `secret`, check out.
async function handshake(p, signer, { admit = true, secret = null, tag = secret } = {}) {
  return p.evaluate(async ({ pkcs8, pub, secret, tag, admit }) => {
    const sub = crypto.subtle, enc = new TextEncoder(), dec = new TextDecoder();
    const sleep = ms => new Promise(r => setTimeout(r, ms));
    const b64 = b => btoa(String.fromCharCode(...new Uint8Array(b))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    const unb64 = s => Uint8Array.from(atob(s.replace(/-/g, '+').replace(/_/g, '/')), c => c.charCodeAt(0));
    const socket = connections.at(-1);
    const sent = () => socket.binary?.length || 0;
    const eph = await sub.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, false, ['deriveBits']);
    const macEph = new Uint8Array(await sub.exportKey('raw', eph.publicKey));
    const hello = new Uint8Array(69); hello.set(enc.encode('KCH3')); hello.set(macEph, 4);
    socket.onmessage({ data: hello.buffer });
    while (sent() < 1) await sleep(5);
    const answer = socket.binary[0];
    if (answer.length !== 69 || dec.decode(answer.slice(0, 4)) !== 'KCB3') throw new Error('no page hello');
    const host = location.pathname.split('/').find(x => /^[0-9a-f]{32}$/.test(x)) || '';
    const label = enc.encode('kaku-remote v3'), id = enc.encode(host);
    const all = new Uint8Array(label.length + id.length + 130);
    all.set(label); all.set(id, label.length); all.set(macEph, label.length + id.length); all.set(answer.slice(4), label.length + id.length + 65);
    const transcript = new Uint8Array(await sub.digest('SHA-256', all));
    const peer = await sub.importKey('raw', answer.slice(4), { name: 'ECDH', namedCurve: 'P-256' }, false, []);
    const shared = await sub.deriveBits({ name: 'ECDH', public: peer }, eph.privateKey, 256);
    const hkdf = await sub.importKey('raw', shared, 'HKDF', false, ['deriveKey']);
    const derive = info => sub.deriveKey({ name: 'HKDF', hash: 'SHA-256', salt: transcript, info: enc.encode(info) },
      hkdf, { name: 'AES-GCM', length: 256 }, false, ['encrypt', 'decrypt']);
    const toHost = await derive('kaku c2h'), fromHost = await derive('kaku h2c');
    const aad = (dir, seq) => { const l = enc.encode(dir); const out = new Uint8Array(l.length + 8); out.set(l); new DataView(out.buffer).setBigUint64(l.length, BigInt(seq)); return out; };
    const deliver = async (msg, seq) => {
      const iv = crypto.getRandomValues(new Uint8Array(12));
      const data = new Uint8Array(await sub.encrypt({ name: 'AES-GCM', iv, additionalData: aad('kaku-web h2c', seq) }, fromHost, enc.encode(JSON.stringify(msg))));
      const frame = new Uint8Array(12 + data.length); frame.set(iv); frame.set(data, 12);
      socket.onmessage({ data: frame.buffer });
    };
    const signing = await sub.importKey('pkcs8', unb64(pkcs8.replace(/\+/g, '-').replace(/\//g, '_')), { name: 'ECDSA', namedCurve: 'P-256' }, false, ['sign']);
    const macAuth = { type: 'mac_auth', sig: b64(await sub.sign({ name: 'ECDSA', hash: 'SHA-256' }, signing, transcript)), key: pub };
    if (tag) {
      const label = enc.encode('kaku-remote mac key'), key = unb64(pub);
      const tagged = new Uint8Array(label.length + transcript.length + key.length);
      tagged.set(label); tagged.set(transcript, label.length); tagged.set(key, label.length + transcript.length);
      const hmac = await sub.importKey('raw', unb64(tag), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
      macAuth.tag = b64(await sub.sign('HMAC', hmac, tagged));
    }
    await deliver(macAuth, 0);
    for (let i = 0; i < 100 && sent() < 2; i++) await sleep(5);
    if (sent() < 2) return { proof: null };
    const frame = socket.binary[1];
    const plain = await sub.decrypt({ name: 'AES-GCM', iv: frame.slice(0, 12), additionalData: aad('kaku-web c2h', 0) }, toHost, frame.slice(12));
    const proof = JSON.parse(dec.decode(plain));
    const device = await sub.importKey('raw', unb64(proof.device), { name: 'ECDSA', namedCurve: 'P-256' }, false, ['verify']);
    const signed = await sub.verify({ name: 'ECDSA', hash: 'SHA-256' }, device, unb64(proof.sig), transcript);
    let paired = null;
    if (secret) {
      const key = await sub.importKey('raw', unb64(secret), { name: 'HMAC', hash: 'SHA-256' }, false, ['verify']);
      paired = await sub.verify('HMAC', key, unb64(proof.proof || ''), transcript);
    }
    if (admit) await deliver({ type: 'pong' }, 1);
    await sleep(50);
    return { proof, signed, paired };
  }, { pkcs8: signer.pkcs8, pub: signer.pub.toString('base64url'), secret: secret && secret.toString('base64url'), tag: tag && tag.toString('base64url'), admit });
}
const storedDevice = p => p.evaluate(() => new Promise(resolve => {
  const open = indexedDB.open('kaku', 1);
  open.onupgradeneeded = () => open.result.createObjectStore('devices');
  open.onsuccess = () => {
    const req = open.result.transaction('devices').objectStore('devices').get(location.pathname);
    req.onsuccess = () => { open.result.close(); resolve(req.result ? Array.from(req.result.publicKey) : null); };
  };
}));

test('incomplete, old or missing pairing fails visibly without throwing', async t => {
  for (const [hash, message] of [['#p=bad&m=YQ', /incomplete/], ['#k=' + Buffer.alloc(32, 1).toString('base64url'), /earlier Kaku/], ['', /Pair this browser first/]]) {
    const { p, errors } = await page(t, hash);
    assert.match(await p.locator('#stage').innerText(), message);
    assert.equal(await p.evaluate(() => location.hash), '');
    assert.equal(await p.evaluate(() => connections.length), 0);
    assert.deepEqual(errors, []);
  }
});

test('a pairing link proves itself once and the device signs in afterwards', async t => {
  const mac = await newMac();
  const secret = Buffer.alloc(16, 3);
  const context = await browser.newContext();
  t.after(() => context.close());
  const { p, errors } = await page(t, pairingLink(mac, secret), {}, context);
  assert.equal(await p.evaluate(() => location.hash), '');
  // Nothing is kept before Kaku admits the browser.
  assert.equal(await storedDevice(p), null);
  const first = await handshake(p, mac, { secret });
  assert.equal(first.proof.type, 'pair');
  assert.equal(first.signed, true);
  assert.equal(first.paired, true);
  assert.match(first.proof.name, /on /);
  assert.match(await p.locator('#toast').innerText(), /Paired/);
  const kept = await storedDevice(p);
  assert.ok(kept);
  // Back later with no link: the same device signs in, with no pairing proof.
  const next = await page(t, '', {}, context);
  assert.equal(await next.p.evaluate(() => connections.length), 1);
  const again = await handshake(next.p, mac);
  assert.equal(again.proof.type, 'auth');
  assert.equal(again.signed, true);
  assert.equal(again.proof.proof, undefined);
  assert.deepEqual(await storedDevice(next.p), kept);
  assert.deepEqual([...errors, ...next.errors], []);
});

test('a link from earlier Kaku with the key in it still pairs', async t => {
  const mac = await newMac();
  const secret = Buffer.alloc(16, 5);
  const context = await browser.newContext();
  t.after(() => context.close());
  const { p, errors } = await page(t, legacyLink(mac, secret), {}, context);
  assert.equal(await p.evaluate(() => location.hash), '');
  const first = await handshake(p, mac, { secret, tag: null });
  assert.equal(first.signed, true);
  assert.equal(first.paired, true);
  const next = await page(t, '', {}, context);
  const again = await handshake(next.p, mac);
  assert.equal(again.proof.type, 'auth');
  assert.equal(again.signed, true);
  assert.deepEqual([...errors, ...next.errors], []);
});

test('a key the link secret did not vouch for is refused', async t => {
  const mac = await newMac();
  const impostor = await newMac();
  // The relay can stand in as a Mac but never sees the secret, so its tag is
  // under some other secret, or missing, which reads as a used link.
  for (const [tag, notice] of [[Buffer.alloc(16, 9), /Could not verify the Mac/], [null, /expired/]]) {
    const { p, errors } = await page(t, pairingLink(mac, Buffer.alloc(16, 4)));
    const result = await handshake(p, impostor, { tag });
    assert.equal(result.proof, null);
    assert.equal(await p.evaluate(() => connections.at(-1).readyState), 3);
    assert.match(await p.locator('#notice').innerText(), notice);
    await p.waitForTimeout(700);
    assert.equal(await p.evaluate(() => connections.length), 1);
    assert.equal(await storedDevice(p), null);
    assert.deepEqual(errors, []);
  }
});

test('a link from another Mac pairs this browser with a new key', async t => {
  const mac = await newMac();
  const other = await newMac();
  const context = await browser.newContext();
  t.after(() => context.close());
  const { p } = await page(t, pairingLink(mac, Buffer.alloc(16, 10)), {}, context);
  await handshake(p, mac, { secret: Buffer.alloc(16, 10) });
  const kept = await storedDevice(p);
  const same = await page(t, pairingLink(mac, Buffer.alloc(16, 11)), {}, context);
  assert.equal((await handshake(same.p, mac, { secret: Buffer.alloc(16, 11) })).paired, true);
  assert.deepEqual(await storedDevice(same.p), kept);
  const next = await page(t, pairingLink(other, Buffer.alloc(16, 12)), {}, context);
  const paired = await handshake(next.p, other, { secret: Buffer.alloc(16, 12) });
  assert.equal(paired.signed, true);
  assert.equal(paired.paired, true);
  assert.notDeepEqual(await storedDevice(next.p), kept);
});

test('a page proves nothing to a Mac that is not the one it paired with', async t => {
  const mac = await newMac();
  const impostor = await newMac();
  const { p, errors } = await page(t, legacyLink(mac, Buffer.alloc(16, 4)));
  const result = await handshake(p, impostor);
  // The page sent its hello and nothing else: no proof, no input.
  assert.equal(result.proof, null);
  assert.equal(await p.evaluate(() => connections.at(-1).readyState), 3);
  assert.match(await p.locator('#notice').innerText(), /Could not verify the Mac/);
  await p.waitForTimeout(700);
  assert.equal(await p.evaluate(() => connections.length), 1);
  assert.equal(await storedDevice(p), null);
  assert.deepEqual(errors, []);
});

test('a message sealed for another connection closes the connection', async t => {
  const mac = await newMac();
  const { p } = await page(t, pairingLink(mac, Buffer.alloc(16, 5)));
  const closed = await p.evaluate(async () => {
    const socket = connections.at(-1);
    let closed = false; socket.close = () => { closed = true; };
    const eph = await crypto.subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, true, ['deriveBits']);
    const hello = new Uint8Array(69); hello.set(new TextEncoder().encode('KCH3'));
    hello.set(new Uint8Array(await crypto.subtle.exportKey('raw', eph.publicKey)), 4);
    socket.onmessage({ data: hello.buffer });
    await new Promise(r => setTimeout(r, 50));
    // A recorded frame from another connection: other keys, so it cannot open.
    const frame = crypto.getRandomValues(new Uint8Array(60));
    socket.onmessage({ data: frame.buffer });
    await new Promise(r => setTimeout(r, 100));
    return closed;
  });
  assert.equal(closed, true);
});

test('a removed device forgets its pairing and an expired link keeps nothing', async t => {
  const mac = await newMac();
  const secret = Buffer.alloc(16, 6);
  const context = await browser.newContext();
  t.after(() => context.close());
  const { p } = await page(t, pairingLink(mac, secret), {}, context);
  await handshake(p, mac, { secret });
  const next = await page(t, '', {}, context);
  await handshake(next.p, mac);
  await next.p.evaluate(() => connections.at(-1).close(4403));
  assert.match(await next.p.locator('#notice').innerText(), /not paired/);
  await next.p.waitForTimeout(100);
  assert.equal(await storedDevice(next.p), null);
  // An expired or used link: its own message, and nothing saved.
  const late = await page(t, pairingLink(mac, Buffer.alloc(16, 7)));
  await handshake(late.p, mac, { admit: false });
  await late.p.evaluate(() => connections.at(-1).close(4401));
  assert.match(await late.p.locator('#notice').innerText(), /expired/);
  assert.equal(await storedDevice(late.p), null);
  await late.p.waitForTimeout(700);
  assert.equal(await late.p.evaluate(() => connections.length), 1);
});

test('a paired device survives closing and relaunching the browser process', async t => {
  const profile = fs.mkdtempSync(path.join(require('node:os').tmpdir(), 'kaku-browser-'));
  const options = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE
    ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE } : {};
  let context;
  t.after(async () => { await context?.close(); fs.rmSync(profile, { recursive: true, force: true }); });
  context = await chromium.launchPersistentContext(profile, options);
  const mac = await newMac();
  const secret = Buffer.alloc(16, 8);
  const first = await page(t, pairingLink(mac, secret), {}, context);
  await handshake(first.p, mac, { secret });
  await context.close();
  context = await chromium.launchPersistentContext(profile, options);
  const next = await page(t, '', {}, context);
  assert.equal(await next.p.evaluate(() => connections.length), 1);
  assert.equal((await handshake(next.p, mac)).proof.type, 'auth');
  assert.deepEqual(next.errors, []);
});

test('blocked site storage still pairs this tab and says it will not last', async t => {
  const context = await browser.newContext();
  t.after(() => context.close());
  await context.addInitScript(() => {
    const failing = { open() { const req = {}; setTimeout(() => { req.error = new DOMException('blocked', 'SecurityError'); req.onerror?.(); }); return req; } };
    Object.defineProperty(window, 'indexedDB', { get() { return failing; } });
  });
  const mac = await newMac();
  const secret = Buffer.alloc(16, 9);
  const { p, errors } = await page(t, pairingLink(mac, secret), {}, context);
  const result = await handshake(p, mac, { secret });
  assert.equal(result.paired, true);
  assert.match(await p.locator('#toast').innerText(), /this tab only/);
  assert.deepEqual(errors, []);
});

test('connection help and first-use notice fit narrow and short viewports', async t => {
  const mac = await newMac();
  const secret = Buffer.alloc(16, 10);
  const { p } = await page(t, pairingLink(mac, secret));
  await handshake(p, mac, { secret });
  for (const width of [375, 1280]) for (const height of [240, 900]) {
    await p.setViewportSize({ width, height });
    await p.locator('.xterm-helper-textarea').focus();
    await p.keyboard.press('Meta+Slash');
    const box = await p.locator('#help .card').boundingBox();
    assert.ok(box.x >= 15 && box.x + box.width <= width - 15);
    assert.ok(box.y >= 15 && box.y + box.height <= height - 15);
    assert.ok(await p.locator('.connection-note').count());
    await p.keyboard.press('Escape');
    assert.equal(await p.locator('#toast').evaluate(el => el.scrollWidth <= el.clientWidth), true);
  }
});

test('Cmd+clicking a Mac path downloads the file in acknowledged chunks', async t => {
  const { p, errors } = await page(t);
  await screen(p, 'Saved to ~/Desktop/shot.png.');
  // Hovering the path with Cmd shows it as a link; the click asks the Mac.
  const box = await p.locator('.xterm-screen').boundingBox();
  const cell = await p.evaluate(() => probe.term._core._renderService.dimensions.css.cell);
  await p.keyboard.down('Meta');
  await p.mouse.move(box.x + cell.width * 14.5, box.y + cell.height / 2);
  await p.mouse.click(box.x + cell.width * 14.5, box.y + cell.height / 2);
  await p.keyboard.up('Meta');
  const request = await p.waitForFunction(() => messages.find(m => m.type === 'download'));
  const { id, path: asked } = await request.jsonValue();
  assert.equal(asked, '~/Desktop/shot.png');
  const bytes = Buffer.from(Array.from({ length: 300000 }, (_, i) => i % 251));
  const downloaded = p.waitForEvent('download');
  await p.evaluate(({ id, chunks }) => {
    probe.handle({ type: 'file_start', id, name: 'shot.png', size: chunks.reduce((n, c) => n + atob(c).length, 0) });
    for (const data of chunks) probe.handle({ type: 'file_chunk', id, data });
    probe.handle({ type: 'file_end', id });
  }, { id, chunks: [bytes.subarray(0, 200000).toString('base64'), bytes.subarray(200000).toString('base64')] });
  const file = await downloaded;
  assert.equal(file.suggestedFilename(), 'shot.png');
  assert.deepEqual(fs.readFileSync(await file.path()), bytes);
  // Each chunk was acknowledged with the bytes held so far.
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'download_ack').map(m => m.received)), [200000, 300000]);
  assert.match(await p.locator('#toast').innerText(), /Downloaded shot.png/);
  // A refused path says why, and a dropped link does not leave it hanging.
  await p.evaluate(() => probe.handle({ type: 'file_error', id: 99, detail: 'x' }));
  await p.evaluate(id => probe.handle({ type: 'file_error', id, detail: '~/Desktop is a folder' }), id);
  assert.deepEqual(errors, []);
});

test('local token stays tab-only', async t => {
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

test('clicking back to a cached tab shows it in the next frame, never blank', async t => {
  const { p } = await page(t);
  await screen(p, 'first');
  await p.evaluate(() => probe.attach({ pane_id: 2, tab_id: 2 }));
  await screen(p, 'second', 2);
  await p.evaluate(() => {
    const rows = () => document.querySelector('.xterm-rows').textContent;
    window.__states = [];
    new MutationObserver(() => window.__states.push(rows().trim()))
      .observe(document.querySelector('.xterm-rows'), { subtree: true, childList: true, characterData: true });
    // After the tab's own click handler: what the first frame after it shows.
    window.addEventListener('click', () => requestAnimationFrame(() => { window.__firstFrame = rows(); }), { once: true });
  });
  await p.locator('#bar .tab').first().click();
  await p.waitForFunction(() => window.__firstFrame !== undefined);
  // xterm parses a write on a timer, which Chrome runs only after the frame
  // following a click; the snapshot then showed a frame late, and a
  // term.reset() before it blanked the screen in between.
  assert.ok(await p.evaluate(() => window.__firstFrame.includes('first')));
  assert.ok(!(await p.evaluate(() => window.__states.some(s => s === ''))));
});

test('a closed tab leaves the bar on the click and stays gone until Kaku agrees', async t => {
  const { p } = await page(t);
  await screen(p, 'first');
  await p.evaluate(() => probe.attach({ pane_id: 2, tab_id: 2 }));
  await screen(p, 'second', 2);
  const titles = () => p.evaluate(() => [...document.querySelectorAll('#bar .tab')].map(b => b.textContent.trim()));
  await p.evaluate(() => { messages.length = 0; });
  await p.locator('#bar .tab').nth(1).hover();
  await p.locator('#bar .tab').nth(1).locator('.close').click();
  // Gone at once, and the neighbour shown from its snapshot.
  assert.deepEqual(await titles(), ['one']);
  await p.waitForFunction(() => document.querySelector('.xterm-rows').textContent.includes('first'), null, { timeout: 1000 });
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'close_tab').map(m => m.tab_id)), [2]);
  // Drop the request in flight and reconnect before the grace expires.
  await p.evaluate(() => connections.at(-1).close(1006));
  await p.waitForFunction(() => connections.length === 2);
  await p.evaluate(() => probe.handle({ type: 'tabs', tabs: [{ tab_id: 1, pane_id: 1, panes: [], title: 'one', active: true }, { tab_id: 2, pane_id: 2, panes: [], title: 'two' }] }));
  assert.deepEqual(await titles(), ['one']);
  // A quick reconnect returns this same list before the grace expires.
  // The host deduplicates it afterwards, so recovery must use a real timer.
  await p.waitForTimeout(3300);
  assert.deepEqual(await titles(), ['one', 'two']);
  // Hiding a pending close must not discard its cached screen.
  assert.equal(await p.evaluate(() => probe.snapshots.has(2)), true);
});

test('closing while disconnected leaves the live tab visible', async t => {
  const { p } = await page(t);
  await screen(p);
  await p.evaluate(() => { connections.at(-1).readyState = 3; messages.length = 0; });
  await p.locator('#bar .tab').nth(1).hover();
  await p.locator('#bar .tab').nth(1).locator('.close').click();
  assert.deepEqual(await p.evaluate(() => [...document.querySelectorAll('#bar .tab')].map(b => b.textContent.trim())), ['one', 'two']);
  assert.equal(await p.evaluate(() => messages.filter(m => m.type === 'close_tab').length), 0);
});

test('an acknowledged close stays gone after the recovery timer', async t => {
  const { p } = await page(t);
  await screen(p);
  await p.locator('#bar .tab').nth(1).hover();
  await p.locator('#bar .tab').nth(1).locator('.close').click();
  await p.evaluate(() => probe.handle({ type: 'tabs', tabs: [{ tab_id: 1, pane_id: 1, panes: [], title: 'one', active: true }] }));
  await p.waitForTimeout(3300);
  assert.deepEqual(await p.evaluate(() => [...document.querySelectorAll('#bar .tab')].map(b => b.textContent.trim())), ['one']);
});

test('the terminal names no emoji font, so status symbols keep their color', async t => {
  const { p } = await page(t);
  await screen(p);
  // Named, an emoji font drew Claude Code's ⏺ and ✳ (green, red by status)
  // as fixed-color emoji whenever the fonts before it lacked them.
  const family = await p.evaluate(() => probe.term.options.fontFamily);
  assert.doesNotMatch(family, /Emoji/i);
  assert.match(family, /Kaku Mono/);
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

async function focusHistory(p, select = false) {
  await p.evaluate(select => {
    probe.openPast();
    probe.onHistory({ request: messages.filter(m => m.type === 'history').at(-1).request,
      pane_id: 1, first: 0, top: 0, rows: ['\x1b[0mhistory text'] });
    document.body.tabIndex = -1;
    document.body.focus();
    if (select) {
      const range = document.createRange();
      range.selectNodeContents(document.querySelector('#past .row'));
      getSelection().removeAllRanges(); getSelection().addRange(range);
    }
    messages.length = 0;
  }, select);
}

for (const [name, key, expected] of [
  ['newline', 'Shift+Enter', { type: 'text', data: '\n' }],
  ['function key', 'F2', { type: 'key', key: 'F2', ctrl: false, alt: false, shift: false }],
  ['word motion', process.platform === 'darwin' ? 'Alt+ArrowLeft' : 'Control+ArrowLeft', { type: 'key', key: 'b', ctrl: false, alt: true, shift: false }],
  ['plain character', 'x', { type: 'text', data: 'x' }],
]) test('history first key preserves ' + name, async t => {
  const { p, errors } = await page(t); await screen(p); await captureClipboard(p);
  await focusHistory(p);
  await p.keyboard.press(key);
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'text' || m.type === 'key')), [expected]);
  assert.deepEqual(errors, []);
});

test('history paste keeps the browser default and focuses the terminal', async t => {
  const { p } = await page(t, '#t=fixture', { platform: 'Linux x86_64' });
  await screen(p); await captureClipboard(p); await focusHistory(p);
  const result = await p.evaluate(() => {
    const ev = new KeyboardEvent('keydown', { key: 'v', code: 'KeyV', ctrlKey: true, bubbles: true, cancelable: true });
    document.body.dispatchEvent(ev);
    const focused = document.activeElement.classList.contains('xterm-helper-textarea');
    const data = new DataTransfer(); data.setData('text/plain', 'paste fixture');
    document.activeElement.dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }));
    return { prevented: ev.defaultPrevented, focused, inputs: messages.filter(m => ['text', 'key', 'paste'].includes(m.type)) };
  });
  assert.deepEqual(result, { prevented: false, focused: true, inputs: [{ type: 'paste', data: 'paste fixture' }] });
});

test('history first IME key focuses input without cancelling composition', async t => {
  const { p, errors } = await page(t); await screen(p); await captureClipboard(p); await focusHistory(p);
  const result = await p.evaluate(() => {
    const ev = new KeyboardEvent('keydown', { key: 'Unidentified', keyCode: 229, bubbles: true, cancelable: true });
    document.body.dispatchEvent(ev);
    return { prevented: ev.defaultPrevented, focused: document.activeElement.classList.contains('xterm-helper-textarea') };
  });
  assert.deepEqual(result, { prevented: false, focused: true });
  const cdp = await p.context().newCDPSession(p);
  await cdp.send('Input.imeSetComposition', { text: 'nihao', selectionStart: 5, selectionEnd: 5 });
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'text')), []);
  await cdp.send('Input.insertText', { text: '你好' });
  await p.waitForTimeout(30);
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'text')), [{ type: 'text', data: '你好' }]);
  assert.deepEqual(errors, []);
});

test('queued wheel scrolling cannot follow attachment to another pane', async t => {
  const { p } = await page(t); await screen(p);
  await p.evaluate(() => {
    probe.handle({ type: 'frame', pane_id: 1, cols: 80, rows: 24, local_scroll: false, data: '' });
    messages.length = 0;
    document.querySelector('#stage').dispatchEvent(new WheelEvent('wheel', { deltaY: 100, bubbles: true, cancelable: true }));
    probe.attach({ pane_id: 2, tab_id: 2 }, true);
  });
  await p.waitForTimeout(50);
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'scroll')), []);
});

test('touch fling stops when attachment changes', async t => {
  const { p } = await page(t, '#t=fixture', { hasTouch: true }); await screen(p);
  await p.evaluate(() => {
    probe.handle({ type: 'frame', pane_id: 1, cols: 80, rows: 24, local_scroll: false, data: '' });
    const stage = document.querySelector('#stage');
    const touch = y => new Touch({ identifier: 1, target: stage, clientX: 50, clientY: y });
    stage.dispatchEvent(new TouchEvent('touchstart', { touches: [touch(250)], bubbles: true }));
    stage.dispatchEvent(new TouchEvent('touchmove', { touches: [touch(50)], bubbles: true, cancelable: true }));
    stage.dispatchEvent(new TouchEvent('touchend', { touches: [], bubbles: true, cancelable: true }));
    probe.attach({ pane_id: 2, tab_id: 2 }, true);
    messages.length = 0;
  });
  await p.waitForTimeout(100);
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'scroll' || m.type === 'history')), []);
});

test('changing a custom font back to the default restores the bundled stack', async t => {
  const { p } = await page(t); await screen(p);
  const fonts = await p.evaluate(() => {
    const initial = probe.term.options.fontFamily;
    probe.handle({ type: 'config', font_family: 'Menlo' });
    const custom = probe.term.options.fontFamily;
    probe.handle({ type: 'config', font_family: 'JetBrains Mono' });
    return { initial, custom, restored: probe.term.options.fontFamily };
  });
  assert.match(fonts.custom, /^"Menlo",/);
  assert.equal(fonts.restored, fonts.initial);
});

test('history shortcuts do not duplicate tab actions or leak find keys', async t => {
  const { p, errors } = await page(t, '#t=fixture', { platform: 'Linux' });
  await screen(p);
  await focusHistory(p);
  await p.keyboard.press('Control+Shift+f');
  assert.equal(await p.locator('#find').evaluate(el => el.classList.contains('show')), true);
  assert.deepEqual(await p.evaluate(() => messages.filter(m => ['key', 'text'].includes(m.type))), []);
  assert.deepEqual(errors, []);
  const mac = (await page(t, '#t=fixture', { platform: 'MacIntel' })).p;
  await screen(mac);
  await focusHistory(mac);
  await mac.keyboard.press('Meta+2');
  assert.deepEqual(await mac.evaluate(() => messages.filter(m => m.type === 'attach').map(m => m.pane_id)), [2]);
});

test('history copy keeps native selection and does not send terminal input', async t => {
  for (const platform of ['MacIntel', 'Linux']) {
    const { p } = await page(t, '#t=fixture', { platform });
    await screen(p);
    await captureClipboard(p);
    await focusHistory(p, true);
    const result = await p.evaluate(mac => {
      const before = String(getSelection());
      const ev = new KeyboardEvent('keydown', { key: 'c', code: 'KeyC', metaKey: mac, ctrlKey: !mac, bubbles: true, cancelable: true });
      document.body.dispatchEvent(ev);
      // A late font load can re-measure and send a resize; only input counts.
      const input = messages.filter(m => ['key', 'text'].includes(m.type));
      return { before, after: String(getSelection()), prevented: ev.defaultPrevented, messages: input };
    }, platform === 'MacIntel');
    assert.ok(result.before);
    assert.equal(result.after, result.before);
    assert.equal(result.prevented, false);
    assert.deepEqual(result.messages, []);
  }
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

test('phone, tablet and desktop grids stay within their viewports, the desktop bar one row tall', async t => {
  for (const [width, height, touch] of [[375, 667, true], [768, 1024, true], [844, 499, true], [844, 501, true], [1280, 720, false]]) {
    const { p, errors } = await page(t, '#t=fixture', { viewport: { width, height }, hasTouch: touch, isMobile: touch });
    await screen(p, 'terminal');
    // Apply the grid requested from the Mac, as a real size acknowledgment.
    // The bar settling to one row grows the stage once, so acknowledge the
    // follow-up request too.
    for (const seq of [2, 3]) {
      await p.evaluate(seq => {
        const size = messages.filter(m => m.type === 'resize').at(-1);
        if (!size) throw new Error('no requested grid');
        probe.handle({ type: 'frame', seq, pane_id: 1, ...size, type: 'frame', data: '\x1b[1;1Hterminal' });
      }, seq);
      await p.waitForTimeout(150);
    }
    const geometry = await p.evaluate(() => {
      const screen = document.querySelector('.xterm-screen').getBoundingClientRect();
      const stage = document.querySelector('#stage').getBoundingClientRect();
      const rows = messages.filter(m => m.type === 'resize').at(-1).rows;
      return { overflow: document.documentElement.scrollWidth > innerWidth, right: screen.right - stage.right, bottom: screen.bottom - stage.bottom,
        bar: document.querySelector('#bar').getBoundingClientRect().height, row: screen.height / rows };
    });
    assert.equal(geometry.overflow, false, JSON.stringify({ width, height, geometry }));
    // Like Kaku's own tab bar the desktop bar is one terminal row; touch keeps its finger-sized bar.
    if (!touch) assert.ok(Math.abs(geometry.bar - geometry.row) <= 1, JSON.stringify({ width, height, geometry }));
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

test('live copy joins soft wraps and keeps hard breaks across all copy paths and snapshots', async t => {
  const { p, errors } = await page(t);
  await screen(p);
  await captureClipboard(p);
  const result = await p.evaluate(async () => {
    const drain = () => new Promise(resolve => probe.term.write('', resolve));
    probe.handle({ type: 'frame', pane_id: 1, cols: 10, rows: 3, local_scroll: true, wrapped: [true, false, false],
      data: '\x1b[2J\x1b[1;1HABCDEFGHIJ\x1b[2;1HK\x1b[3;1H你好' });
    await drain();
    document.querySelector('#stage').dispatchEvent(new PointerEvent('pointerdown', { button: 0, bubbles: true }));
    probe.term.select(0, 0, 24);
    window.dispatchEvent(new MouseEvent('mouseup', { button: 0 }));
    const copy = () => {
      const data = new DataTransfer();
      document.querySelector('.xterm-helper-textarea').dispatchEvent(new ClipboardEvent('copy', { clipboardData: data, bubbles: true, cancelable: true }));
      return data.getData('text/plain');
    };
    const automatic = copied.at(-1), manual = copy();
    document.querySelector('#term').dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: 200, clientY: 100 }));
    [...document.querySelectorAll('#menu button')].find(b => b.textContent === 'Copy').click();
    const menu = copied.at(-1);
    probe.attach({ tab_id: 2, pane_id: 2 });
    probe.handle({ type: 'frame', pane_id: 2, cols: 10, rows: 3, data: '\x1b[2J\x1b[1;1Hother' });
    await drain();
    probe.attach({ tab_id: 1, pane_id: 1 });
    await drain();
    probe.term.select(0, 0, 24);
    return { automatic, manual, menu, cached: copy() };
  });
  assert.deepEqual(result, Object.fromEntries(['automatic', 'manual', 'menu', 'cached'].map(k => [k, 'ABCDEFGHIJK\n你好'])));
  assert.deepEqual(errors, []);
});

test('copy does not guess soft wraps from a full row when metadata is absent or false', async t => {
  const { p } = await page(t);
  await screen(p);
  await captureClipboard(p);
  for (const wrapped of [undefined, [false, false, false]]) {
    const selected = await p.evaluate(async wrapped => {
      probe.handle({ type: 'frame', pane_id: 1, cols: 10, rows: 3, wrapped,
        data: '\x1b[2J\x1b[1;1HABCDEFGHIJ\x1b[2;1HK' });
      await new Promise(resolve => probe.term.write('', resolve));
      probe.term.select(0, 0, 11);
      const data = new DataTransfer();
      document.querySelector('.xterm-helper-textarea').dispatchEvent(new ClipboardEvent('copy', { clipboardData: data, bubbles: true, cancelable: true }));
      return data.getData('text/plain');
    }, wrapped);
    assert.equal(selected, 'ABCDEFGHIJ\nK');
  }
});

test('history copy preserves soft wraps when older pages are prepended', async t => {
  const { p } = await page(t);
  await screen(p);
  await captureClipboard(p);
  const selected = await p.evaluate(() => {
    const request = () => messages.filter(m => m.type === 'history').at(-1).request;
    probe.openPast();
    probe.onHistory({ request: request(), pane_id: 1, first: 0, top: 2, rows: ['tail', 'hard'], wrapped: [false, false] });
    probe.pastScroll(1e7);
    probe.onHistory({ request: request(), pane_id: 1, first: 0, top: 0, rows: ['ABCDEFGHIJ', 'K'], wrapped: [true, true] });
    const rows = document.querySelectorAll('#past .row');
    const range = document.createRange();
    range.setStart(rows[0].querySelector('span span').firstChild, 2);
    range.setEnd(rows[3].querySelector('span span').firstChild, 4);
    getSelection().removeAllRanges(); getSelection().addRange(range);
    const data = new DataTransfer();
    document.dispatchEvent(new ClipboardEvent('copy', { clipboardData: data, cancelable: true }));
    return data.getData('text/plain');
  });
  assert.equal(selected, 'CDEFGHIJKtail\nhard');
});

test('soft-wrap copy keeps spaces and partial endpoints and updates on a metadata-only frame', async t => {
  const { p } = await page(t);
  await screen(p);
  const result = await p.evaluate(async () => {
    const frame = wrapped => probe.handle({ type: 'frame', pane_id: 1, cols: 10, rows: 3, wrapped,
      data: '\x1b[2J\x1b[1;1HAB  CDEF  \x1b[2;1H  K' });
    const copy = async () => {
      await new Promise(resolve => probe.term.write('', resolve));
      probe.term.select(2, 0, 11);
      const data = new DataTransfer();
      document.querySelector('.xterm-helper-textarea').dispatchEvent(new ClipboardEvent('copy', { clipboardData: data, bubbles: true, cancelable: true }));
      return data.getData('text/plain');
    };
    frame([true, false, false]);
    const soft = await copy();
    probe.handle({ type: 'frame', pane_id: 1, cols: 10, rows: 3, wrapped: [false, false, false], data: '' });
    return { soft, hard: await copy() };
  });
  assert.deepEqual(result, { soft: '  CDEF    K', hard: '  CDEF  \n  K' });
});

test('soft-wrap metadata does not join a rectangular selection', async t => {
  const { p } = await page(t);
  await screen(p);
  await captureClipboard(p);
  await p.evaluate(async () => {
    probe.handle({ type: 'frame', pane_id: 1, cols: 10, rows: 3, wrapped: [true, false, false],
      data: '\x1b[2J\x1b[1;1HABCDEFGHIJ\x1b[2;1HTUVWXYZ' });
    await new Promise(resolve => probe.term.write('', resolve));
  });
  const rect = await p.locator('.xterm-screen').boundingBox();
  await p.keyboard.down('Alt');
  await p.mouse.move(rect.x + rect.width / 10 * 1.1, rect.y + rect.height / 6);
  await p.mouse.down();
  const current = await p.locator('.xterm-screen').boundingBox();
  await p.mouse.move(current.x + current.width / 10 * 4.1, current.y + current.height / 2, { steps: 3 });
  await p.mouse.up();
  await p.keyboard.up('Alt');
  const result = await p.evaluate(() => {
    const data = new DataTransfer();
    document.querySelector('.xterm-helper-textarea').dispatchEvent(new ClipboardEvent('copy', { clipboardData: data, bubbles: true, cancelable: true }));
    return { selected: probe.term.getSelection(), manual: data.getData('text/plain'), automatic: copied.at(-1) };
  });
  assert.equal(result.selected, 'BCD\nUVW');
  assert.equal(result.manual, result.selected);
  assert.equal(result.automatic, result.selected);
  // Shift extends the existing rectangular selection after Alt is released.
  await p.keyboard.down('Shift');
  await p.mouse.click(current.x + current.width / 10 * 5.1, current.y + current.height * 5 / 6);
  await p.keyboard.up('Shift');
  const extended = await p.evaluate(() => {
    const data = new DataTransfer();
    document.querySelector('.xterm-helper-textarea').dispatchEvent(new ClipboardEvent('copy', { clipboardData: data, bubbles: true, cancelable: true }));
    return { selected: probe.term.getSelection(), manual: data.getData('text/plain') };
  });
  assert.match(extended.selected, /\n/);
  assert.equal(extended.manual, extended.selected);
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
      // Unicode 11 widths give this emoji two cells, as Kaku does.
      assert.equal(found.links[0].range.start.x, 4);
      assert.equal(found.links[0].range.end.x, 27);
    }
  }
});

test('emoji take the two cells Kaku gives them, so their rows match the Mac', async t => {
  const { p } = await page(t);
  const row = await p.evaluate(async () => {
    probe.term.resize(80, 24);
    // Kaku pins each run to its column, as the server's frames do.
    await new Promise(resolve => probe.term.write('\x1b[2J\x1b[H\u2705\x1b[3G done \u26a1\x1b[11G x', resolve));
    const line = probe.term.buffer.active.getLine(0);
    return { text: line.translateToString(true), widths: [0, 8].map(x => line.getCell(x).getWidth()) };
  });
  assert.equal(row.text, '\u2705 done \u26a1 x');
  assert.deepEqual(row.widths, [2, 2]);
});

test('Mac paths stop at Chinese sentence punctuation but keep Chinese file names', async t => {
  const { p } = await page(t);
  const cases = [
    ['对照图在 /private/tmp/icons/all-ship.png，按字母顺序排：chatgpt', '/private/tmp/icons/all-ship.png'],
    ['存到了 ~/Desktop/shot.png。', '~/Desktop/shot.png'],
    ['见（~/Desktop/a.png）和「./b.txt」', '~/Desktop/a.png ./b.txt'],
    ['截图在 ~/桌面/截图.png', '~/桌面/截图.png'],
  ];
  for (const [source, expected] of cases) {
    const found = await p.evaluate(async source => {
      probe.term.resize(180, 24);
      await new Promise(resolve => probe.term.write('\x1b[?7l\x1b[2J\x1b[H' + source, resolve));
      const links = await new Promise(resolve => testLinkProvider.provideLinks(1, resolve));
      return (links || []).map(l => l.text).join(' ');
    }, source);
    assert.equal(found, expected);
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

test('notifications from the Mac reach a background page, and a click returns to their pane', async t => {
  const { p, errors } = await page(t); await screen(p);
  await p.evaluate(() => {
    window.shown = [];
    window.permissionAsked = 0;
    window.Notification = class {
      constructor(title, options) { this.title = title; this.body = options.body; shown.push(this); }
      close() {}
    };
    Notification.permission = 'default';
    Notification.requestPermission = async () => { permissionAsked++; Notification.permission = 'granted'; return 'granted'; };
    window.pageHidden = false;
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => pageHidden });
    document.hasFocus = () => !pageHidden;
  });
  // In front: another tab's notice is a toast, the attached pane's is already on screen.
  await p.evaluate(() => probe.handle({ type: 'notify', pane_id: 1, title: 'Claude Code', body: 'Done' }));
  assert.equal(await p.locator('#toast.show').count(), 0);
  await p.evaluate(() => probe.handle({ type: 'notify', pane_id: 2, title: 'Claude Code', body: 'Done' }));
  assert.match(await p.locator('#toast').innerText(), /Claude Code: Done/);
  // In the background, without permission yet: offer it once, ask only on a click.
  await p.evaluate(() => { pageHidden = true; probe.handle({ type: 'notify', pane_id: 2, title: 'Build', body: 'ok' }); });
  assert.match(await p.locator('#toast').innerText(), /notifications/);
  assert.equal(await p.evaluate(() => permissionAsked), 0);
  await p.evaluate(() => { pageHidden = false; });
  await p.locator('#toast').click();
  assert.equal(await p.evaluate(() => permissionAsked), 1);
  // Granted: a background notice is the browser's own, and its click goes to that pane.
  await p.evaluate(() => { pageHidden = true; messages.length = 0; probe.handle({ type: 'notify', pane_id: 2, title: 'Build', body: 'ok' }); });
  assert.deepEqual(await p.evaluate(() => shown.map(n => [n.title, n.body])), [['Build', 'ok']]);
  await p.evaluate(() => shown[0].onclick());
  assert.equal(await p.evaluate(() => messages.some(m => m.type === 'attach' && m.pane_id === 2)), true);
  // An unrelated toast later is not a permission prompt.
  await p.evaluate(() => { pageHidden = false; probe.handle({ type: 'error', detail: 'something else' }); });
  await p.locator('#toast').click();
  assert.equal(await p.evaluate(() => permissionAsked), 1);
  assert.deepEqual(errors, []);
});

test('inline images from the Mac sit over their cells and leave with the pane', async t => {
  const { p, errors } = await page(t); await screen(p);
  const png = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==';
  await p.evaluate(png => {
    probe.handle({ type: 'image', id: 'ab12', mime: 'image/png', data: png });
    probe.handle({ type: 'frame', seq: 2, pane_id: 1, cols: 80, rows: 24, local_scroll: true, data: '',
      images: [{ id: 'ab12', col: 3, row: 2, cols: 4, rows: 2 }] });
  }, png);
  const box = await p.evaluate(() => {
    const img = document.querySelector('#images img');
    const cell = document.querySelector('.xterm-screen').getBoundingClientRect();
    return img && { left: parseFloat(img.style.left), top: parseFloat(img.style.top), width: parseFloat(img.style.width),
      cellW: cell.width / 80, cellH: cell.height / 24, height: parseFloat(img.style.height) };
  });
  assert.ok(box, 'image placed');
  assert.ok(Math.abs(box.left - 3 * box.cellW) < 1 && Math.abs(box.top - 2 * box.cellH) < 1, JSON.stringify(box));
  assert.ok(Math.abs(box.width - 4 * box.cellW) < 1 && Math.abs(box.height - 2 * box.cellH) < 1, JSON.stringify(box));
  // A frame without an images field keeps them; switching panes clears them.
  await p.evaluate(() => probe.handle({ type: 'frame', seq: 3, pane_id: 1, cols: 80, rows: 24, local_scroll: true, data: '\x1b[1;1Hx' }));
  assert.equal(await p.locator('#images img').count(), 1);
  await p.evaluate(() => probe.attach({ tab_id: 2, pane_id: 2 }, true));
  assert.equal(await p.locator('#images img').count(), 0);
  assert.deepEqual(errors, []);
});

// Selection and find (which selects its match) use Kaku's selection colors,
// text included; xterm.js's default kept white text on a light highlight.
test('selection uses Kaku\'s selection colors so highlighted text stays readable', async t => {
  const { p } = await page(t); await screen(p);
  const theme = await p.evaluate(() => {
    probe.handle({ type: 'config', background: '#16161a', foreground: '#d4d4d8', selection_bg: 'rgba(142,106,217,0.55)', selection_fg: '#ffffff' });
    const t = probe.term.options.theme;
    return [t.selectionBackground, t.selectionInactiveBackground, t.selectionForeground];
  });
  assert.deepEqual(theme, ['rgba(142,106,217,0.55)', 'rgba(142,106,217,0.55)', '#ffffff']);
});

test('the close confirmation answers to N, Y and clicks outside it', async t => {
  const { p } = await page(t); await screen(p);
  await p.evaluate(() => { messages.length = 0; probe.handle({ type: 'tabs', tabs: [
    { tab_id: 1, pane_id: 1, title: 'one', active: true },
    { tab_id: 2, pane_id: 2, title: 'two', confirm_close: true },
  ] }); });
  const closes = () => p.evaluate(() => messages.filter(m => m.type === 'close_tab').length);
  const ask = () => p.locator('#bar .tab').nth(1).click({ button: 'middle' });
  await ask();
  assert.equal(await p.locator('#dialog .title').innerText(), 'Close this tab?');
  assert.equal(await p.locator('#dialog [data-act="ok"]').innerText(), 'Close');
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

test('a split pane zooms from the menu or Cmd+Shift+Enter, and the menu names the state', async t => {
  const { p } = await page(t); await screen(p);
  const split = zoomed => p.evaluate(zoomed => probe.handle({ type: 'tabs', tabs: [
    { tab_id: 1, pane_id: 1, title: 'split', zoomed, panes: [
      { pane_id: 1, left: 0, top: 0, width: 40, height: 24 },
      { pane_id: 3, left: 40, top: 0, width: 40, height: 24 },
    ] }, { tab_id: 2, pane_id: 2, title: 'two' },
  ] }), zoomed);
  await split(false);
  await p.locator('.xterm-screen').click({ button: 'right', position: { x: 20, y: 10 } });
  await p.locator('#menu button').getByText('Zoom Pane', { exact: true }).click();
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'zoom_pane')), [{ type: 'zoom_pane', pane_id: 1 }]);
  await p.evaluate(() => { messages.length = 0; });
  await p.locator('.xterm-helper-textarea').focus();
  await p.keyboard.press('Meta+Shift+Enter');
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'zoom_pane' || m.type === 'text')), [{ type: 'zoom_pane', pane_id: 1 }]);
  await split(true);
  await p.locator('.xterm-screen').click({ button: 'right', position: { x: 20, y: 10 } });
  assert.equal(await p.locator('#menu button').getByText('Unzoom Pane', { exact: true }).count(), 1);
  await p.keyboard.press('Escape');
  // A tab without splits has nothing to zoom.
  await p.evaluate(() => probe.attach({ tab_id: 2, pane_id: 2 }, true));
  await p.locator('.xterm-screen').click({ button: 'right', position: { x: 20, y: 10 } });
  assert.equal(await p.locator('#menu button').getByText(/Zoom Pane/).count(), 0);
});

for (const entry of ['keyboard', 'menu']) test('browser zoom reclaims its grid after the authoritative frame via ' + entry, async t => {
  const { p, errors } = await page(t);
  await screen(p);
  await p.evaluate(() => document.fonts.ready);
  await p.waitForTimeout(200);
  const initial = await p.evaluate(async () => {
    const grid = messages.filter(m => m.type === 'resize').at(-1);
    probe.handle({ type: 'frame', pane_id: 1, cols: grid.cols, rows: grid.rows, data: '\x1b[2J\x1b[1;1Hzoom' });
    await new Promise(resolve => probe.term.write('', resolve));
    probe.handle({ type: 'tabs', tabs: [{ tab_id: 1, pane_id: 1, title: 'split', zoomed: false,
      panes: [{ pane_id: 1 }, { pane_id: 3 }] }] });
    messages.length = 0;
    return { grid, font: probe.term.options.fontSize };
  });
  const frame = cols => p.evaluate(async ({ cols, rows }) => {
    probe.handle({ type: 'frame', pane_id: 1, cols, rows, data: '' });
    await new Promise(resolve => probe.term.write('', resolve));
    return messages.filter(m => m.type === 'resize');
  }, { cols, rows: initial.grid.rows });
  // A resize from native Kaku or another browser does not give this page
  // permission to take the tab back without its own interaction.
  assert.deepEqual(await frame(initial.grid.cols * 2), []);
  await frame(initial.grid.cols);
  for (const zoomed of [false, true]) {
    await p.evaluate(zoomed => probe.handle({ type: 'tabs', tabs: [{ tab_id: 1, pane_id: 1, title: 'split', zoomed,
      panes: [{ pane_id: 1 }, { pane_id: 3 }] }] }), zoomed);
    await p.evaluate(() => { messages.length = 0; });
    if (entry === 'keyboard') {
      await p.locator('.xterm-helper-textarea').focus();
      await p.keyboard.press('Meta+Shift+Enter');
    } else {
      await p.locator('.xterm-screen').click({ button: 'right', position: { x: 20, y: 10 } });
      await p.locator('#menu button').getByText(zoomed ? 'Unzoom Pane' : 'Zoom Pane', { exact: true }).click();
    }
    assert.equal(await p.evaluate(() => messages.filter(m => m.type === 'zoom_pane').length), 1);
    await p.evaluate(() => { messages.length = 0; });
    // Kaku reports the zoom, then the pane's frame. Zoomed, the pane is the
    // whole tab and the page claims it; unzoomed, the tab is split again and
    // keeps the Mac's layout. Half as wide and as tall as before, the pane
    // already fills the page's height, so the text keeps its size.
    await p.evaluate(zoomed => probe.handle({ type: 'tabs', tabs: [{ tab_id: 1, pane_id: 1, title: 'split', zoomed: !zoomed,
      panes: [{ pane_id: 1 }, { pane_id: 3 }] }] }), zoomed);
    if (zoomed) {
      assert.deepEqual(await frame(Math.floor(initial.grid.cols / 2)), []);
      assert.equal(await p.evaluate(() => probe.term.options.fontSize), initial.font);
    } else {
      assert.deepEqual(await frame(initial.grid.cols * 2), [initial.grid]);
    }
    await frame(initial.grid.cols);
    assert.equal(await p.evaluate(() => probe.term.options.fontSize), initial.font);
  }
  assert.deepEqual(errors, []);
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

// A background tab can lose its GPU context. The DOM renderer must take
// over at once (the addon itself waits about 3 s, showing a blank or broken
// grid), and WebGL must come back once the page is in front again.
test('a lost WebGL context hands the grid to the DOM at once and comes back', async t => {
  const gpu = await chromium.launch(launchOptions());
  t.after(() => gpu.close());
  const { p, errors } = await page(t, '#t=fixture', {}, gpu);
  const hasWebgl = await p.evaluate(() => !!document.createElement('canvas').getContext('webgl2'));
  if (!hasWebgl) return t.skip('this Chromium has no WebGL2');
  await p.evaluate(() => {
    probe.handle({ type: 'tabs', tabs: [{ tab_id: 1, pane_id: 1, panes: [], title: 'one', active: true }] });
    probe.handle({ type: 'frame', seq: 1, pane_id: 1, cols: 80, rows: 24, data: '\x1b[2J\x1b[1;1HLOST-CONTEXT' });
  });
  await p.waitForFunction(() => probe.term.buffer.active.getLine(0).translateToString(true) === 'LOST-CONTEXT');
  const webglCanvas = () => p.evaluate(() => [...document.querySelectorAll('.xterm-screen canvas')].some(c => c.getContext('webgl2')));
  assert.equal(await webglCanvas(), true);
  await p.evaluate(() => {
    const c = [...document.querySelectorAll('.xterm-screen canvas')].find(c => c.getContext('webgl2'));
    c.getContext('webgl2').getExtension('WEBGL_lose_context').loseContext();
  });
  await p.waitForTimeout(300);
  // Read now, without waiting: the addon alone hands over only after ~3 s.
  assert.match(await p.evaluate(() => document.querySelector('.xterm-rows')?.textContent || ''), /LOST-CONTEXT/);
  await p.evaluate(() => document.dispatchEvent(new Event('visibilitychange')));
  await p.waitForTimeout(300);
  assert.equal(await webglCanvas(), true);
  assert.equal(await p.locator('.xterm-rows').count(), 0);
  assert.deepEqual(errors, []);
});

test('the grid draws with WebGL where the browser has it, and with the DOM otherwise', async t => {
  const { p: dom, errors: domErrors } = await page(t);
  await screen(dom, 'DOM-GRID');
  assert.equal(await dom.locator('.xterm-rows').count(), 1);
  assert.deepEqual(domErrors, []);

  const gpu = await chromium.launch(launchOptions());
  t.after(() => gpu.close());
  const { p, errors } = await page(t, '#t=fixture', {}, gpu);
  const hasWebgl = await p.evaluate(() => !!document.createElement('canvas').getContext('webgl2'));
  if (!hasWebgl) return t.skip('this Chromium has no WebGL2');
  await p.evaluate(() => {
    probe.handle({ type: 'tabs', tabs: [{ tab_id: 1, pane_id: 1, panes: [], title: 'one', active: true }] });
    probe.handle({ type: 'frame', seq: 1, pane_id: 1, cols: 80, rows: 24, data: '\x1b[2J\x1b[1;1HGPU-GRID' });
  });
  await p.waitForFunction(() => probe.term.buffer.active.getLine(0).translateToString(true) === 'GPU-GRID');
  // The WebGL renderer replaces the DOM rows.
  assert.equal(await p.locator('.xterm-rows').count(), 0);
  // And it really draws the text: a canvas with only the background passes
  // every buffer check above.
  const shot = (await p.locator('.xterm-screen').screenshot()).toString('base64');
  const lit = await p.evaluate(async png => {
    const img = await createImageBitmap(await (await fetch('data:image/png;base64,' + png)).blob());
    const c = new OffscreenCanvas(img.width, img.height).getContext('2d');
    c.drawImage(img, 0, 0);
    const d = c.getImageData(0, 0, img.width, img.height).data;
    let n = 0;
    for (let i = 0; i < d.length; i += 4) if (d[i] + d[i + 1] + d[i + 2] > 450) n++;
    return n;
  }, shot);
  assert.ok(lit > 50, `WebGL drew ${lit} text pixels`);
  // History still opens over the grid in the terminal's font.
  const past = await p.evaluate(() => {
    probe.openPast();
    probe.onHistory({ request: messages.filter(m => m.type === 'history').at(-1)?.request, pane_id: 1, first: 0, top: 0,
      rows: ['\x1b[0mhistory-row'] });
    const el = document.querySelector('#past');
    return { shown: el.classList.contains('show'), text: el.textContent, font: el.style.fontFamily, size: el.style.fontSize };
  });
  assert.ok(past.shown);
  assert.match(past.text, /history-row/);
  assert.match(past.font, /Kaku Mono/);
  assert.equal(past.size, (await p.evaluate(() => probe.term.options.fontSize)) + 'px');
  assert.deepEqual(errors, []);
});

test('the cursor stays steady while frames arrive and blinks once the screen is still', async t => {
  const { p } = await page(t);
  await p.evaluate(() => probe.handle({ type: 'tabs', tabs: [{ tab_id: 1, pane_id: 1, panes: [], title: 'one', active: true }] }));
  const frame = (seq, data) => p.evaluate(({ seq, data }) =>
    probe.handle({ type: 'frame', seq, pane_id: 1, cols: 80, rows: 24, data }), { seq, data });
  const blink = () => p.evaluate(() => probe.term.options.cursorBlink);
  const style = () => p.evaluate(() => probe.term.options.cursorStyle);
  await frame(1, '\x1b[1;1Hone\x1b[1;4H\x1b[5 q');
  await p.waitForTimeout(50);
  assert.equal(await blink(), false);
  assert.equal(await style(), 'bar');
  // Frames without a cursor escape keep it steady too.
  for (let seq = 2; seq < 8; seq++) {
    await frame(seq, '\x1b[1;1Hbusy ' + seq);
    await p.waitForTimeout(150);
    assert.equal(await blink(), false);
  }
  await p.waitForFunction(() => probe.term.options.cursorBlink === true, null, { timeout: 2000 });
  assert.equal(await style(), 'bar');
  await frame(9, '\x1b[1;1Hagain');
  await p.waitForTimeout(50);
  assert.equal(await blink(), false);
  // A steady cursor the app asked for never starts blinking.
  await frame(10, '\x1b[2 q');
  await p.waitForTimeout(1000);
  assert.equal(await blink(), false);
  assert.equal(await style(), 'block');
});

test('offline stops retrying and coming back online reconnects at once', async t => {
  const { p } = await page(t);
  await p.evaluate(() => probe.handle({ type: 'tabs', tabs: [{ tab_id: 1, pane_id: 1, panes: [], title: 'one', active: true }] }));
  await p.context().setOffline(true);
  const before = await p.evaluate(() => { connections.at(-1).close(1006); return connections.length; });
  await p.waitForTimeout(2500);
  assert.equal(await p.evaluate(() => connections.length), before);
  assert.match(await p.locator('#notice').innerText(), /No connection/);
  await p.context().setOffline(false);
  await p.waitForFunction(n => connections.length === n + 1, before, { timeout: 500 });
});

test('the agent working in a tab shows its mark and still topic', async t => {
  const { p, errors } = await page(t);
  await screen(p);
  const agent = p.locator('#bar .agent');
  assert.equal(await agent.isVisible(), false, 'no agent, no title');
  await p.evaluate(() => probe.handle({ type: 'tabs', tabs: [
    { tab_id: 1, pane_id: 1, panes: [], title: 'one', active: true, agent: { program: 'claude', topic: 'Fix the padding bug' } },
    { tab_id: 2, pane_id: 2, panes: [], title: 'two', agent: { program: 'codex', topic: 'Other tab' } },
  ] }));
  assert.equal(await agent.isVisible(), true);
  assert.equal(await agent.innerText(), 'Fix the padding bug');
  assert.ok((await agent.locator('path').getAttribute('d')).startsWith('M'), 'the Claude mark is drawn');
  // The spinner in the pane title never reaches the browser tab.
  await p.evaluate(() => probe.handle({ type: 'frame', seq: 2, pane_id: 1, cols: 80, rows: 24, title: '✳ Fix the padding bug', data: '' }));
  assert.equal(await p.title(), 'Fix the padding bug - Kaku');
  await p.setViewportSize({ width: 375, height: 600 });
  assert.equal(await agent.isVisible(), false, 'phones keep the bar for tabs');
  assert.deepEqual(errors, []);
});

test('the Mac covering a pane with AI chat dims it and says so', async t => {
  const { p, errors } = await page(t);
  await screen(p);
  const tabs = chat => ({ type: 'tabs', tabs: [
    { tab_id: 1, pane_id: 1, panes: [], title: 'one', active: true, ...(chat ? { ai_chat: [1] } : {}) },
    { tab_id: 2, pane_id: 2, panes: [], title: 'two' },
  ] });
  await p.evaluate(m => probe.handle(m), tabs(true));
  assert.match(await p.locator('#notice').innerText(), /AI chat is open on the Mac/);
  assert.equal(await p.evaluate(() => document.body.classList.contains('stale')), true);
  await p.evaluate(m => probe.handle(m), tabs(false));
  assert.equal(await p.locator('#notice').isVisible(), false, 'closing the chat clears the notice');
  assert.equal(await p.evaluate(() => document.body.classList.contains('stale')), false);
  assert.deepEqual(errors, []);
});

test('a page open across an update reloads into the new one on reconnect, once', async t => {
  const { p, errors } = await page(t);
  await screen(p);
  const reconnect = async () => {
    const before = await p.evaluate(() => connections.length);
    await p.evaluate(() => connections.at(-1).close());
    await p.waitForFunction(n => connections.length > n, before);
    await p.evaluate(() => connections.at(-1).onmessage({ data: '{"type":"pong"}' }));
    await p.waitForTimeout(300);
  };
  let loads = 0;
  p.on('load', () => loads++);
  // The first message only counts as a reconnect after an earlier session.
  await p.evaluate(() => connections.at(-1).onmessage({ data: '{"type":"pong"}' }));
  await reconnect();
  assert.equal(loads, 0, 'the same script never reloads');

  // The address now serves a new script, but a stale cache keeps handing
  // the reload the old page: it reloads once and then stays put.
  const served = fs.readFileSync(path.join(root, 'crates/kaku-remote/assets/web/index.html'), 'utf8')
    .replace('<script>', '<script>window.newPage = true;');
  await p.route('http://localhost:9988/', route => route.request().resourceType() === 'fetch'
    ? route.fulfill({ contentType: 'text/html', body: served })
    : route.fallback());
  await reconnect();
  await p.waitForFunction(() => window.probe);
  assert.equal(loads, 1, 'a changed script reloads the page');
  await p.evaluate(() => connections.at(-1).onmessage({ data: '{"type":"pong"}' }));
  await reconnect();
  assert.equal(loads, 1, 'the same new version never reloads twice');
  assert.deepEqual(errors, []);
});

test('no size is claimed before Kaku\'s config and font are in, then one', async t => {
  const { p } = await page(t, '#t=fixture', { noConfig: true });
  await screen(p);
  await p.setViewportSize({ width: 900, height: 560 });
  await p.waitForTimeout(250);
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'resize')), []);
  await p.evaluate(() => { probe.handle({ type: 'config', font_size: 14 }); return document.fonts.load('16px "Kaku Mono"'); });
  await p.waitForTimeout(250);
  assert.equal(await p.evaluate(() => messages.filter(m => m.type === 'resize').length), 1);
});

test('the drift check names the last frame drawn, so it runs while output streams', async t => {
  const { p } = await page(t);
  await screen(p);
  await p.evaluate(() => {
    for (let seq = 2; seq <= 3; seq++) probe.handle({ type: 'frame', seq, pane_id: 1, cols: 80, rows: 24, data: '\x1b[2;1Hline ' + seq });
    probe.handle({ type: 'frame', seq: 4, pane_id: 1, cols: 90, rows: 24, data: '\x1b[2J\x1b[1;1Hresized' });
  });
  await p.waitForTimeout(1400);
  const checks = await p.evaluate(() => messages.filter(m => m.type === 'verify'));
  assert.ok(checks.length >= 1, 'a resize is checked within a second or so');
  const last = checks.at(-1);
  assert.equal(last.seq, 4);
  assert.deepEqual([last.view.cols, last.view.rows], [90, 24]);
  assert.equal(last.rows[0].trimEnd(), 'resized');
  // How the page draws goes along for the Mac's log.
  assert.ok(last.view.font > 0 && last.view.cell_w > 0 && last.view.cell_h > 0 && last.view.icon > 0, JSON.stringify(last.view));
  assert.equal(last.view.past, false);
  const resets = last.view.atlas;
  await p.evaluate(() => window.dispatchEvent(new Event('focus')));
  await p.evaluate(() => probe.handle({ type: 'frame', seq: 5, pane_id: 1, cols: 90, rows: 24, data: '\x1b[2;1Hmore' }));
  await p.waitForTimeout(1400);
  const after = (await p.evaluate(() => messages.filter(m => m.type === 'verify'))).at(-1);
  assert.equal(after.view.atlas, after.view.gl ? resets + 1 : resets);
});

test('the page draws with Kaku\'s font families in their order', async t => {
  const { p } = await page(t);
  assert.equal(await p.evaluate(() => probe.term.options.fontFamily),
    '"Kaku Mono", "Kaku Symbols", "PingFang SC", "Apple Symbols", "Microsoft YaHei", "Segoe UI Symbol", monospace');
  await p.evaluate(() => probe.handle({ type: 'config', font_families: ['Maple Mono NF', 'JetBrains Mono', 'PingFang SC', 'Apple Color Emoji'] }));
  assert.equal(await p.evaluate(() => probe.term.options.fontFamily),
    '"Maple Mono NF", "Kaku Mono", "Kaku Symbols", "PingFang SC", "Apple Symbols", "Microsoft YaHei", "Segoe UI Symbol", monospace');
  // A Mac on an older Kaku names only its first family.
  await p.evaluate(() => probe.handle({ type: 'config', font_family: 'Iosevka' }));
  assert.ok((await p.evaluate(() => probe.term.options.fontFamily)).startsWith('"Iosevka", "Kaku Mono", '));
});

test('Kaku\'s bundled icons draw on the page', async t => {
  const { p } = await page(t);
  const drawn = await p.evaluate(async () => {
    const faces = await document.fonts.load('16px "Kaku Symbols"', '');
    const ctx = document.createElement('canvas').getContext('2d');
    const width = (font) => { ctx.font = font; return ctx.measureText('').width; };
    return { faces: faces.length, stack: width('16px ' + probe.term.options.fontFamily), icons: width('16px "Kaku Symbols"') };
  });
  assert.equal(drawn.faces, 1);
  assert.equal(drawn.stack, drawn.icons, 'the stack draws icons from the bundled symbol font');
});

test('a font only the Mac has is fetched once, then kept by this browser', async t => {
  const context = await browser.newContext();
  t.after(() => context.close());
  const font = fs.readFileSync(path.join(root, 'crates/kaku-remote/assets/web/font-regular.woff2'));
  const file = { family: 'Kaku Test Mono', path: '/Users/me/Library/Fonts/Test Mono.woff2', size: font.length, modified: 7 };
  const serve = async (p) => {
    const asked = await p.evaluate(() => messages.filter(m => m.type === 'download'));
    for (const { id } of asked) {
      await p.evaluate(({ id, size, data }) => {
        probe.handle({ type: 'file_start', id, name: 'Test Mono.woff2', size });
        for (let i = 0; i < data.length; i += 65536) probe.handle({ type: 'file_chunk', id, data: data.slice(i, i + 65536) });
        probe.handle({ type: 'file_end', id });
      }, { id, size: font.length, data: font.toString('base64') });
    }
    return asked;
  };
  const loaded = (p) => p.evaluate(() => [...document.fonts].some(f => f.family.replace(/"/g, '') === 'Kaku Test Mono' && f.status === 'loaded'));
  const first = await page(t, '#t=fixture', {}, context);
  await screen(first.p);
  await first.p.evaluate(file => probe.handle({ type: 'config', font_families: [file.family], font_files: [file] }), file);
  await first.p.waitForTimeout(200);
  const asked = await serve(first.p);
  assert.deepEqual(asked.map(m => m.path), [file.path]);
  await first.p.waitForTimeout(300);
  assert.ok(await loaded(first.p), 'the fetched file draws the terminal');
  assert.equal(await first.p.evaluate(() => document.querySelector('#toast').classList.contains('show')), false, 'no download toast for a font');
  assert.deepEqual(first.errors, []);

  const again = await page(t, '#t=fixture', {}, context);
  await screen(again.p);
  await again.p.evaluate(file => probe.handle({ type: 'config', font_families: [file.family], font_files: [file] }), file);
  await again.p.waitForTimeout(400);
  assert.deepEqual(await again.p.evaluate(() => messages.filter(m => m.type === 'download')), [], 'kept from the first visit');
  assert.ok(await loaded(again.p));
});

test('a font this browser already has is not fetched', async t => {
  const { p } = await page(t);
  await p.evaluate(() => probe.handle({ type: 'config', font_families: ['Menlo'], font_files: [{ family: 'Menlo', path: '/Library/Fonts/Menlo.ttf', size: 100, modified: 1 }] }));
  await p.waitForTimeout(300);
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'download')), []);
});

// The icon font loads only once an icon is drawn, after WebGL already
// rasterized a box for it; the grid must redraw with the icon by itself.
test('an icon drawn before its font loaded is redrawn once the font is in', async t => {
  const gpu = await chromium.launch(launchOptions());
  t.after(() => gpu.close());
  const { p, errors } = await page(t, '#t=fixture', {}, gpu);
  if (!await p.evaluate(() => !!document.createElement('canvas').getContext('webgl2'))) return t.skip('this Chromium has no WebGL2');
  await p.evaluate(() => {
    probe.handle({ type: 'tabs', tabs: [{ tab_id: 1, pane_id: 1, panes: [], title: 'one', active: true }] });
    probe.handle({ type: 'frame', seq: 1, pane_id: 1, cols: 80, rows: 24, data: '\x1b[2J\x1b[1;1H dir' });
  });
  await p.waitForFunction(() => [...document.fonts].some(f => f.family.includes('Kaku Symbols') && f.status === 'loaded'));
  await p.waitForTimeout(300);
  const box = await p.locator('.xterm-screen').boundingBox();
  const clip = { x: box.x, y: box.y, width: 80, height: 24 };
  const drawn = await p.screenshot({ clip });
  // What the cell looks like once the atlas is rebuilt with the font in.
  await p.evaluate(() => { probe.term.clearTextureAtlas(); probe.term.refresh(0, probe.term.rows - 1); });
  await p.waitForTimeout(300);
  assert.ok(drawn.equals(await p.screenshot({ clip })), 'the icon, not the box drawn before its font arrived');
  assert.deepEqual(errors, []);
});

test('a tab split on the Mac keeps the Mac\'s sizes, its split shown as large as fits', async t => {
  const { p, errors } = await page(t);
  await screen(p);
  await p.waitForTimeout(200);
  const grid = await p.evaluate(() => messages.filter(m => m.type === 'resize').at(-1));
  const font = await p.evaluate(() => probe.term.options.fontSize);
  const tabs = (panes) => p.evaluate(panes => probe.handle({ type: 'tabs', tabs: [{ tab_id: 1, pane_id: 1, title: 'one', active: true,
    panes: panes > 1 ? Array.from({ length: panes }, (_, i) => ({ pane_id: i + 1 })) : undefined }] }), panes);
  const frame = (cols, rows) => p.evaluate(async ({ cols, rows }) => {
    probe.handle({ type: 'frame', pane_id: 1, cols, rows, data: '\x1b[2J\x1b[1;1Hpane' });
    await new Promise(resolve => probe.term.write('', resolve));
    const stage = document.querySelector('#stage') || document.querySelector('.xterm').parentElement;
    const screen = document.querySelector('.xterm-screen').getBoundingClientRect();
    const room = stage.getBoundingClientRect();
    return { font: probe.term.options.fontSize, fits: screen.width <= room.width + 1 && screen.height <= room.height + 1,
      claims: messages.filter(m => m.type === 'resize') };
  }, { cols, rows });
  await p.evaluate(() => { messages.length = 0; });
  await tabs(2);
  const half = await frame(Math.floor(grid.cols / 2), Math.floor(grid.rows / 2));
  assert.deepEqual(half.claims, [], 'the Mac keeps its layout');
  assert.ok(half.font > font * 1.5 && half.fits, 'a half-size split fills the page');
  await tabs(4);
  const quarter = await frame(Math.floor(grid.cols / 4), Math.floor(grid.rows / 4));
  assert.deepEqual(quarter.claims, []);
  assert.ok(quarter.font <= font * 2 && quarter.fits, 'at most twice Kaku\'s size');
  await tabs(1);
  const back = await frame(grid.cols, grid.rows);
  assert.deepEqual(back.claims.map(({ cols, rows }) => ({ cols, rows })), [{ cols: grid.cols, rows: grid.rows }], 'one pane again: the page sizes the tab');
  assert.equal(back.font, font);
  assert.deepEqual(errors, []);
});

test('the desktop view is offered only when Kaku has its helper', async t => {
  const { p, errors } = await page(t);
  await screen(p);
  assert.equal(await p.locator('#bar .tool.desktop').count(), 0);
  assert.equal(await p.evaluate(() => !!window.KakuDesktop), false, 'its code is not even loaded');
  await p.evaluate(() => probe.handle({ type: 'config', desktop: true }));
  await p.locator('#bar .tool.desktop').waitFor();
  assert.deepEqual(errors, []);
});

test('the desktop view takes the keyboard and mouse, and gives them back', async t => {
  const { p, errors } = await page(t);
  await screen(p);
  await p.evaluate(() => probe.handle({ type: 'config', desktop: true }));
  const tool = p.locator('#bar .tool.desktop');
  await tool.click();
  assert.equal(await p.evaluate(() => document.body.classList.contains('desktop')), true);
  const start = await p.evaluate(() => messages.filter(m => m.type === 'desktop_start'));
  assert.equal(start.length, 1);
  assert.ok(start[0].max_width >= 640 && start[0].max_height >= 400, JSON.stringify(start[0]));
  assert.match(await p.locator('#desktop .status .title').innerText(), /Connecting/);

  // Until a picture is drawn, keys are held back from both the terminal
  // and the Mac.
  await p.evaluate(() => { messages.length = 0; });
  await p.keyboard.press('a');
  assert.deepEqual(await p.evaluate(() => messages.filter(m => ['text', 'key', 'desktop_key'].includes(m.type))), []);

  // A helper error reads as one.
  await p.evaluate(() => probe.handle({ type: 'desktop_status', state: 'error', detail: 'Kaku has no Screen Recording permission on this Mac' }));
  assert.match(await p.locator('#desktop .status').innerText(), /Screen Recording/);

  // A reconnect asks again.
  await p.evaluate(() => { messages.length = 0; probe.handle({ type: 'config', desktop: true }); });
  assert.equal(await p.evaluate(() => messages.filter(m => m.type === 'desktop_start').length), 1);

  // Back to the terminal: the helper stops and typing goes to the pane.
  await p.evaluate(() => { messages.length = 0; });
  await tool.click();
  assert.equal(await p.evaluate(() => document.body.classList.contains('desktop')), false);
  assert.equal(await p.evaluate(() => messages.filter(m => m.type === 'desktop_stop').length), 1);
  await p.keyboard.type('b');
  assert.ok(await p.evaluate(() => messages.some(m => m.type === 'text' && m.data === 'b')));
  assert.deepEqual(errors, []);
});

test('picking a tab leaves the desktop view for the terminal', async t => {
  const { p } = await page(t);
  await screen(p);
  await p.evaluate(() => probe.handle({ type: 'config', desktop: true }));
  await p.locator('#bar .tool.desktop').click();
  const looks = await p.evaluate(() => [...document.querySelectorAll('#tabs .tab')].map(b => getComputedStyle(b).backgroundColor));
  assert.equal(new Set(looks).size, 1, 'no tab reads as current over the screen: ' + looks);
  assert.notEqual(await p.locator('#bar .tool.desktop').evaluate(b => getComputedStyle(b).backgroundColor), looks[0]);
  await p.locator('#tabs .tab').first().click();
  assert.equal(await p.evaluate(() => document.body.classList.contains('desktop')), false);
  assert.ok(await p.evaluate(() => messages.some(m => m.type === 'desktop_stop')));
});

test('on a phone the desktop view pinches to zoom and taps where the zoom shows', async t => {
  const { p, errors } = await page(t, '#t=fixture', { viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });
  await screen(p);
  await p.evaluate(() => probe.handle({ type: 'config', desktop: true }));
  await p.locator('#bar .tool.desktop').click();
  const start = await p.evaluate(() => messages.find(m => m.type === 'desktop_start'));
  assert.ok(start.max_width >= 1920, 'enough detail to zoom into: ' + JSON.stringify(start));
  assert.equal(await p.locator('#keys').isVisible(), false, 'the terminal keys would type into the terminal');

  // Stand in for a decoded picture: the canvas is live at its own size.
  const box = await p.evaluate(() => {
    document.getElementById('desktop').classList.add('live');
    const r = document.querySelector('#desktop canvas').getBoundingClientRect();
    return { x: r.left, y: r.top, w: r.width, h: r.height };
  });
  const touch = (type, id, x, y) => p.evaluate(([type, id, x, y]) => {
    document.querySelector('#desktop canvas').dispatchEvent(new PointerEvent(type, { pointerId: id, pointerType: 'touch', clientX: x, clientY: y, bubbles: true, cancelable: true, isPrimary: id === 1 }));
  }, [type, id, x, y]);
  const cx = box.x + box.w / 2;
  const cy = box.y + box.h / 2;
  await p.evaluate(() => { messages.length = 0; });

  // Fingers spreading apart zoom in, around their midpoint; nothing scrolls.
  await touch('pointerdown', 1, cx - 20, cy);
  await touch('pointerdown', 2, cx + 20, cy);
  for (let d = 30; d <= 60; d += 10) {
    await touch('pointermove', 1, cx - d, cy);
    await touch('pointermove', 2, cx + d, cy);
  }
  await touch('pointerup', 1, cx - 60, cy);
  await touch('pointerup', 2, cx + 60, cy);
  const zoomed = await p.evaluate(() => {
    const r = document.querySelector('#desktop canvas').getBoundingClientRect();
    return { w: r.width, left: r.left };
  });
  assert.ok(Math.abs(zoomed.w / box.w - 3) < 0.05, 'three times the spread, three times the size: ' + zoomed.w);
  assert.ok(Math.abs(zoomed.left + zoomed.w / 2 - cx) < 2, 'the midpoint stayed put');
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type.startsWith('desktop_'))), []);

  // Zoomed in, two fingers move the view rather than scroll the Mac.
  await touch('pointerdown', 1, cx - 10, cy);
  await touch('pointerdown', 2, cx + 10, cy);
  await touch('pointermove', 1, cx - 10 + 40, cy);
  await touch('pointermove', 2, cx + 10 + 40, cy);
  await touch('pointerup', 1, cx + 30, cy);
  await touch('pointerup', 2, cx + 50, cy);
  const moved = await p.evaluate(() => document.querySelector('#desktop canvas').getBoundingClientRect().left);
  assert.ok(Math.abs(moved - zoomed.left - 40) < 2, 'the view followed the fingers: ' + (moved - zoomed.left));
  assert.deepEqual(await p.evaluate(() => messages.filter(m => m.type === 'desktop_wheel')), []);

  // A tap lands on the Mac where the zoomed picture shows it.
  await touch('pointerdown', 1, cx, cy);
  await touch('pointerup', 1, cx, cy);
  const click = await p.evaluate(() => messages.find(m => m.type === 'desktop_pointer' && m.kind === 'down'));
  const expectX = (cx - moved) / zoomed.w;
  assert.ok(Math.abs(click.x - expectX) < 0.01 && Math.abs(click.y - 0.5) < 0.01, JSON.stringify(click) + ' vs ' + expectX);
  assert.deepEqual(errors, []);
});
