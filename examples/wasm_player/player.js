/**
 * dash-rs WASM player — browser bridge + UI logic.
 *
 * This file has two responsibilities:
 *
 *   1. **Bridge object** — every browser Web API call that dash-rs needs goes
 *      through one of the methods on `bridge`.  WASM calls these methods via
 *      the `JsBridge` extern type declared in `src/lib.rs`; Rust never touches
 *      browser objects directly.  Reading this file top-to-bottom shows the
 *      complete set of Web APIs involved in MPEG-DASH playback.
 *
 *   2. **UI wiring** — initialises the WASM module, pre-fills the URL input,
 *      and handles the Play form submission.
 *
 * Build the WASM module first (from the workspace root):
 *
 *   wasm-pack build examples/wasm_player --target web --out-dir examples/wasm_player/pkg
 *
 * Then serve this directory with any static file server and open index.html.
 */

import init, { play } from './pkg/wasm_player.js';

// ── DOM refs ───────────────────────────────────────────────────────────────────

const video  = document.getElementById('video');
const status = document.getElementById('status');
const form   = document.getElementById('form');
const input  = document.getElementById('url-input');
const logEl  = document.getElementById('api-log');

// Pre-fill URL from ?url= query parameter if present.
const searchUrl = new URLSearchParams(window.location.search).get('url');
if (searchUrl) input.value = searchUrl;

// ── On-screen logger ───────────────────────────────────────────────────────────
//
// All output — bridge Web API calls AND Rust tracing logs from WASM — appears
// in the on-screen panel.
//
// Strategy:
//   • Save the original console methods now, before overriding them.
//   • bridge calls use log(category, msg) which writes directly to the panel
//     (with Web-API category colours) and calls the *original* console method
//     so they are never double-written.
//   • console overrides capture everything else (tracing-wasm output, etc.)
//     and forward to the panel with severity-level colours.

// 1. Snapshot originals so log() can bypass the overrides.
const _console = {
  log:   console.log.bind(console),
  info:  console.info.bind(console),
  warn:  console.warn.bind(console),
  error: console.error.bind(console),
  debug: console.debug.bind(console),
};

// 2. Low-level append — creates a coloured span in the log panel.
function appendToLog(cssClass, text) {
  const line     = document.createElement('span');
  line.className = cssClass;
  line.textContent = text + '\n';
  logEl.appendChild(line);
  logEl.scrollTop = logEl.scrollHeight;
}

// 3. Format an args array from a console call into a plain string.
//    tracing-wasm uses "%c TEXT %c TARGET %c MSG" with CSS style strings
//    after the format string — strip the %c placeholders and skip the
//    corresponding style arguments so the text is readable on-screen.
function formatConsoleArgs(args) {
  if (!args.length) return '';
  const first = args[0];
  if (typeof first !== 'string') {
    return args.map(String).join(' ');
  }
  if (first.includes('%c')) {
    const styleCount = (first.match(/%c/g) || []).length;
    const text  = first.replace(/%c/g, '');
    const extra = args.slice(1 + styleCount).map(String).join(' ');
    return (text + (extra ? ' ' + extra : '')).trim();
  }
  return args.map(a => typeof a === 'string' ? a : String(a)).join(' ');
}

// 4. Override console methods — each calls the original (DevTools) AND the panel.
for (const [method, cssClass] of [
  ['log',   'level-log'],
  ['info',  'level-info'],
  ['warn',  'level-warn'],
  ['error', 'level-error'],
  ['debug', 'level-debug'],
]) {
  const orig = _console[method];
  console[method] = (...args) => {
    orig(...args);                                    // always visible in DevTools
    appendToLog(cssClass, formatConsoleArgs(args));   // mirror to panel
  };
}

// 5. log(category, msg) — for explicit bridge calls.
//    Writes directly to the panel with Web-API category colouring and uses
//    _console (original) so the console override above does NOT fire again.
function log(category, msg) {
  _console.log(`[${category}] ${msg}`);
  appendToLog(`cat-${category.toLowerCase()}`, `[${category}] ${msg}`);
}

// ── Bridge object ──────────────────────────────────────────────────────────────
//
// Every browser Web API call made by dash-rs goes through one of the methods
// below.  The implementations are intentionally explicit so that each line maps
// to a recognisable Web API name.
//
// WASM calls these methods via the JsBridge extern type in src/lib.rs.
// Rust never touches browser objects directly.

// MSE state — shared across all bridge methods via closure.
let mediaSource;            // MediaSource instance
let objectUrl;              // blob: URL handed to <video src>
const sourceBuffers = {};   // opaque id (string) → SourceBuffer
let nextSbId = 0;

const bridge = {

  // ── Clock ────────────────────────────────────────────────────────────────────

  // performance.now() — monotonic milliseconds since navigation start.
  nowMs() {
    return performance.now();
  },

  // Date.now() / 1000 — seconds since the Unix epoch.
  unixNowS() {
    return Date.now() / 1000;
  },

  // ── Utility ──────────────────────────────────────────────────────────────────

  // Resolves after `ms` milliseconds (used by the buffer-level gate loop).
  sleep(ms) {
    return new Promise(resolve => setTimeout(resolve, ms));
  },

  // Seek the <video> element to `time_s` seconds.
  // Required for live streams: fMP4 segments carry their native presentation
  // timestamp (potentially billions of seconds for BBC-style epoch streams)
  // so the video element must be seeked to where the buffered content lives.
  // Called once after session construction, before any segments are fetched.
  seekTo(time_s) {
    log('MSE', `video.currentTime = ${time_s.toFixed(3)}`);
    video.currentTime = time_s;
  },

  // ── MediaSource lifecycle ─────────────────────────────────────────────────────

  // 1. Instantiate a MediaSource.
  // 2. Create a blob: URL that points at it.
  // 3. Assign it to the <video> element — this kicks off browser setup.
  // 4. Wait for the 'sourceopen' event, which signals the MediaSource is
  //    ready to accept SourceBuffers.
  setupMediaSource() {
    mediaSource = new MediaSource();
    objectUrl   = URL.createObjectURL(mediaSource);
    video.src   = objectUrl;
    log('MSE', `new MediaSource() → readyState: "${mediaSource.readyState}"`);
    log('MSE', `URL.createObjectURL(mediaSource) → "${objectUrl}"`);
    log('MSE', `video.src = objectUrl`);

    return new Promise((resolve, reject) => {
      mediaSource.addEventListener('sourceopen', () => {
        log('MSE', `sourceopen — readyState: "${mediaSource.readyState}"`);
        resolve();
      }, { once: true });
      mediaSource.addEventListener('error', (e) => {
        reject(new Error(`MediaSource error: ${e}`));
      }, { once: true });
    });
  },

  // mediaSource.readyState  →  "open" | "ended" | "closed"
  getReadyState() {
    return mediaSource.readyState;
  },

  // mediaSource.duration
  getDuration() {
    return mediaSource.duration;
  },

  // mediaSource.duration = d
  setDuration(d) {
    log('MSE', `mediaSource.duration = ${d.toFixed(3)}`);
    mediaSource.duration = d;
  },

  // Register a new SourceBuffer for one track (video or audio).
  // Returns an opaque ID that identifies this buffer in subsequent calls.
  addSourceBuffer(mimeType) {
    const sb = mediaSource.addSourceBuffer(mimeType);
    const id = String(nextSbId++);
    sourceBuffers[id] = sb;
    log('MSE', `mediaSource.addSourceBuffer("${mimeType}") → id ${id}`);
    return id;
  },

  // Detach and discard a SourceBuffer by its opaque ID.
  removeSourceBuffer(id) {
    log('MSE', `mediaSource.removeSourceBuffer(sourceBuffers[${id}])`);
    mediaSource.removeSourceBuffer(sourceBuffers[id]);
    delete sourceBuffers[id];
  },

  // Signal that all segments have been appended; transitions to 'ended'.
  endOfStream() {
    log('MSE', `mediaSource.endOfStream()`);
    mediaSource.endOfStream();
  },

  // Release the blob: URL so the browser can free the MediaSource.
  close() {
    log('MSE', `URL.revokeObjectURL("${objectUrl}")`);
    URL.revokeObjectURL(objectUrl);
  },

  // ── SourceBuffer operations ───────────────────────────────────────────────────

  // Append raw fMP4 bytes to a SourceBuffer.
  // Returns a Promise that resolves when the browser fires 'updateend',
  // meaning the data has been parsed and is now in the decode pipeline.
  appendBuffer(id, data) {
    const sb = sourceBuffers[id];
    log('MSE', `sourceBuffers[${id}].appendBuffer(${data.byteLength} bytes)`);
    sb.appendBuffer(data);
    return new Promise((resolve, reject) => {
      sb.addEventListener('updateend', resolve, { once: true });
      sb.addEventListener('error',     reject,  { once: true });
    });
  },

  // Remove buffered data in the range [start, end) seconds.
  removeRange(id, start, end) {
    const sb = sourceBuffers[id];
    log('MSE', `sourceBuffers[${id}].remove(${start.toFixed(3)}, ${end.toFixed(3)})`);
    sb.remove(start, end);
    return new Promise((resolve, reject) => {
      sb.addEventListener('updateend', resolve, { once: true });
      sb.addEventListener('error',     reject,  { once: true });
    });
  },

  // Reset the SourceBuffer parser state mid-stream.
  abortBuffer(id) {
    log('MSE', `sourceBuffers[${id}].abort()`);
    sourceBuffers[id].abort();
  },

  // Switch the MIME type of an existing SourceBuffer without clearing data.
  changeType(id, mimeType) {
    log('MSE', `sourceBuffers[${id}].changeType("${mimeType}")`);
    sourceBuffers[id].changeType(mimeType);
  },

  // Returns the buffered ranges as a flat Float64Array: [s0,e0, s1,e1, …]
  getBuffered(id) {
    const ranges = sourceBuffers[id].buffered;
    const out    = [];
    for (let i = 0; i < ranges.length; i++) {
      out.push(ranges.start(i), ranges.end(i));
    }
    return new Float64Array(out);
  },

  // True while an appendBuffer or remove operation is in progress.
  isUpdating(id) {
    return sourceBuffers[id].updating;
  },

  // ── SourceBuffer attributes ───────────────────────────────────────────────────

  getTimestampOffset(id)      { return sourceBuffers[id].timestampOffset; },
  setTimestampOffset(id, v)   {
    log('MSE', `sourceBuffers[${id}].timestampOffset = ${v}`);
    sourceBuffers[id].timestampOffset = v;
  },

  getAppendWindowStart(id)    { return sourceBuffers[id].appendWindowStart; },
  setAppendWindowStart(id, v) {
    log('MSE', `sourceBuffers[${id}].appendWindowStart = ${v}`);
    sourceBuffers[id].appendWindowStart = v;
  },

  getAppendWindowEnd(id)      { return sourceBuffers[id].appendWindowEnd; },
  setAppendWindowEnd(id, v)   {
    log('MSE', `sourceBuffers[${id}].appendWindowEnd = ${v}`);
    sourceBuffers[id].appendWindowEnd = v;
  },

  // ── Fetch API ─────────────────────────────────────────────────────────────────

  // Download a segment (or manifest) and return its bytes.
  // `rangeHeader` is a "bytes=<start>-<end>" string for byte-range requests
  // (used by SegmentBase and SegmentList with @mediaRange), or undefined for
  // a full-file GET.
  async fetchBytes(url, rangeHeader) {
    const headers = {};
    if (rangeHeader) headers['Range'] = rangeHeader;

    const label = rangeHeader ? `${url} [${rangeHeader}]` : url;
    log('Fetch', `fetch("${label}", { mode: 'cors' })`);

    const t0       = performance.now();
    const response = await fetch(url, { mode: 'cors', headers });

    if (!response.ok) {
      throw new Error(`HTTP ${response.status} ${response.statusText}: ${url}`);
    }

    // Resolve the final URL (may differ from request URL after redirects).
    const finalUrl = response.url || url;

    // Read the entire body into an ArrayBuffer, then wrap in Uint8Array.
    const arrayBuffer = await response.arrayBuffer();
    const data        = new Uint8Array(arrayBuffer);
    const durationMs  = performance.now() - t0;

    log('Fetch', `  → ${data.byteLength} bytes in ${durationMs.toFixed(0)} ms`);

    // Return a plain object; Rust reads its properties via the FetchResult
    // extern type accessors (getter attributes in the extern "C" block in lib.rs).
    return { data, finalUrl, durationMs };
  },

  // HEAD request to test whether a URL is reachable (used by XLink probing).
  async checkExists(url) {
    log('Fetch', `fetch("${url}", { method: 'HEAD', mode: 'cors' })`);
    const response = await fetch(url, { method: 'HEAD', mode: 'cors' });
    return response.ok;
  },
};

// ── WASM initialisation ────────────────────────────────────────────────────────

await init();
status.textContent = 'Ready. Enter a manifest URL and press Play.';

// ── Form handler ───────────────────────────────────────────────────────────────

form.addEventListener('submit', async (e) => {
  e.preventDefault();
  const url = input.value.trim();
  if (!url) return;

  // Clear the API log from any previous playback.
  logEl.textContent = '';

  status.className   = '';
  status.textContent = `Starting playback: ${url}`;

  try {
    // play() runs the full DASH fetch loop and resolves when EOS is reached.
    // It receives the bridge object so that every Web API call it needs
    // can be seen in plain JavaScript above.
    await play(url, bridge);
    status.textContent = 'Playback complete.';
  } catch (err) {
    status.className   = 'error';
    status.textContent = `Error: ${err}`;
    console.error('dash-rs error:', err);
  }
});
