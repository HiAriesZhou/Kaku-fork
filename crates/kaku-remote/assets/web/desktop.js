// Kaku Desktop: the Mac's own screen in the web terminal, from the
// kaku-desktop helper that Kaku starts when the page asks. A button in the
// tab bar switches between the terminal and the screen; the screen arrives
// as H.264, decoded here with WebCodecs, and the mouse and keyboard over it
// go to the Mac. Loaded only when Kaku says the helper is installed.
(() => {
  "use strict";

  const CSS = `
  #desktop { position: absolute; inset: 0; z-index: 4; display: none; align-items: center; justify-content: center; background: var(--bg); overflow: hidden; }
  body.desktop #desktop { display: flex; }
  /* Full screen: the Mac's screen covers the whole display, bar included,
     without resizing the stage, so the terminal keeps its grid on the Mac. */
  body.desktop.desktop-full #desktop { position: fixed; inset: 0; z-index: 50; background: #000; }
  body.desktop #term, body.desktop #past, body.desktop #history, body.desktop #find { visibility: hidden !important; }
  body.desktop #keys { display: none !important; }
  /* The screen, not a tab, is what is showing: the button alone reads as current. */
  body.desktop #bar .tab[aria-current="true"] { background: transparent; color: var(--tab-fg); }
  body.desktop #bar .tab[aria-current="true"]:hover { background: var(--tab-hover-bg); color: var(--tab-hover-fg); }
  body.desktop #bar .tab[aria-current="true"] .close { opacity: 0; pointer-events: none; }
  #desktop canvas { display: block; max-width: none; outline: none; opacity: 0; transition: opacity 0.18s ease-out; touch-action: none; image-rendering: auto; transform-origin: 0 0; }
  #desktop.live canvas { opacity: 1; }
  #desktop .sink { position: absolute; left: -9999px; top: 0; width: 1px; height: 1px; opacity: 0; resize: none; }
  #desktop .status { position: absolute; inset: 0; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 6px; padding: 24px; text-align: center; pointer-events: none; transition: opacity 0.18s; }
  #desktop.live .status { opacity: 0; }
  #desktop .status .title { color: var(--fg); font-size: 15px; }
  #desktop .status .detail { color: var(--muted); font-size: 13px; max-width: 420px; }
  #desktop .status .detail:empty { display: none; }
  #desktop .status .spin { width: 18px; height: 18px; margin-bottom: 8px; border: 2px solid rgba(127,127,127,0.25); border-top-color: var(--fg); border-radius: 50%; opacity: 0.7; animation: desktop-spin 0.9s linear infinite; }
  #desktop.failed .status .spin { display: none; }
  @keyframes desktop-spin { to { transform: rotate(360deg); } }
  #bar .tool.desktop svg, #bar .tool.screens svg { width: 15px; height: 15px; }
  #bar .tool.screens { display: none; }
  body.desktop.screens #bar .tool.screens { display: flex; }
  /* Files dragged over the screen: the same outline as over the terminal,
     drawn above the picture. */
  #desktop::after { content: ""; position: absolute; inset: 0; pointer-events: none; box-shadow: inset 0 0 0 2px var(--muted); opacity: 0; }
  body.dropping #desktop::after { opacity: 1; }
  `;
  const SCREENS_ICON = '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.25" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="1.25" y="4.5" width="9" height="6.5" rx="1.25"/><path d="M4.5 4.5V3.25c0-.7.55-1.25 1.25-1.25h7.75c.7 0 1.25.55 1.25 1.25v5c0 .7-.55 1.25-1.25 1.25H10.25M4.5 13.5h3.5"/></svg>';
  const ICON = '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.25" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="1.75" y="2.5" width="12.5" height="8.5" rx="1.5"/><path d="M6 13.5h4M8 11v2.5"/></svg>';

  // Pictures the page may hold undrawn; the helper paces itself on our acks.
  const WHEEL_LINE = 16;
  const LONG_PRESS_MS = 450;
  const TAP_SLOP = 10;
  // A pinch that changes the fingers' spread by more than this zooms
  // instead of scrolling.
  const PINCH_START = 0.12;
  const MAX_ZOOM = 5;
  // How long a paste waits for the browser's clipboard before pressing
  // Command-V with whatever the Mac already holds, and a copy for the Mac's.
  const PASTE_WAIT_MS = 300;
  const COPY_WAIT_MS = 1500;
  // Browser messages through the relay are capped at 512 KB once sealed.
  const MAX_PASTE_BYTES = 256 * 1024;

  const init = (api) => {
    const { send, toolsEl, stageEl, isMac, toast, focusTerminal } = api;
    // The page's own reading of the screen, so both agree on touch.
    const TOUCH = api.touch ?? matchMedia("(pointer: coarse)").matches;
    const style = document.createElement("style");
    style.textContent = CSS;
    document.head.append(style);

    const view = document.createElement("div");
    view.id = "desktop";
    view.innerHTML = '<canvas tabindex="0" aria-label="Mac screen"></canvas><textarea class="sink" tabindex="-1" aria-hidden="true" autocomplete="off" autocorrect="off" autocapitalize="off" spellcheck="false"></textarea><div class="status"><div class="spin"></div><div class="title"></div><div class="detail"></div></div>';
    stageEl.append(view);
    const canvas = view.querySelector("canvas");
    // With a keyboard, focus sits in a hidden text field: only an editable
    // target gets the browser's paste event, which carries its clipboard.
    // A touch screen keeps focus on the canvas so no soft keyboard opens.
    const sink = view.querySelector(".sink");
    const focusView = () => (TOUCH ? canvas : sink).focus({ preventScroll: true });
    const ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
    const statusTitle = view.querySelector(".status .title");
    const statusDetail = view.querySelector(".status .detail");

    const button = document.createElement("button");
    button.type = "button";
    button.className = "tool desktop";
    button.innerHTML = ICON;
    // Beside the full screen button, the two icons together, with the
    // clock's text to their left rather than between them.
    // With more than one display on the Mac, the next one is a click away.
    const screens = document.createElement("button");
    screens.type = "button";
    screens.className = "tool screens";
    screens.innerHTML = SCREENS_ICON;
    const anchor = toolsEl.querySelector(".fs");
    for (const b of [button, screens]) {
      if (anchor) toolsEl.insertBefore(b, anchor); else toolsEl.append(b);
    }

    let active = false;
    let decoder = null;
    let configured = "";
    let haveKey = false;
    let videoW = 0;
    let videoH = 0;
    let canControl = true;
    let warnedControl = false;
    let lastDrawn = 0;
    let displays = [];
    let shownDisplay = 0;
    const DISPLAY_SLOT = "kaku.desktop.display";
    let chosenDisplay = 0;
    try { chosenDisplay = Number(localStorage.getItem(DISPLAY_SLOT)) || 0; } catch (_) {}

    const showStatus = (title, detail = "", failed = false) => {
      statusTitle.textContent = title;
      statusDetail.textContent = detail;
      view.classList.toggle("failed", failed);
      view.classList.remove("live");
    };

    const syncButton = () => {
      button.setAttribute("aria-current", active ? "true" : "false");
      button.title = active ? "Back to the terminal" : "Show the Mac's screen";
    };
    syncButton();

    // The canvas keeps the video's own pixels; CSS fits it in the stage
    // with its aspect kept.
    const layout = () => {
      if (!videoW) return;
      const s = Math.min(view.clientWidth / videoW, view.clientHeight / videoH);
      canvas.style.width = Math.floor(videoW * s) + "px";
      canvas.style.height = Math.floor(videoH * s) + "px";
      setZoom(zoom.z, zoom.x, zoom.y);
    };

    // A phone shows a whole Mac screen too small to read, so a pinch zooms
    // in and moves around. The zoom is the page's alone; the pointer
    // positions sent to the Mac come from the canvas's on-screen rectangle,
    // so they follow it without any extra math.
    const zoom = { z: 1, x: 0, y: 0 };
    const setZoom = (z, x, y) => {
      const w = canvas.offsetWidth;
      const h = canvas.offsetHeight;
      z = Math.min(MAX_ZOOM, Math.max(1, z));
      zoom.z = z;
      zoom.x = Math.min(0, Math.max(w - w * z, x));
      zoom.y = Math.min(0, Math.max(h - h * z, y));
      canvas.style.transform = z === 1 ? "" : `translate(${zoom.x}px, ${zoom.y}px) scale(${z})`;
    };
    new ResizeObserver(() => { layout(); resized(); }).observe(view);

    // The picture is sized to the view in device pixels, up to the Mac's
    // own (the helper never scales up), so a Retina or 4K screen gets every
    // pixel and a small window no more than it can show.
    let requested = null;
    const request = () => {
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      // A touch screen zooms in, so it gets the detail a zoom shows.
      const floor = TOUCH ? 1920 : 640;
      requested = {
        type: "desktop_start",
        max_width: Math.min(3840, Math.max(floor, Math.round(view.clientWidth * dpr))),
        max_height: Math.min(2400, Math.max(floor * 0.625, Math.round(view.clientHeight * dpr))),
        fps: 30,
        // With a mouse the page's own pointer is the one to follow, in the
        // Mac's shape; a touch screen has none, so the Mac's stays in the
        // picture.
        cursor: TOUCH,
        binary: true,
      };
      if (chosenDisplay) requested.display = chosenDisplay;
      return requested;
    };
    // A view that grows (full screen) or shrinks by more than a tenth asks
    // for a picture of its new size; the old one stays up meanwhile.
    let resizeTimer = 0;
    const resized = () => {
      if (!active || !requested || document.hidden) return;
      clearTimeout(resizeTimer);
      resizeTimer = setTimeout(() => {
        if (!active || document.hidden || !view.clientWidth) return;
        const dpr = Math.min(window.devicePixelRatio || 1, 2);
        const w = Math.min(3840, view.clientWidth * dpr);
        const h = Math.min(2400, view.clientHeight * dpr);
        if (Math.abs(w / requested.max_width - 1) > 0.1 || Math.abs(h / requested.max_height - 1) > 0.1) send(request());
      }, 400);
    };

    const closeDecoder = () => {
      if (decoder && decoder.state !== "closed") { try { decoder.close(); } catch (_) {} }
      decoder = null;
      configured = "";
      haveKey = false;
    };

    const supported = () => typeof window.VideoDecoder === "function" && typeof window.EncodedVideoChunk === "function";

    const start = () => {
      closeDecoder();
      videoW = 0;
      setZoom(1, 0, 0);
      showStatus("Connecting to the Mac's screen");
      if (!supported()) {
        showStatus("This browser cannot show the Mac's screen", "It needs WebCodecs: a current Chrome, Edge or Safari.", true);
        return;
      }
      send(request());
    };

    // With a keyboard the screen opens in full screen, every key locked to
    // the page where the browser allows it (Chrome, Edge), so Command-W and
    // the like reach the Mac; holding Esc leaves full screen, and the bar
    // with this view's button comes back.
    let ownFullscreen = false;
    const lockKeys = () => navigator.keyboard?.lock?.().catch(() => {});
    const goFullscreen = () => {
      if (TOUCH || !document.fullscreenEnabled) return;
      if (document.fullscreenElement) { lockKeys(); return; }
      document.documentElement.requestFullscreen().then(() => {
        ownFullscreen = true;
        lockKeys();
        focusView();
      }).catch(() => {});
    };
    const syncFullscreen = () => {
      document.body.classList.toggle("desktop-full", active && !!document.fullscreenElement);
      if (!document.fullscreenElement) ownFullscreen = false;
    };
    document.addEventListener("fullscreenchange", syncFullscreen);

    const enter = () => {
      if (active) return;
      active = true;
      document.body.classList.add("desktop");
      syncButton();
      if (document.activeElement && document.activeElement !== document.body) document.activeElement.blur();
      goFullscreen();
      syncFullscreen();
      start();
      focusView();
    };

    const leave = () => {
      if (!active) return;
      active = false;
      if (pasting) { clearTimeout(pasting.timer); pasting = null; }
      if (copyWait) copyWait.reject(new Error("left"));
      clearTimeout(resizeTimer);
      releaseAll();
      send({ type: "desktop_stop" });
      closeDecoder();
      document.body.classList.remove("desktop");
      view.classList.remove("live");
      canvas.style.cursor = "";
      syncScreens();
      if (ownFullscreen && document.fullscreenElement) document.exitFullscreen().catch(() => {});
      syncFullscreen();
      syncButton();
      focusTerminal();
    };

    button.addEventListener("click", () => (active ? leave() : enter()));
    // Command-W outside full screen closes the browser tab, not the Mac's
    // window under the pointer; ask before the page goes.
    window.addEventListener("beforeunload", (ev) => {
      if (!active) return;
      ev.preventDefault();
      ev.returnValue = "";
    });
    // Picking a tab means the terminal.
    document.getElementById("tabs")?.addEventListener("click", () => { if (active) leave(); }, true);
    document.addEventListener("visibilitychange", () => {
      if (!active) return;
      if (document.hidden) { releaseAll(); send({ type: "desktop_stop" }); closeDecoder(); }
      else start();
    });

    const ack = (seq) => send({ type: "desktop_ack", seq });
    const wantKey = () => {
      haveKey = false;
      send({ type: "desktop_keyframe" });
    };

    // A new stream (another size, another display) brings new parameter
    // sets: a change of description reconfigures the decoder in order.
    const ensureDecoder = (codec, desc) => {
      const key = codec + ":" + desc;
      if (decoder && decoder.state === "configured" && configured === key) return true;
      const config = { codec, description: b64(desc), optimizeForLatency: true, hardwareAcceleration: "no-preference" };
      if (decoder && decoder.state === "configured") {
        try {
          decoder.configure(config);
          configured = key;
          return true;
        } catch (_) {}
      }
      closeDecoder();
      decoder = new VideoDecoder({
        output: (frame) => {
          const seq = frame.timestamp;
          if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
            canvas.width = frame.displayWidth;
            canvas.height = frame.displayHeight;
            videoW = frame.displayWidth;
            videoH = frame.displayHeight;
            layout();
          }
          ctx.drawImage(frame, 0, 0, canvas.width, canvas.height);
          frame.close();
          lastDrawn = seq;
          if (!view.classList.contains("live")) view.classList.add("live");
          ack(seq);
        },
        error: (e) => {
          console.warn("kaku desktop decoder", e);
          closeDecoder();
          if (active && !document.hidden) wantKey();
        },
      });
      try {
        decoder.configure(config);
      } catch (e) {
        showStatus("This browser cannot decode the Mac's screen", String(e.message || e), true);
        closeDecoder();
        return false;
      }
      configured = key;
      return true;
    };

    const b64 = (s) => {
      const bin = atob(s);
      const out = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
      return out;
    };

    const onFrame = (msg) => {
      if (!active || document.hidden) { ack(msg.seq); return; }
      if (msg.key) {
        if (!msg.codec || !msg.desc) { ack(msg.seq); return; }
        if (!ensureDecoder(msg.codec, msg.desc)) { ack(msg.seq); return; }
        haveKey = true;
      } else if (!haveKey || !decoder) {
        // A picture that depends on one this decoder never saw.
        ack(msg.seq);
        return;
      }
      try {
        decoder.decode(new EncodedVideoChunk({ type: msg.key ? "key" : "delta", timestamp: msg.seq, data: msg.bytes || b64(msg.data) }));
      } catch (e) {
        ack(msg.seq);
        wantKey();
      }
    };

    const syncScreens = () => {
      const many = active && displays.length > 1;
      document.body.classList.toggle("screens", many);
      if (!many) return;
      const at = Math.max(0, displays.findIndex((d) => d.id === shownDisplay));
      const next = displays[(at + 1) % displays.length];
      screens.title = `Showing ${displays[at].name}. Switch to ${next.name}`;
    };
    screens.addEventListener("click", () => {
      if (!active || displays.length < 2) return;
      const at = Math.max(0, displays.findIndex((d) => d.id === shownDisplay));
      chosenDisplay = displays[(at + 1) % displays.length].id;
      try { localStorage.setItem(DISPLAY_SLOT, String(chosenDisplay)); } catch (_) {}
      releaseAll();
      start();
      focusView();
    });

    // The Mac's pointer shape (an I-beam over text, a resize arrow at a
    // window's edge) on the page's own pointer, drawn at twice its size.
    const onCursor = (msg) => {
      if (!active || TOUCH || typeof msg.png !== "string") return;
      const url = `url("data:image/png;base64,${msg.png}")`;
      const hot = `${msg.hot_x | 0} ${msg.hot_y | 0}`;
      const scale = msg.scale || 1;
      for (const value of [`image-set(${url} ${scale}x) ${hot}, default`, `-webkit-image-set(${url} ${scale}x) ${hot}, default`]) {
        canvas.style.cursor = value;
        if (canvas.style.cursor) return;
      }
      canvas.style.cursor = "";
    };

    const onStatus = (msg) => {
      if (!active) return;
      if (msg.state === "streaming") {
        canControl = msg.control !== false;
        displays = Array.isArray(msg.displays) ? msg.displays : [];
        shownDisplay = msg.display || 0;
        syncScreens();
        if (!canControl && !warnedControl) {
          warnedControl = true;
          toast("View only: Kaku has no Accessibility permission on the Mac");
        }
      } else if (msg.state === "error") {
        closeDecoder();
        showStatus("Can't show the Mac's screen", msg.detail || "", true);
      }
    };

    // ── Input ────────────────────────────────────────────────────────────
    const held = new Set();
    const where = (ev) => {
      const r = canvas.getBoundingClientRect();
      return {
        x: Math.min(1, Math.max(0, (ev.clientX - r.left) / Math.max(1, r.width))),
        y: Math.min(1, Math.max(0, (ev.clientY - r.top) / Math.max(1, r.height))),
      };
    };
    const live = () => active && canControl && view.classList.contains("live");

    let pendingMove = null;
    let moveFrame = 0;
    const flushMove = () => {
      moveFrame = 0;
      if (pendingMove) { send(pendingMove); pendingMove = null; }
    };
    // Modifier keys as the Mac reads them: off a Mac, Ctrl stands for
    // Command and the Windows key for Control, so Ctrl-click selects more
    // in Finder as it does in Explorer.
    const mods = (ev) => {
      if (!ev) return {};
      let meta = ev.metaKey;
      let ctrl = ev.ctrlKey;
      if (!isMac) [meta, ctrl] = [ctrl, meta];
      return { meta, ctrl, alt: ev.altKey, shift: ev.shiftKey };
    };
    // The Mac counts double clicks itself: browsers give pointer events no
    // click count.
    const pointer = (kind, p, button = 0, ev = null) => {
      if (kind === "move") {
        pendingMove = { type: "desktop_pointer", kind, x: p.x, y: p.y, ...mods(ev) };
        if (!moveFrame) moveFrame = requestAnimationFrame(flushMove);
        return;
      }
      if (pendingMove) flushMove();
      if (kind === "down") held.add(button); else held.delete(button);
      send({ type: "desktop_pointer", kind, x: p.x, y: p.y, button, ...mods(ev) });
    };
    const releaseButtons = () => {
      for (const b of held) send({ type: "desktop_pointer", kind: "up", x: lastPoint.x, y: lastPoint.y, button: b });
      held.clear();
    };
    let lastPoint = { x: 0.5, y: 0.5 };

    // Touch: a tap clicks, a held finger right-clicks, a finger that moves
    // drags, two fingers scroll, and a pinch zooms. Once zoomed in, two
    // fingers move the view instead of scrolling the Mac.
    const touches = new Map();
    let touchPlan = null;
    const touchCenter = () => {
      let x = 0;
      let y = 0;
      for (const t of touches.values()) { x += t.x; y += t.y; }
      return { x: x / touches.size, y: y / touches.size };
    };
    const touchSpread = () => {
      const [a, b] = [...touches.values()];
      return a && b ? Math.max(1, Math.hypot(a.x - b.x, a.y - b.y)) : 1;
    };
    // The view box the zoom moves within, on screen.
    const baseRect = () => {
      const r = canvas.getBoundingClientRect();
      return { left: r.left - zoom.x, top: r.top - zoom.y };
    };

    canvas.addEventListener("pointerdown", (ev) => {
      if (!live()) return;
      ev.preventDefault();
      focusView();
      flushCopy();
      const p = where(ev);
      lastPoint = p;
      if (ev.pointerType === "touch") {
        touches.set(ev.pointerId, { x: ev.clientX, y: ev.clientY });
        if (touches.size === 1) {
          const startAt = { x: ev.clientX, y: ev.clientY };
          touchPlan = { mode: "maybe", start: startAt, point: p, timer: setTimeout(() => {
            if (touchPlan && touchPlan.mode === "maybe") {
              touchPlan.mode = "done";
              pointer("move", p);
              pointer("down", p, 2);
              pointer("up", p, 2);
            }
          }, LONG_PRESS_MS) };
        } else {
          if (touchPlan) { clearTimeout(touchPlan.timer); if (touchPlan.mode === "drag") pointer("up", lastPoint, 0); }
          const center = touchCenter();
          touchPlan = { mode: zoom.z > 1 ? "zoom" : "scroll", center, from: { center, spread: touchSpread(), z: zoom.z, x: zoom.x, y: zoom.y } };
        }
        return;
      }
      canvas.setPointerCapture(ev.pointerId);
      const button = ev.button === 2 ? 2 : ev.button === 1 ? 1 : 0;
      pointer("down", p, button, ev);
    });

    canvas.addEventListener("pointermove", (ev) => {
      if (!live()) return;
      const p = where(ev);
      if (ev.pointerType === "touch") {
        if (!touches.has(ev.pointerId)) return;
        touches.set(ev.pointerId, { x: ev.clientX, y: ev.clientY });
        if (!touchPlan) return;
        if (touchPlan.mode === "scroll" && touches.size > 1 && Math.abs(touchSpread() / touchPlan.from.spread - 1) > PINCH_START) {
          touchPlan.mode = "zoom";
        }
        if (touchPlan.mode === "zoom") {
          // Keep the point under the fingers' first midpoint under their
          // midpoint now.
          const f = touchPlan.from;
          const c = touchCenter();
          const z = Math.min(MAX_ZOOM, Math.max(1, f.z * (touches.size > 1 ? touchSpread() / f.spread : 1)));
          const b = baseRect();
          const px = (f.center.x - b.left - f.x) / f.z;
          const py = (f.center.y - b.top - f.y) / f.z;
          setZoom(z, c.x - b.left - z * px, c.y - b.top - z * py);
          return;
        }
        if (touchPlan.mode === "scroll") {
          const c = touchCenter();
          const dx = touchPlan.center.x - c.x;
          const dy = touchPlan.center.y - c.y;
          touchPlan.center = c;
          send({ type: "desktop_wheel", x: lastPoint.x, y: lastPoint.y, dx: dx * 2, dy: dy * 2 });
          return;
        }
        if (touchPlan.mode === "maybe" && Math.hypot(ev.clientX - touchPlan.start.x, ev.clientY - touchPlan.start.y) > TAP_SLOP) {
          clearTimeout(touchPlan.timer);
          touchPlan.mode = "drag";
          pointer("move", touchPlan.point);
          pointer("down", touchPlan.point, 0);
        }
        if (touchPlan.mode === "drag") { lastPoint = p; pointer("move", p); }
        return;
      }
      lastPoint = p;
      pointer("move", p, 0, ev);
    });

    const endPointer = (ev) => {
      if (ev.pointerType === "touch") {
        touches.delete(ev.pointerId);
        if (!touchPlan) return;
        if (touchPlan.mode === "maybe") {
          clearTimeout(touchPlan.timer);
          if (live()) {
            pointer("move", touchPlan.point);
            pointer("down", touchPlan.point, 0);
            pointer("up", touchPlan.point, 0);
          }
        } else if (touchPlan.mode === "drag") {
          pointer("up", lastPoint, 0);
        }
        if (touchPlan.mode === "zoom") {
          if (zoom.z < 1.08) setZoom(1, 0, 0);
          // One finger left on the glass keeps moving the view from here.
          if (touches.size === 1) {
            const center = touchCenter();
            touchPlan.from = { center, spread: 1, z: zoom.z, x: zoom.x, y: zoom.y };
          }
        }
        if (touches.size === 0) touchPlan = null;
        return;
      }
      if (!active) return;
      const button = ev.button === 2 ? 2 : ev.button === 1 ? 1 : 0;
      if (held.has(button)) pointer("up", where(ev), button, ev);
    };
    canvas.addEventListener("pointerup", endPointer);
    canvas.addEventListener("pointercancel", (ev) => {
      if (ev.pointerType === "touch") { touches.delete(ev.pointerId); if (touchPlan?.timer) clearTimeout(touchPlan.timer); touchPlan = null; }
      else releaseButtons();
    });
    canvas.addEventListener("contextmenu", (ev) => ev.preventDefault());

    let wheel = null;
    let wheelFrame = 0;
    canvas.addEventListener("wheel", (ev) => {
      if (!live()) return;
      ev.preventDefault();
      const unit = ev.deltaMode === 1 ? WHEEL_LINE : ev.deltaMode === 2 ? view.clientHeight : 1;
      const p = where(ev);
      if (!wheel) wheel = { type: "desktop_wheel", x: p.x, y: p.y, dx: 0, dy: 0 };
      wheel.x = p.x;
      wheel.y = p.y;
      wheel.dx += ev.deltaX * unit;
      wheel.dy += ev.deltaY * unit;
      if (!wheelFrame) wheelFrame = requestAnimationFrame(() => {
        wheelFrame = 0;
        if (wheel) { send(wheel); wheel = null; }
      });
    }, { passive: false });

    // Keys go to the Mac by physical position; the Mac's layout and input
    // method make the character. Off a Mac, Ctrl stands for Command (copy,
    // paste, undo work as the hands expect) and the Windows key for Control.
    const SWAP = { ControlLeft: "MetaLeft", ControlRight: "MetaRight", MetaLeft: "ControlLeft", MetaRight: "ControlRight", OSLeft: "ControlLeft", OSRight: "ControlRight" };
    const keyMessage = (ev, down) => {
      const code = isMac ? ev.code : SWAP[ev.code] || ev.code;
      return { type: "desktop_key", code, down, ...mods(ev) };
    };
    const ownsKeys = (ev) => {
      if (!active) return false;
      const t = ev.target;
      // A field of the page itself (renaming a tab, a dialog) keeps its keys.
      if (t && (t.tagName === "INPUT" || (t.tagName === "TEXTAREA" && t !== sink && !t.classList.contains("xterm-helper-textarea")))) return false;
      return true;
    };

    // Clipboard. Command-V (Ctrl-V off a Mac) first puts the browser's
    // clipboard on the Mac, then presses the keys there. Command-C and
    // Command-X hand the browser a clipboard entry that fills in once the
    // Mac reports its copy; that write starts inside the key press, which
    // browsers require. A copy made on the Mac some other way (a menu, a
    // right click) is written when the browser allows it, or at the next
    // click or key press here.
    let pasting = null;
    let copyWait = null;
    let pendingCopy = null;
    const finishPaste = (text) => {
      if (!pasting) return;
      clearTimeout(pasting.timer);
      const { msg } = pasting;
      pasting = null;
      if (!live()) return;
      if (text) {
        if (new TextEncoder().encode(text).length > MAX_PASTE_BYTES) {
          toast("Too much text to paste on the Mac at once");
          return;
        }
        send({ type: "desktop_clipboard", text });
      }
      send(msg);
      send({ ...msg, down: false });
    };
    window.addEventListener("paste", (ev) => {
      if (!pasting) return;
      ev.preventDefault();
      ev.stopImmediatePropagation();
      finishPaste(ev.clipboardData?.getData("text/plain") || "");
    }, true);
    const writeClipboard = (text) => {
      if (!navigator.clipboard?.writeText) return;
      navigator.clipboard.writeText(text).then(() => { if (pendingCopy === text) pendingCopy = null; }, () => { pendingCopy = text; });
    };
    const flushCopy = () => {
      if (pendingCopy !== null) writeClipboard(pendingCopy);
    };
    const awaitCopy = () => {
      if (!navigator.clipboard?.write || typeof window.ClipboardItem !== "function") return;
      if (copyWait) copyWait.reject(new Error("superseded"));
      const text = new Promise((resolve, reject) => {
        const timer = setTimeout(() => { copyWait = null; reject(new Error("the Mac copied nothing")); }, COPY_WAIT_MS);
        copyWait = {
          resolve: (t) => { clearTimeout(timer); copyWait = null; resolve(t); },
          reject: (e) => { clearTimeout(timer); copyWait = null; reject(e); },
        };
      });
      const blob = text.then((t) => new Blob([t], { type: "text/plain" }));
      navigator.clipboard.write([new ClipboardItem({ "text/plain": blob })]).then(() => { pendingCopy = null; }, () => {});
    };
    const onClipboard = (msg) => {
      if (!active || typeof msg.text !== "string") return;
      if (copyWait) copyWait.resolve(msg.text);
      else writeClipboard(msg.text);
    };

    const pressed = new Set();
    // Keys and buttons still held when the view goes away would stay down
    // on the Mac.
    const releaseAll = () => {
      for (const code of pressed) send({ type: "desktop_key", code: isMac ? code : SWAP[code] || code, down: false });
      pressed.clear();
      releaseButtons();
    };
    const onKey = (down) => (ev) => {
      if (!ownsKeys(ev)) return;
      const command = (isMac ? ev.metaKey : ev.ctrlKey) && !ev.altKey;
      if (down && command && ev.code === "KeyV" && !TOUCH && live()) {
        // Let the browser go on to its paste event.
        ev.stopImmediatePropagation();
        if (!pasting) pasting = { msg: keyMessage(ev, true), timer: setTimeout(() => finishPaste(""), PASTE_WAIT_MS) };
        return;
      }
      ev.preventDefault();
      ev.stopImmediatePropagation();
      if (!live() || !ev.code) return;
      if (down) flushCopy();
      if (down && command && (ev.code === "KeyC" || ev.code === "KeyX") && !ev.repeat) awaitCopy();
      if (down) pressed.add(ev.code); else if (!pressed.delete(ev.code)) return;
      send(keyMessage(ev, down));
    };
    window.addEventListener("keydown", onKey(true), true);
    window.addEventListener("keyup", onKey(false), true);
    // Keys still held when the page loses focus would stay down on the Mac.
    window.addEventListener("blur", releaseAll);

    return {
      handle(msg) {
        if (msg.type === "desktop_frame") onFrame(msg);
        else if (msg.type === "desktop_status") onStatus(msg);
        else if (msg.type === "desktop_clipboard") onClipboard(msg);
        else if (msg.type === "desktop_cursor") onCursor(msg);
      },
      // A new session (a reconnect) has no helper running yet.
      sessionStarted() {
        if (active && !document.hidden) start();
      },
      get active() { return active; },
      get lastDrawn() { return lastDrawn; },
      enter,
      leave,
    };
  };

  window.KakuDesktop = { init };
})();
