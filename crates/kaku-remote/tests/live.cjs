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
    fs.writeFileSync(config, `return {default_prog={'/bin/zsh','-f'}, default_cwd=${JSON.stringify(home)}, restore_previous_session=false, initial_cols=80, initial_rows=24, font_size=16, remote={enabled=true,bind='127.0.0.1',port=${local},tunnel=true,tunnel_url='ws://127.0.0.1:${relay}'}}`);
    start(path.join(root, 'crates/kaku-relay/target/debug/kaku-relay'), [], { PORT: String(relay) });
    await until(async () => { try { return (await fetch(`http://127.0.0.1:${relay}/health`)).ok; } catch { return false; } });
    const app = process.env.KAKU_TEST_APP || path.join(root, 'dist/Kaku.app');
    start(path.join(app, 'Contents/MacOS/kaku-gui'), ['--config-file', config, 'start', '--always-new-process'], {
      HOME: home, XDG_CONFIG_HOME: path.join(home, '.config'), TMPDIR: path.join(home, 'tmp/')
    });
    const statePath = path.join(home, 'tmp/kaku-remote.json');
    await until(() => fs.existsSync(statePath));
    const state = JSON.parse(fs.readFileSync(statePath));
    const url = `http://127.0.0.1:${local}/#t=${state.token}`;
    browser = await chromium.launch(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE
      ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE } : {});
    const page = await browser.newPage({ viewport: { width: 1100, height: 650 } });
    const errors = []; page.on('pageerror', e => errors.push(e.message));
    await page.goto(url);
    await page.waitForSelector('body.drawn');
    await page.locator('.xterm-helper-textarea').focus();
    await page.keyboard.type('printf "LIVE-OK\\n"\n');
    await page.waitForFunction(() => [...document.querySelectorAll('.xterm-rows > div')].some(row => row.textContent.trim() === 'LIVE-OK'));

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
    await sleep(200);
    send({ type: 'text', data: 'for i in {1..30}; do echo FLOW-$i; sleep 0.03; done\n' });
    await sleep(1500);
    const frames = received.filter(m => m.type === 'frame');
    assert.equal(frames.length, 4, 'slow consumers receive at most four unacknowledged frames');
    send({ type: 'frame_ack', seq: 1000000 }); await sleep(100);
    assert.equal(received.filter(m => m.type === 'frame').length, 4, 'future ACK cannot release the window');
    send({ type: 'frame_ack', seq: frames.at(-1).seq });
    const latest = await until(() => received.filter(m => m.type === 'frame').at(-1).seq > frames.at(-1).seq);
    assert.ok(latest);
    assert.ok(received.filter(m => m.type === 'frame').slice(4).some(m => m.data.includes('FLOW-30')), 'ACK releases the latest coalesced screen');
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
    const relayBase = `http://127.0.0.1:${relay}/t/${id}/`;
    await until(async () => (await fetch(relayBase)).ok);
    const bad = await browser.newPage();
    await bad.goto(relayBase + '#k=' + crypto.randomBytes(32).toString('base64url'));
    await bad.waitForFunction(() => document.querySelector('#notice .title')?.textContent === 'This link no longer works');
    const encrypted = await browser.newPage();
    await encrypted.addInitScript(() => {
      const Original = WebSocket;
      window.__testSockets = [];
      window.WebSocket = class extends Original {
        constructor(...args) { super(...args); window.__testSockets.push(this); }
      };
    });
    await encrypted.goto(relayBase + '#k=' + identity.key);
    await encrypted.waitForSelector('body.drawn');
    await encrypted.locator('.xterm-helper-textarea').focus();
    await encrypted.keyboard.type('echo ENCRYPTED-OK\n');
    await encrypted.waitForFunction(() => [...document.querySelectorAll('.xterm-rows > div')].some(row => row.textContent.trim() === 'ENCRYPTED-OK'));
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

    // Replacing a host retires its browser immediately, even if old output
    // was queued. Use a separate synthetic identity, never the app's link.
    const secret = crypto.randomBytes(32).toString('hex');
    const replacementId = crypto.createHash('sha256').update(secret).digest('hex').slice(0, 32);
    const hostURL = `ws://127.0.0.1:${relay}/host/${replacementId}`;
    const connectHost = async () => {
      const socket = new WebSocket(hostURL, { headers: { 'x-kaku-secret': secret } });
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
    assert.deepEqual(errors, []);
    console.log(JSON.stringify({ localTerminal: 'pass', frameWindow: frames.length, latestFrameAfterAck: 'pass', hiddenInput: 'pass', encryptedTerminal: 'pass', wrongKey: 'pass', idleAuthentication: 'pass', retiredHost: 'pass', legacyHello: 'pass', encryptedUploadReconnect: 'pass', rejectionWithQueuedInput: 'pass' }));
  } finally {
    for (const ws of sockets) ws.terminate();
    await browser?.close();
    for (const p of children.reverse()) { p.kill('SIGTERM'); await Promise.race([new Promise(r => p.once('exit', r)), sleep(3000)]); if (p.exitCode === null) p.kill('SIGKILL'); }
    fs.rmSync(home, { recursive: true, force: true });
  }
})().catch(error => { console.error(error.stack); process.exitCode = 1; });
