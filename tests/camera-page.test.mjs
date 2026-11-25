import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import vm from "node:vm";

const page = readFileSync(new URL("../crates/oc-camera/web/index.html", import.meta.url), "utf8");
const script = page.match(/<script>([\s\S]*?)<\/script>/)[1];

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

class Events {
  listeners = new Map();
  textContent = "";
  disabled = false;

  addEventListener(type, listener) {
    const listeners = this.listeners.get(type) || [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  emit(type) {
    if (type === "click" && this.disabled) return;
    for (const listener of this.listeners.get(type) || []) listener();
  }
}

function stream(facing = "environment", label = "Physical camera") {
  const track = Object.assign(new Events(), {
    label,
    stopped: 0,
    stop() { this.stopped += 1; },
    getSettings() { return { facingMode: facing }; },
  });
  return { track, getTracks: () => [track], getVideoTracks: () => [track] };
}

function wakeLock() {
  return Object.assign(new Events(), {
    released: 0,
    async release() { this.released += 1; this.emit("release"); },
  });
}

async function flush() {
  for (let index = 0; index < 30; index += 1) await Promise.resolve();
}

function browser() {
  const elements = Object.fromEntries(["video", "canvas", "status", "err", "start", "flip"].map((id) => [id, new Events()]));
  const cameras = [];
  const locks = [];
  const sockets = [];
  const encodes = [];
  const timers = new Map();
  let nextTimer = 0;
  const draws = [];
  const document = Object.assign(new Events(), {
    visibilityState: "visible",
    getElementById: (id) => elements[id],
  });
  const window = new Events();
  Object.assign(elements.video, {
    videoWidth: 1920,
    videoHeight: 1080,
    readyState: 2,
    srcObject: null,
    paused: 0,
    play: () => Promise.resolve(),
    pause() { this.paused += 1; },
  });
  Object.assign(elements.canvas, {
    getContext: () => ({ fillRect() {}, drawImage(...args) { draws.push(args); } }),
    toBlob(callback, type, quality) { encodes.push({ callback, type, quality }); },
  });
  class Socket {
    static OPEN = 1;
    readyState = 0;
    bufferedAmount = 0;
    sent = [];
    closed = 0;

    constructor(url) { this.url = url; sockets.push(this); }
    open() { this.readyState = Socket.OPEN; this.onopen?.(); }
    send(bytes) {
      assert.equal(this.readyState, Socket.OPEN);
      this.sent.push(bytes);
      this.bufferedAmount += bytes.byteLength;
    }
    close() { this.closed += 1; this.readyState = 3; this.onclose?.(); }
  }
  vm.runInNewContext(script, {
    document,
    window,
    navigator: {
      mediaDevices: { getUserMedia(constraints) { const request = deferred(); cameras.push({ constraints, ...request }); return request.promise; } },
      wakeLock: { request(kind) { assert.equal(kind, "screen"); const request = deferred(); locks.push(request); return request.promise; } },
    },
    WebSocket: Socket,
    location: { host: "100.64.0.1:8443" },
    setTimeout(callback, delay) { const id = ++nextTimer; timers.set(id, { callback, delay }); return id; },
    clearTimeout(id) { timers.delete(id); },
  }, { filename: "camera-page.js" });
  return {
    elements, cameras, locks, sockets, encodes, timers, document, window, draws,
    click(id) { elements[id].emit("click"); },
    tick(delay = 1000 / 24) {
      const pending = [...timers].filter(([, timer]) => timer.delay === delay);
      assert.equal(pending.length, 1, `expected one ${delay}ms timer`);
      const [id, timer] = pending[0];
      timers.delete(id);
      timer.callback();
    },
    async start(side = "environment") {
      this.click("start");
      await flush();
      const picked = stream(side);
      cameras.at(-1).resolve(picked);
      await flush();
      sockets.at(-1).open();
      return picked;
    },
  };
}

function blob(bytes = 3) {
  return { arrayBuffer: async () => new ArrayBuffer(bytes) };
}

async function stop(browser) {
  browser.click("start");
  await flush();
}

test("keeps the physical camera constraints, cover frame, 24 FPS, JPEG, and chosen-side labels", async () => {
  const b = browser();
  assert.equal(b.elements.start.textContent, "Start back camera");
  b.click("flip");
  assert.equal(b.elements.start.textContent, "Start front camera");
  const picked = await b.start("user");
  const constraints = b.cameras[0].constraints;
  assert.equal(constraints.audio, false);
  assert.equal(constraints.video.facingMode.exact, "user");
  assert.equal(constraints.video.width.ideal, 1280);
  assert.equal(constraints.video.height.ideal, 720);
  assert.equal(constraints.video.frameRate.ideal, 24);
  assert.equal(b.elements.canvas.width, 1280);
  assert.equal(b.elements.canvas.height, 720);
  assert.equal(b.elements.start.textContent, "Stop");
  assert.equal(b.elements.flip.textContent, "Switch to back");
  assert.equal(b.sockets[0].url, "wss://100.64.0.1:8443/ws");
  b.tick();
  assert.equal(b.encodes[0].type, "image/jpeg");
  assert.equal(b.encodes[0].quality, 0.7);
  b.encodes[0].callback(blob());
  await flush();
  assert.equal(b.sockets[0].sent.length, 1);
  assert.match(b.elements.status.textContent, /^Front camera · 1 frames$/);
  await stop(b);
  assert.equal(picked.track.stopped, 1);
  assert.equal(b.elements.start.textContent, "Start front camera");
});

test("rapid start, stop, start and flips serialize getUserMedia and stop superseded tracks", async () => {
  const b = browser();
  b.click("start");
  await flush();
  assert.equal(b.elements.start.textContent, "Cancel");
  b.click("start");
  b.click("start");
  b.click("flip");
  b.click("flip");
  b.click("flip");
  await flush();
  assert.equal(b.cameras.length, 1, "camera opens must not overlap");
  const stale = stream();
  b.cameras[0].resolve(stale);
  await flush();
  assert.equal(stale.track.stopped, 1);
  assert.equal(b.cameras.length, 2, "only the latest queued request opens a camera");
  assert.equal(b.cameras[1].constraints.video.facingMode.exact, "user");
  const latest = stream("user");
  b.cameras[1].resolve(latest);
  await flush();
  assert.equal(b.sockets.length, 1);
  assert.equal(b.timers.size, 1);
  assert.equal(b.elements.video.srcObject, latest);
  await stop(b);
  assert.equal(latest.track.stopped, 1);
  assert.equal(b.timers.size, 0);
});

test("stop during a pending camera request releases a late stream without starting transport", async () => {
  const b = browser();
  b.click("start");
  await flush();
  b.click("start");
  const late = stream();
  b.cameras[0].resolve(late);
  await flush();
  assert.equal(late.track.stopped, 1);
  assert.equal(b.elements.video.srcObject, null);
  assert.equal(b.sockets.length, 0);
  assert.equal(b.locks.length, 0);
  assert.equal(b.timers.size, 0);
  assert.equal(b.elements.status.textContent, "Idle");
});

test("backpressure skips encoding and drops frames queued during blob conversion", async () => {
  const b = browser();
  await b.start();
  const socket = b.sockets[0];
  socket.bufferedAmount = 1;
  b.tick();
  assert.equal(b.encodes.length, 0);
  socket.bufferedAmount = 0;
  b.tick();
  const converted = deferred();
  b.encodes[0].callback({ arrayBuffer: () => converted.promise });
  await flush();
  socket.bufferedAmount = 20;
  converted.resolve(new ArrayBuffer(10));
  await flush();
  assert.equal(socket.sent.length, 0);
  b.tick();
  assert.equal(b.encodes.length, 1);
  socket.bufferedAmount = 0;
  b.tick();
  b.encodes[1].callback(blob(100));
  await flush();
  assert.equal(socket.sent.length, 1);
  for (let index = 0; index < 20; index += 1) b.tick();
  assert.equal(b.encodes.length, 2, "one queued frame bounds transfer memory");
  assert.equal(socket.bufferedAmount, 100);
  await stop(b);
});

test("a stale toBlob callback neither sends into a restarted socket nor unlocks its encoder", async () => {
  const b = browser();
  await b.start();
  b.tick();
  const oldEncode = b.encodes[0];
  const oldSocket = b.sockets[0];
  await stop(b);
  await b.start();
  const newSocket = b.sockets[1];
  b.tick();
  assert.equal(b.encodes.length, 2);
  oldEncode.callback(blob());
  await flush();
  b.tick();
  assert.equal(b.encodes.length, 2, "old completion cannot clear the new encoding flag");
  assert.equal(oldSocket.sent.length, 0);
  assert.equal(newSocket.sent.length, 0);
  b.encodes[1].callback(blob());
  await flush();
  assert.equal(newSocket.sent.length, 1);
  await stop(b);
});

test("a stale arrayBuffer result cannot cross a stop or reconnect boundary", async () => {
  const b = browser();
  await b.start();
  b.tick();
  const converted = deferred();
  b.encodes[0].callback({ arrayBuffer: () => converted.promise });
  await flush();
  const oldSocket = b.sockets[0];
  oldSocket.close();
  b.tick(800);
  const replacement = b.sockets[1];
  replacement.open();
  converted.resolve(new ArrayBuffer(3));
  await flush();
  assert.equal(oldSocket.sent.length, 0);
  assert.equal(replacement.sent.length, 0);
  b.tick();
  const later = deferred();
  b.encodes[1].callback({ arrayBuffer: () => later.promise });
  await flush();
  await stop(b);
  await b.start();
  later.resolve(new ArrayBuffer(3));
  await flush();
  assert.equal(b.sockets[2].sent.length, 0);
  assert.equal(b.elements.status.textContent, "Back camera · connected");
  await stop(b);
});

test("stop clears frame/reconnect timers, tracks, preview, and acquired or late wake locks", async () => {
  const b = browser();
  const picked = await b.start();
  const lock = wakeLock();
  b.locks[0].resolve(lock);
  await flush();
  const oldSocket = b.sockets[0];
  oldSocket.close();
  assert.equal(b.timers.size, 2);
  await stop(b);
  assert.equal(picked.track.stopped, 1);
  assert.equal(lock.released, 1);
  assert.equal(b.elements.video.srcObject, null);
  assert.equal(b.timers.size, 0);
  oldSocket.onopen();
  oldSocket.onerror();
  oldSocket.onclose();
  assert.equal(b.elements.status.textContent, "Idle");
  assert.equal(b.elements.err.textContent, "");
  assert.equal(b.timers.size, 0);
  await b.start();
  await stop(b);
  const lateLock = wakeLock();
  b.locks[1].resolve(lateLock);
  await flush();
  assert.equal(lateLock.released, 1);
});

test("camera rejection and playback failure clean up and leave truthful retry controls", async () => {
  const b = browser();
  b.click("start");
  await flush();
  const denied = Object.assign(new Error("Camera permission was denied"), { name: "NotAllowedError" });
  b.cameras[0].reject(denied);
  await flush();
  assert.equal(b.cameras.length, 1);
  assert.equal(b.elements.start.textContent, "Start back camera");
  assert.match(b.elements.err.textContent, /permission was denied/);
  assert.equal(b.sockets.length, 0);
  b.elements.video.play = () => Promise.reject(new Error("Playback was blocked"));
  b.click("start");
  await flush();
  const picked = stream();
  b.cameras[1].resolve(picked);
  await flush();
  assert.equal(picked.track.stopped, 1);
  assert.equal(b.elements.video.srcObject, null);
  assert.equal(b.timers.size, 0);
  assert.match(b.elements.err.textContent, /Playback was blocked/);
  assert.equal(b.elements.start.textContent, "Start back camera");
});

test("virtual cameras are stopped and never begin transport", async () => {
  const b = browser();
  b.click("start");
  await flush();
  for (let index = 0; index < 3; index += 1) {
    const virtual = stream("environment", "iPhone Camera");
    b.cameras[index].resolve(virtual);
    await flush();
    assert.equal(virtual.track.stopped, 1);
  }
  assert.equal(b.sockets.length, 0);
  assert.match(b.elements.err.textContent, /virtual camera/);
  assert.equal(b.elements.start.textContent, "Start back camera");
});

test("encoding rejection is contained and the next frame can still be encoded", async () => {
  const b = browser();
  await b.start();
  b.tick();
  b.encodes[0].callback({ arrayBuffer: async () => { throw new Error("Blob conversion failed"); } });
  await flush();
  assert.match(b.elements.err.textContent, /Blob conversion failed/);
  b.tick();
  b.encodes[1].callback(blob());
  await flush();
  assert.equal(b.sockets[0].sent.length, 1);
  await stop(b);
});

test("page exit and native camera interruption release the current session", async () => {
  const b = browser();
  const first = await b.start();
  first.track.emit("ended");
  await flush();
  assert.equal(first.track.stopped, 1);
  assert.equal(b.timers.size, 0);
  assert.match(b.elements.err.textContent, /camera stopped/);
  const second = await b.start();
  b.window.emit("pagehide");
  await flush();
  assert.equal(second.track.stopped, 1);
  assert.equal(b.elements.video.srcObject, null);
  assert.equal(b.timers.size, 0);
});

test("cancelling pending video playback unblocks the next serialized camera start", async () => {
  const b = browser();
  const pendingPlay = deferred();
  b.elements.video.play = () => pendingPlay.promise;
  b.click("start");
  await flush();
  const first = stream();
  b.cameras[0].resolve(first);
  await flush();
  assert.equal(b.sockets.length, 0);
  b.click("start");
  b.elements.video.play = () => Promise.resolve();
  b.click("start");
  await flush();
  assert.equal(first.track.stopped, 1);
  assert.equal(b.cameras.length, 2);
  const second = stream();
  b.cameras[1].resolve(second);
  await flush();
  b.sockets[0].open();
  pendingPlay.reject(new Error("old playback aborted"));
  await flush();
  assert.equal(b.elements.video.srcObject, second);
  assert.equal(b.elements.err.textContent, "");
  assert.equal(b.elements.start.textContent, "Stop");
  assert.equal(b.timers.size, 1);
  await stop(b);
});

test("fallback cameras use actual facing when known and avoid claiming an unknown side", async () => {
  const b = browser();
  b.click("start");
  await flush();
  b.cameras[0].reject(new Error("exact facing unavailable"));
  await flush();
  assert.equal(b.cameras[1].constraints.video.facingMode.ideal, "environment");
  const front = stream("user");
  b.cameras[1].resolve(front);
  await flush();
  b.sockets[0].open();
  assert.equal(b.elements.status.textContent, "Front camera · connected");
  assert.equal(b.elements.flip.textContent, "Switch to back");
  await stop(b);
  assert.equal(b.elements.start.textContent, "Start front camera");
  b.click("start");
  await flush();
  b.cameras[2].reject(new Error("exact facing unavailable"));
  await flush();
  b.cameras[3].reject(new Error("ideal facing unavailable"));
  await flush();
  assert.equal(b.cameras[4].constraints.video.facingMode, undefined);
  b.cameras[4].resolve(stream(null));
  await flush();
  b.sockets[1].open();
  assert.equal(b.elements.status.textContent, "Camera · connected");
  await stop(b);
});

test("restarts await acquired wake-lock cleanup and late locks cannot release a new session", async () => {
  const b = browser();
  await b.start();
  const firstLock = wakeLock();
  const released = deferred();
  firstLock.release = () => { firstLock.released += 1; return released.promise; };
  b.locks[0].resolve(firstLock);
  await flush();
  b.click("start");
  b.click("start");
  await flush();
  assert.equal(b.cameras.length, 1, "cleanup completes before another camera opens");
  assert.equal(firstLock.released, 1);
  released.resolve();
  await flush();
  b.cameras[1].resolve(stream());
  await flush();
  b.sockets[1].open();
  const lateRequest = b.locks[1];
  await stop(b);
  await b.start();
  const currentLock = wakeLock();
  b.locks[2].resolve(currentLock);
  await flush();
  const lateLock = wakeLock();
  lateRequest.resolve(lateLock);
  await flush();
  assert.equal(lateLock.released, 1);
  assert.equal(currentLock.released, 0);
  assert.equal(b.elements.start.textContent, "Stop");
  await stop(b);
  assert.equal(currentLock.released, 1);
});
