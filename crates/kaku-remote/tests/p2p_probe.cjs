// Interop check for the direct path against a real browser: run with
// `cargo test -p kaku-remote --features desktop browser_probe -- --ignored`
// in another shell, both with the same KAKU_P2P_DIR.
const fs = require('node:fs');
const path = require('node:path');
const { chromium } = require('playwright');

(async () => {
  const dir = process.env.KAKU_P2P_DIR;
  const browser = await chromium.launch({ executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE });
  const page = await browser.newPage();
  await page.goto('about:blank');
  const offer = await page.evaluate(async () => {
    const pc = new RTCPeerConnection({ iceServers: [] });
    const dc = pc.createDataChannel('kaku');
    dc.binaryType = 'arraybuffer';
    window.t = { pc, dc, got: [], parts: [] };
    dc.onmessage = (ev) => {
      const b = new Uint8Array(ev.data);
      t.parts.push(b.length - 1);
      if (b[0] & 1) { t.got.push({ text: !!(b[0] & 2), size: t.parts.reduce((a, n) => a + n, 0), at: performance.now() }); t.parts = []; }
    };
    await pc.setLocalDescription(await pc.createOffer());
    await new Promise((r) => { if (pc.iceGatheringState === 'complete') r(); pc.onicegatheringstatechange = () => pc.iceGatheringState === 'complete' && r(); setTimeout(r, 2000); });
    return pc.localDescription.sdp;
  });
  console.log('offer candidates:', offer.split('\n').filter((l) => l.startsWith('a=candidate')).map((l) => l.trim()).join(' | ') || 'none');
  fs.writeFileSync(path.join(dir, 'offer.sdp'), offer);
  const answerPath = path.join(dir, 'answer.sdp');
  for (let i = 0; i < 100 && !fs.existsSync(answerPath); i++) await new Promise((r) => setTimeout(r, 100));
  const answer = fs.readFileSync(answerPath, 'utf8');
  const result = await page.evaluate(async (sdp) => {
    const began = performance.now();
    await t.pc.setRemoteDescription({ type: 'answer', sdp });
    await new Promise((r, j) => { if (t.dc.readyState === 'open') r(); t.dc.onopen = r; setTimeout(() => j(new Error('not open, ice ' + t.pc.iceConnectionState)), 10000); });
    const opened = performance.now() - began;
    const rtts = [];
    for (let i = 0; i < 5; i++) {
      const n = t.got.length;
      const sent = performance.now();
      t.dc.send(JSON.stringify({ type: 'ping', i }));
      while (t.got.length < n + 2) await new Promise((r) => setTimeout(r, 1));
      rtts.push(Math.round(t.got[n + 1].at - sent));
    }
    const pair = [...(await t.pc.getStats()).values()].find((s) => s.type === 'candidate-pair' && s.nominated);
    return { opened: Math.round(opened), rtts, got: t.got.slice(0, 2).map(({ text, size }) => ({ text, size })), pair: pair && pair.state };
  }, answer);
  console.log(JSON.stringify(result));
  await browser.close();
})().catch((e) => { console.error('FAILED', e.message); process.exit(1); });
