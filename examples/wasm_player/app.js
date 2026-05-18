/**
 * dash-rs WASM player — UI wiring and WASM initialisation.
 *
 * Responsibilities:
 *   - On-screen player log (console override + coloured spans)
 *   - WASM module initialisation (init())
 *   - URL input pre-fill from the ?url= query parameter
 *   - Form submit handler — creates a fresh bridge per session and calls play()
 *   - Debug panel — updated every 500 ms via bridge.getLatestStatus()
 *
 * MSE bridge logic (all browser Web API calls made by dash-rs) lives in
 * player.js and is kept entirely separate from this file.
 */

import init, { play } from './pkg/wasm_player.js';
import { createBridge } from './player.js';

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

// 2. Low-level append — creates a coloured div (block) in the log panel.
function appendToLog(cssClass, text) {
  const line     = document.createElement('div');
  line.className = cssClass;
  line.textContent = text;
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

// 5. log(category, msg) — used by the bridge for explicit Web API call traces.
//    Writes directly to the panel with Web-API category colouring and uses
//    _console (original) so the console override above does NOT fire again.
export function log(category, msg) {
  _console.log(`[${category}] ${msg}`);
  appendToLog(`cat-${category.toLowerCase()}`, `[${category}] ${msg}`);
}

// ── Debug panel ────────────────────────────────────────────────────────────────
//
// updateDebugPanel() is called by a setInterval every 500 ms.  It reads:
//   • bridge.getLatestStatus() — the last JSON snapshot pushed from Rust.
//   • video.* — live browser properties for playback state and position.
//
// All DOM element references are looked up once here and reused each tick.

const dp = {
  dot:       document.getElementById('dp-dot'),
  stateText: document.getElementById('dp-state-text'),
  live:      document.getElementById('dp-live'),
  pos:       document.getElementById('dp-pos'),
  vq:        document.getElementById('dp-vq'),
  vbps:      document.getElementById('dp-vbps'),
  vbuf:      document.getElementById('dp-vbuf'),
  aq:        document.getElementById('dp-aq'),
  abps:      document.getElementById('dp-abps'),
  abuf:      document.getElementById('dp-abuf'),
  hc:        document.getElementById('dp-hc'),
  sc:        document.getElementById('dp-sc'),
  url:       document.getElementById('dp-url'),
  tms:       document.getElementById('dp-tms'),
  tbps:      document.getElementById('dp-tbps'),
};

// Current bridge — set when a session starts, cleared when it ends.
let activeBridge = null;

function fmtBps(bps) {
  if (bps === 0) return '—';
  if (bps >= 1_000_000) return `${(bps / 1_000_000).toFixed(2)} Mbps`;
  if (bps >= 1_000)     return `${(bps / 1_000).toFixed(1)} kbps`;
  return `${bps} bps`;
}

function fmtMs(ms) {
  if (ms === 0) return '—';
  return `${ms.toFixed(0)} ms`;
}

function updateDebugPanel() {
  // ── Playback state from the video element ────────────────────────────────
  let stateLabel, dotClass;
  if (video.ended) {
    stateLabel = 'Ended';     dotClass = 'dot-ended';
  } else if (video.paused) {
    stateLabel = 'Paused';    dotClass = 'dot-paused';
  } else if (video.readyState < 3) {
    stateLabel = 'Buffering'; dotClass = 'dot-buffering';
  } else {
    stateLabel = 'Playing';   dotClass = 'dot-playing';
  }
  dp.dot.className       = `dot ${dotClass}`;
  dp.stateText.textContent = stateLabel;
  dp.pos.textContent     = `${video.currentTime.toFixed(2)} s`;

  // ── Rust session status ──────────────────────────────────────────────────
  const s = activeBridge ? activeBridge.getLatestStatus() : null;
  if (!s) return;  // no tick yet — leave remaining rows as '—'

  dp.live.textContent  = s.live  ? 'Live'  : 'VOD';
  dp.vq.textContent    = s.vc > 0 ? `${s.vi + 1} / ${s.vc}` : '—';
  dp.vbps.textContent  = s.vc > 0 ? fmtBps(s.vbps) : '—';
  dp.vbuf.textContent  = s.vc > 0 ? fmtMs(s.vbuf)  : '—';
  dp.aq.textContent    = s.ac > 0 ? `${s.ai + 1} / ${s.ac}` : '—';
  dp.abps.textContent  = s.ac > 0 ? fmtBps(s.abps) : '—';
  dp.abuf.textContent  = s.ac > 0 ? fmtMs(s.abuf)  : '—';
  dp.hc.textContent    = String(s.hc);
  dp.sc.textContent    = String(s.sc);
  // Show only the last path component of the URL to keep the panel compact.
  dp.url.textContent   = s.url ? s.url.split('/').pop() || s.url : '—';
  dp.tms.textContent   = s.tms > 0 ? fmtMs(s.tms)  : '—';
  dp.tbps.textContent  = s.tbps > 0 ? fmtBps(s.tbps) : '—';
}

// Start the panel refresh loop immediately (it is safe to call before playback).
setInterval(updateDebugPanel, 500);

// ── WASM initialisation ────────────────────────────────────────────────────────

await init();
status.textContent = 'Ready. Enter a manifest URL and press Load.';

// ── Form handler ───────────────────────────────────────────────────────────────

form.addEventListener('submit', async (e) => {
  e.preventDefault();
  const url = input.value.trim();
  if (!url) return;

  // Clear the log from any previous playback session.
  logEl.textContent = '';

  status.className   = '';
  status.textContent = `Starting playback: ${url}`;

  // Reset debug panel rows to '—' between sessions.
  for (const el of Object.values(dp)) {
    if (el !== dp.dot && el !== dp.stateText) el.textContent = '—';
  }
  dp.stateText.textContent = '—';
  dp.dot.className = 'dot';

  // Forward browser seek events to the Rust fetch loop.
  // The 'seeking' event fires as soon as video.currentTime changes, before
  // the browser has loaded data at the new position.  Notifying here (rather
  // than on 'seeked') lets Rust redirect the segment cursor on the very next
  // fetch iteration, minimising the window where wrong segments are appended.
  const onSeeking = () => { if (activeBridge) activeBridge.notifySeek(video.currentTime); };
  video.addEventListener('seeking', onSeeking);

  try {
    // Create a fresh bridge for each playback session so MSE state is clean.
    const bridge = createBridge(video, log);
    activeBridge = bridge;

    // play() runs the full DASH fetch loop and resolves when EOS is reached.
    await play(url, bridge);
    status.textContent = 'Playback complete.';
  } catch (err) {
    status.className   = 'error';
    status.textContent = `Error: ${err}`;
    console.error('dash-rs error:', err);
  } finally {
    activeBridge = null;
    video.removeEventListener('seeking', onSeeking);
  }
});
