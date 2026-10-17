// Opt-in smoke against an isolated built app and a loopback relay.
// Never reads an installed Kaku's configuration, link, or terminal content.
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const crypto = require('node:crypto');
const net = require('node:net');
const { spawn } = require('node:child_process');
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
  throw new Error('isolated smoke timed out');
}
(async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'kq-'));
  const children = [], sockets = [];
  let browser;
  const start = (file, args, env) => {
    const p = spawn(file, args, { env: { ...process.env, ...env }, stdio: 'ignore' });
    children.push(p); return p;
  };
  try {
    const local = await port(), relay = await port();
    fs.mkdirSync(path.join(home, '.config/kaku'), { recursive: true });
    fs.mkdirSync(path.join(home, 'tmp'));
    const config = path.join(home, '.config/kaku/kaku.lua');
    fs.writeFileSync(config, `return {default_prog={'/bin/zsh','-f'}, default_cwd=${JSON.stringify(home)}, restore_previous_session=false, pane_close_confirmation="AlwaysPrompt", tab_close_confirmation="AlwaysPrompt", initial_cols=80, initial_rows=24, font_size=16, remote={enabled=true,bind='127.0.0.1',port=${local},tunnel=true,tunnel_url='ws://127.0.0.1:${relay}'}}`);
    start(path.join(root, 'crates/kaku-relay/target/debug/kaku-relay'), [], { PORT: String(relay), KAKU_RELAY_ACCESS_CODE: 'live-code' });
    await until(async () => { try { return (await fetch(`http://127.0.0.1:${relay}/health`)).ok; } catch { return false; } });
    const app = process.env.KAKU_TEST_APP || path.join(root, 'dist/Kaku.app');
    start(path.join(app, 'Contents/MacOS/kaku-gui'), ['--config-file', config, 'start', '--always-new-process'], {
      HOME: home, XDG_CONFIG_HOME: path.join(home, '.config'), TMPDIR: path.join(home, 'tmp/')
    });
    const statePath = path.join(home, 'tmp/kaku-remote.json');
    await until(() => fs.existsSync(statePath));
    const state = JSON.parse(fs.readFileSync(statePath));
    const url = `http://127.0.0.1:${local}/#t=${state.token}`;
    // This integration smoke reads DOM rows and link geometry. The browser
    // unit suite separately verifies WebGL rendering and context recovery.
    browser = await chromium.launch({
      args: ['--disable-webgl', '--disable-3d-apis'],
      ...(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE
        ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE } : {}),
    });
    const page = await browser.newPage({ viewport: { width: 1100, height: 650 } });
    const errors = []; page.on('pageerror', e => errors.push(e.message));
    await page.goto(url);
    await page.waitForSelector('body.drawn');
    await page.locator('.xterm-helper-textarea').focus();
    await page.keyboard.type('printf "LIVE-OK\\n"\n');
    await page.waitForFunction(() => [...document.querySelectorAll('.xterm-rows > div')].some(row => row.textContent.trim() === 'LIVE-OK'));

    // An inline image (iTerm2 protocol, as imgcat writes) reaches the page as
    // an image over its cells, not as dropped cells.
    const png = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==';
    await page.keyboard.type(`printf '\\033]1337;File=inline=1;width=4;height=2:${png}\\a\\n'\n`);
    await page.waitForSelector('#images img', { timeout: 15000 });

    // A page on another site that rebinds its name to 127.0.0.1 still sends
    // its own Host, and is refused; the old token page is gone.
    const statusFor = host => new Promise((resolve, reject) => require('node:http')
      .get({ host: '127.0.0.1', port: local, path: '/', headers: { Host: host } }, res => { res.resume(); resolve(res.statusCode); })
      .on('error', reject));
    assert.equal(await statusFor(`evil.example:${local}`), 403);
    assert.equal(await statusFor(`127.0.0.1:${local}`), 200);
    assert.equal((await fetch(`http://127.0.0.1:${local}/link`)).status, 404);

    // Raw connection measures server flow control without browser mocks.
    const ws = new WebSocket(`ws://127.0.0.1:${local}/web/ws?token=${state.token}`);
    sockets.push(ws);
    const received = []; ws.on('message', bytes => received.push(JSON.parse(bytes)));
    await new Promise((resolve, reject) => { ws.once('open', resolve); ws.once('error', reject); });
    const send = m => ws.send(JSON.stringify(m));
    send({ type: 'hello', frame_ack: true });
    const tabs = await until(() => received.find(m => m.type === 'tabs'));
    const pane = tabs.tabs[0].pane_id;
    send({ type: 'attach', pane_id: pane });
    send({ type: 'split_pane', pane_id: pane, direction: 'Right' });
    const two = await until(() => received.filter(m => m.type === 'tabs').at(-1)?.tabs.find(t => t.panes?.length === 2));
    const right = two.panes.find(p => p.pane_id !== pane).pane_id;
    send({ type: 'split_pane', pane_id: pane, direction: 'Down' });
    send({ type: 'attach', pane_id: right, focus: true });
    const three = await until(() => received.filter(m => m.type === 'tabs').at(-1)?.tabs.find(t => t.panes?.length === 3));
    const originalBox = three.panes.find(p => p.pane_id === pane);
    const lower = three.panes.find(p => p.pane_id !== pane && p.pane_id !== right);
    assert.equal(lower.left, originalBox.left, 'queued split keeps the original pane despite a following focus');
    assert.ok(lower.top > originalBox.top);
    // Already confirmed browser actions must not open a second Mac overlay,
    // even when the native policy is AlwaysPrompt.
    send({ type: 'close_pane', pane_id: lower.pane_id });
    await until(() => received.filter(m => m.type === 'tabs').at(-1)?.tabs.find(t => t.panes?.length === 2));
    send({ type: 'close_pane', pane_id: right });
    await until(() => received.filter(m => m.type === 'tabs').at(-1)?.tabs.some(t => t.pane_id === pane && !t.panes?.length));
    send({ type: 'attach', pane_id: pane });
    // Reset the measurement window after the pane-operation frames.
    send({ type: 'hello', frame_ack: true });
    const prior = received.filter(m => m.type === 'frame').at(-1);
    const flowStart = received.filter(m => m.type === 'frame').length;
    if (prior) send({ type: 'frame_ack', seq: prior.seq });
    await sleep(200);

    send({ type: 'text', data: 'for i in {1..30}; do echo FLOW-$i; sleep 0.03; done\n' });
    await sleep(1500);
    const frames = received.filter(m => m.type === 'frame').slice(flowStart);
    assert.equal(frames.length, 4, 'slow consumers receive at most four unacknowledged frames');
    send({ type: 'frame_ack', seq: 1000000 }); await sleep(100);
    assert.equal(received.filter(m => m.type === 'frame').length - flowStart, 4, 'future ACK cannot release the window');
    send({ type: 'frame_ack', seq: frames.at(-1).seq });
    const latest = await until(() => received.filter(m => m.type === 'frame').at(-1).seq > frames.at(-1).seq);
    assert.ok(latest);
    assert.ok(received.filter(m => m.type === 'frame').slice(flowStart + 4).some(m => m.data.includes('FLOW-30')), 'ACK releases the latest coalesced screen');
    // Pages already open before the upgrade still use the old hello.
    send({ type: 'hello' });
    const beforeLegacy = received.filter(m => m.type === 'frame').length;
    send({ type: 'text', data: 'for i in {1..12}; do echo LEGACY-$i; sleep 0.04; done\n' });
    await sleep(800);
    assert.ok(received.filter(m => m.type === 'frame').length - beforeLegacy > 4);
    ws.close();

    // Real no-echo input must never show, even before the server responds.
    await page.locator('.xterm-helper-textarea').focus();
    await page.keyboard.type('read -s hidden\n'); await sleep(250);
    await page.keyboard.type('fixture-secret'); await sleep(50);
    assert.ok(!(await page.locator('.xterm-rows').innerText()).includes('fixture-secret'));
    await page.keyboard.press('Enter');
    await sleep(250);

    // Through the loopback relay, an incorrect but well-formed key is final.
    const identityPath = path.join(home, 'Library/Application Support/kaku/remote-share.json');
    await until(() => fs.existsSync(identityPath));
    const identity = JSON.parse(fs.readFileSync(identityPath));
    const id = crypto.createHash('sha256').update(identity.secret).digest('hex').slice(0, 32);
    const relayBase = `http://127.0.0.1:${relay}/${id}/`;
    const legacyBase = `http://127.0.0.1:${relay}/t/${id}/`;
    // This relay asks for an access code: without one the Mac is turned
    // away and says so; once `kaku remote` saves it, Kaku tries again within
    // a second or two instead of waiting out its backoff.
    const statusPath = path.join(home, 'tmp/kaku-remote-relay.json');
    await until(() => fs.existsSync(statusPath) && JSON.parse(fs.readFileSync(statusPath)).last_error.includes('access code required'));
    assert.equal((await fetch(relayBase)).status, 404);
    fs.writeFileSync(path.join(home, 'Library/Application Support/kaku/remote-access'), 'live-code\n', { mode: 0o600 });
    const saved = Date.now();
    await until(async () => (await fetch(relayBase)).ok, 8000);
    console.log(`access code accepted after ${Date.now() - saved} ms`);
    await until(async () => (await fetch(legacyBase)).ok);
    await until(async () => (await fetch(relayBase)).ok);
    const cli = (...args) => new Promise((resolve, reject) => {
      const child = spawn(path.join(app, 'Contents/MacOS/kaku'), args, {
        env: { ...process.env, HOME: home, XDG_CONFIG_HOME: path.join(home, '.config'), TMPDIR: path.join(home, 'tmp/') },
      });
      let out = '';
      child.stdout.on('data', d => { out += d; });
      child.once('error', reject);
      child.once('exit', code => code === 0 ? resolve(out) : reject(new Error(`kaku ${args.join(' ')} exited ${code}: ${out}`)));
    });
    const pairingLink = async () => (await cli('remote')).split('\n').find(line => /#[A-Za-z0-9_-]{22}$/.test(line));
    // Without pairing, the page says how to pair and opens no socket.
    const unpaired = await browser.newPage();
    await unpaired.goto(relayBase);
    assert.match(await unpaired.locator('#stage').innerText(), /Pair this browser first/);
    const encryptedContext = await browser.newContext();
    const encrypted = await encryptedContext.newPage();
    await encrypted.addInitScript(() => {
      const Original = WebSocket;
      window.__testSockets = [];
      window.WebSocket = class extends Original {
        constructor(...args) { super(...args); window.__testSockets.push(this); }
      };
    });
    const link = await pairingLink();
    assert.ok(link.startsWith(relayBase + '#'), link);
    await encrypted.goto(link);
    await encrypted.waitForSelector('body.drawn');
    assert.equal(new URL(encrypted.url()).hash, '');
    await until(async () => (await cli('remote', 'devices')).includes(' on '));
    const deviceId = (await cli('remote', 'devices')).trim().split(/\s+/)[0];
    // The link worked once: another browser opening it is turned away.
    const latecomer = await (await browser.newContext()).newPage();
    await latecomer.goto(link);
    await latecomer.waitForFunction(() => document.querySelector('#notice .title')?.textContent === 'This pairing link has expired');
    const returning = await encrypted.context().newPage();
    await returning.goto(relayBase);
    await returning.waitForSelector('body.drawn');
    await returning.close();
    if (process.env.KAKU_TEST_SCREENSHOTS) {
      fs.mkdirSync(process.env.KAKU_TEST_SCREENSHOTS, { recursive: true });
      await encrypted.setViewportSize({ width: 375, height: 700 });
      await encrypted.screenshot({ path: path.join(process.env.KAKU_TEST_SCREENSHOTS, 'paired-phone.png') });
      await encrypted.setViewportSize({ width: 1280, height: 900 });
    }
    await encrypted.locator('.xterm-helper-textarea').focus();
    await encrypted.keyboard.type('echo ENCRYPTED-OK\n');
    await encrypted.waitForFunction(() => [...document.querySelectorAll('.xterm-rows > div')].some(row => row.textContent.trim() === 'ENCRYPTED-OK'));
    // Cmd+clicking a path in the output brings that file from the Mac,
    // through the encrypted relay, as a browser download.
    const fixture = Buffer.from(Array.from({ length: 1500000 }, (_, i) => (i * 7) % 256));
    fs.writeFileSync(path.join(home, 'dl-fixture.bin'), fixture);
    await encrypted.keyboard.type('echo "FILE ~/dl-fixture.bin"\n');
    await encrypted.waitForFunction(() => [...document.querySelectorAll('.xterm-rows > div')].some(row => row.textContent.trim() === 'FILE ~/dl-fixture.bin'));
    const spot = await encrypted.evaluate(() => {
      const row = [...document.querySelectorAll('.xterm-rows > div')].find(r => r.textContent.trim() === 'FILE ~/dl-fixture.bin');
      const walker = document.createTreeWalker(row, NodeFilter.SHOW_TEXT);
      for (let node; (node = walker.nextNode());) {
        const at = node.textContent.indexOf('fixture');
        if (at < 0) continue;
        const range = document.createRange();
        range.setStart(node, at); range.setEnd(node, at + 1);
        const r = range.getBoundingClientRect();
        return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
      }
      return null;
    });
    const fetched = encrypted.waitForEvent('download', { timeout: 30000 });
    await encrypted.keyboard.down('Meta');
    await encrypted.mouse.move(spot.x, spot.y);
    await encrypted.mouse.click(spot.x, spot.y);
    await encrypted.keyboard.up('Meta');
    const got = await fetched;
    assert.equal(got.suggestedFilename(), 'dl-fixture.bin');
    assert.deepEqual(fs.readFileSync(await got.path()), fixture);
    // Exercise the actual browser uploader through encryption, including a
    // dropped connection after the Mac acknowledges part of the file.
    await encrypted.evaluate(() => {
      window.__testDropped = false;
      const observer = new MutationObserver(() => {
        const match = document.querySelector('#toast').textContent.match(/ (\d+)%$/);
        if (match && Number(match[1]) > 0 && Number(match[1]) < 100 && !window.__testDropped) {
          window.__testDropped = true;
          observer.disconnect();
          window.__testSockets.at(-1).close();
        }
      });
      observer.observe(document.querySelector('#toast'), { childList: true });
      const dt = new DataTransfer();
      dt.items.add(new File([new Uint8Array(8 * 1024 * 1024).fill(93)], 'fixture-upload.bin'));
      document.querySelector('#stage').dispatchEvent(new DragEvent('drop', { dataTransfer: dt, bubbles: true, cancelable: true }));
    });
    await encrypted.waitForFunction(() => document.querySelector('#toast').textContent.startsWith('Sent to'), undefined, { timeout: 45000 });
    assert.equal(await encrypted.evaluate(() => window.__testDropped), true);
    const uploadDir = path.join(home, 'Downloads/Kaku');
    assert.deepEqual(fs.readdirSync(uploadDir), ['fixture-upload.bin']);
    assert.deepEqual(fs.readFileSync(path.join(uploadDir, 'fixture-upload.bin')), Buffer.alloc(8 * 1024 * 1024, 93));
    await encrypted.keyboard.press('Control+C');
    await encrypted.keyboard.type('echo AFTER-UPLOAD\n');
    await encrypted.waitForFunction(() => [...document.querySelectorAll('.xterm-rows > div')].some(row => row.textContent.trim() === 'AFTER-UPLOAD'));
    if (process.env.KAKU_TEST_SCREENSHOTS) {
      fs.mkdirSync(process.env.KAKU_TEST_SCREENSHOTS, { recursive: true });
      await encrypted.screenshot({ path: path.join(process.env.KAKU_TEST_SCREENSHOTS, 'desktop.png') });
      await encrypted.setViewportSize({ width: 375, height: 667 });
      await sleep(300);
      await encrypted.screenshot({ path: path.join(process.env.KAKU_TEST_SCREENSHOTS, 'phone.png') });
    }
    // An unauthenticated idle browser releases its slot without receiving
    // a terminal session. Deliberately send no first message.
    const idle = new WebSocket(`ws://127.0.0.1:${relay}/t/${id}/ws`);
    sockets.push(idle);
    const idleClosed = new Promise(resolve => idle.once('close', code => resolve(code)));
    assert.equal(await Promise.race([idleClosed, sleep(21000).then(() => 'timeout')]), 4408);

    // A raw browser speaking the pairing protocol with node's crypto, the
    // way the page does with WebCrypto: Kaku's signature checks out against
    // the key the paired page took on the link's word, a device Kaku never
    // paired is refused, and nothing recorded or out of place reaches a
    // session.
    const macPub = Buffer.from(await encrypted.evaluate(() => new Promise(resolve => {
      const open = indexedDB.open('kaku', 1);
      open.onsuccess = () => {
        const req = open.result.transaction('devices').objectStore('devices').get(location.pathname);
        req.onsuccess = () => resolve(Array.from(req.result.macPub));
      };
    })));
    assert.equal(macPub.length, 65);
    const rawPub = key => { const jwk = key.export({ format: 'jwk' }); return Buffer.concat([Buffer.from([4]), Buffer.from(jwk.x, 'base64url'), Buffer.from(jwk.y, 'base64url')]); };
    const aadFor = (label, seq) => { const out = Buffer.alloc(label.length + 8); Buffer.from(label).copy(out); out.writeBigUInt64BE(BigInt(seq), label.length); return out; };
    const sealWith = (key, label, seq, json) => {
      const iv = crypto.randomBytes(12);
      const cipher = crypto.createCipheriv('aes-256-gcm', key, iv);
      cipher.setAAD(aadFor(label, seq));
      return Buffer.concat([iv, cipher.update(json), cipher.final(), cipher.getAuthTag()]);
    };
    const openWith = (key, label, seq, frame) => {
      const decipher = crypto.createDecipheriv('aes-256-gcm', key, frame.subarray(0, 12));
      decipher.setAAD(aadFor(label, seq));
      decipher.setAuthTag(frame.subarray(frame.length - 16));
      return Buffer.concat([decipher.update(frame.subarray(12, frame.length - 16)), decipher.final()]).toString();
    };
    const rawBrowser = async ({ answer = true } = {}) => {
      const socket = new WebSocket(`ws://127.0.0.1:${relay}/${id}/ws`);
      sockets.push(socket);
      const inbox = [];
      socket.on('message', (data, binary) => { if (binary) inbox.push(Buffer.from(data)); });
      const closed = new Promise(resolve => socket.once('close', code => resolve(code)));
      await new Promise((resolve, reject) => { socket.once('open', resolve); socket.once('error', reject); });
      const first = await until(() => inbox[0]);
      assert.equal(first.subarray(0, 4).toString(), 'KCH3');
      if (!answer) return { socket, closed, inbox };
      const macEph = first.subarray(4);
      const ecdh = crypto.createECDH('prime256v1');
      const pageEph = ecdh.generateKeys();
      socket.send(Buffer.concat([Buffer.from('KCB3'), pageEph]));
      const transcript = crypto.createHash('sha256').update('kaku-remote v3').update(id).update(macEph).update(pageEph).digest();
      const shared = ecdh.computeSecret(macEph);
      const derive = info => Buffer.from(crypto.hkdfSync('sha256', shared, transcript, info, 32));
      const keys = { toHost: derive('kaku c2h'), fromHost: derive('kaku h2c') };
      const auth = JSON.parse(openWith(keys.fromHost, 'kaku-web h2c', 0, await until(() => inbox[1])));
      const macKey = crypto.createPublicKey({ key: { kty: 'EC', crv: 'P-256', x: macPub.subarray(1, 33).toString('base64url'), y: macPub.subarray(33).toString('base64url') }, format: 'jwk' });
      assert.equal(auth.type, 'mac_auth');
      assert.equal(auth.key, macPub.toString('base64url'));
      // The link was used, so Kaku vouches for its key to no one.
      assert.equal(auth.tag, undefined);
      assert.ok(crypto.verify('sha256', transcript, { key: macKey, dsaEncoding: 'ieee-p1363' }, Buffer.from(auth.sig, 'base64url')), 'Kaku signs the exchange with the paired key');
      return { socket, closed, inbox, keys, transcript };
    };
    const stranger = crypto.generateKeyPairSync('ec', { namedCurve: 'P-256' });
    const signIn = (conn, device) => JSON.stringify({ type: 'auth', device: rawPub(device.publicKey).toString('base64url'),
      sig: crypto.sign('sha256', conn.transcript, { key: device.privateKey, dsaEncoding: 'ieee-p1363' }).toString('base64url') });
    const unknown = await rawBrowser();
    unknown.socket.send(sealWith(unknown.keys.toHost, 'kaku-web c2h', 0, signIn(unknown, stranger)));
    assert.equal(await Promise.race([unknown.closed, sleep(5000).then(() => 'timeout')]), 4403);
    const recorded = sealWith(unknown.keys.toHost, 'kaku-web c2h', 0, signIn(unknown, stranger));
    const replay = await rawBrowser();
    replay.socket.send(recorded);
    assert.equal(await Promise.race([replay.closed, sleep(5000).then(() => 'timeout')]), 4403);
    const misplaced = await rawBrowser();
    misplaced.socket.send(sealWith(misplaced.keys.toHost, 'kaku-web c2h', 1, signIn(misplaced, stranger)));
    assert.equal(await Promise.race([misplaced.closed, sleep(5000).then(() => 'timeout')]), 4403);
    // A page from before pairing, open across the update, is asked to reload.
    const oldPage = await rawBrowser({ answer: false });
    oldPage.socket.send(Buffer.concat([Buffer.from('KCB2'), crypto.randomBytes(16)]));
    assert.equal(await Promise.race([oldPage.closed, sleep(5000).then(() => 'timeout')]), 4426);

    // Revoking the paired browser ends its session within seconds.
    await cli('remote', 'revoke', deviceId);
    await encrypted.waitForFunction(() => document.querySelector('#notice .title')?.textContent === 'This browser is not paired', undefined, { timeout: 10000 });

    // Replacing a host retires its browser immediately, even if old output
    // was queued. Use a separate synthetic identity, never the app's link.
    const secret = crypto.randomBytes(32).toString('hex');
    const replacementId = crypto.createHash('sha256').update(secret).digest('hex').slice(0, 32);
    const hostURL = `ws://127.0.0.1:${relay}/host/${replacementId}`;
    const connectHost = async () => {
      const socket = new WebSocket(hostURL, { headers: { 'x-kaku-secret': secret, 'x-kaku-access': 'live-code' } });
      sockets.push(socket);
      await new Promise((resolve, reject) => { socket.once('open', resolve); socket.once('error', reject); });
      return socket;
    };
    const oldHost = await connectHost();
    let oldInputs = 0;
    oldHost.on('message', (_, binary) => { if (binary) oldInputs++; });
    const stale = new WebSocket(`ws://127.0.0.1:${relay}/t/${replacementId}/ws`);
    sockets.push(stale);
    const retired = new Promise(resolve => stale.once('close', resolve));
    await new Promise(resolve => stale.once('open', resolve));
    stale.send(Buffer.from('before'));
    await until(() => oldInputs === 1);
    await connectHost();
    assert.notEqual(await Promise.race([retired, sleep(2000).then(() => 'timeout')]), 'timeout');
    assert.equal(oldInputs, 1);
    // Rejection must keep its close code even with more browser input queued.
    const rejecting = await connectHost();
    rejecting.on('message', (data, binary) => {
      if (!binary) return;
      const c = Buffer.from(data).readUInt32BE(1);
      rejecting.send(JSON.stringify({ t: 'close', c, code: 4403 }));
    });
    const racing = new WebSocket(`ws://127.0.0.1:${relay}/t/${replacementId}/ws`);
    sockets.push(racing);
    const rejected = new Promise(resolve => racing.once('close', resolve));
    await new Promise(resolve => racing.once('open', resolve));
    for (let i = 0; i < 100; i++) racing.send(Buffer.from('queued-input'));
    assert.equal(await Promise.race([rejected, sleep(2000).then(() => 'timeout')]), 4403);
    const reset = start(path.join(app, 'Contents/MacOS/kaku'), ['remote', '--reset'], {
      HOME: home, XDG_CONFIG_HOME: path.join(home, '.config'), TMPDIR: path.join(home, 'tmp/')
    });
    const resetExit = await new Promise(resolve => reset.once('exit', resolve));
    assert.equal(resetExit, 0);
    await until(async () => (await fetch(relayBase)).status === 404);
    assert.notEqual(JSON.parse(fs.readFileSync(identityPath)).mac_key, identity.mac_key);
    assert.equal((await cli('remote', 'devices')).includes(' on '), false);
    assert.deepEqual(errors, []);
    console.log(JSON.stringify({ localTerminal: 'pass', foreignHostRefused: 'pass', frameWindow: frames.length, latestFrameAfterAck: 'pass', hiddenInput: 'pass', encryptedTerminal: 'pass', pairingLinkOnce: 'pass', unknownDeviceRefused: 'pass', macSignatureVerified: 'pass', idleAuthentication: 'pass', replayRejected: 'pass', oldPageAskedToReload: 'pass', revokeEndsSession: 'pass', retiredHost: 'pass', legacyHello: 'pass', encryptedUploadReconnect: 'pass', downloadByCmdClick: 'pass', rejectionWithQueuedInput: 'pass', paneActions: 'pass', pairedNewTab: 'pass', resetForgetsDevices: 'pass' }));
  } finally {
    for (const ws of sockets) ws.terminate();
    await browser?.close();
    for (const p of children.reverse()) { p.kill('SIGTERM'); await Promise.race([new Promise(r => p.once('exit', r)), sleep(3000)]); if (p.exitCode === null) p.kill('SIGKILL'); }
    fs.rmSync(home, { recursive: true, force: true });
  }
})().catch(error => { console.error(error.stack); process.exitCode = 1; });
