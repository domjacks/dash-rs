/**
 * dash-rs WASM player — UI wiring and WASM initialisation.
 *
 * Responsibilities:
 *   - On-screen player log (console override + coloured spans)
 *   - WASM module initialisation (init())
 *   - URL input pre-fill from the ?url= query parameter
 *   - Form submit handler — creates a fresh bridge per session and calls play()
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

// 5. log(category, msg) — used by the bridge for explicit Web API call traces.
//    Writes directly to the panel with Web-API category colouring and uses
//    _console (original) so the console override above does NOT fire again.
export function log(category, msg) {
  _console.log(`[${category}] ${msg}`);
  appendToLog(`cat-${category.toLowerCase()}`, `[${category}] ${msg}`);
}

// ── WASM initialisation ────────────────────────────────────────────────────────

await init();
status.textContent = 'Ready. Enter a manifest URL and press Play.';

// ── Form handler ───────────────────────────────────────────────────────────────

form.addEventListener('submit', async (e) => {
  e.preventDefault();
  const url = input.value.trim();
  if (!url) return;

  // Clear the log from any previous playback session.
  logEl.textContent = '';

  status.className   = '';
  status.textContent = `Starting playback: ${url}`;

  try {
    // Create a fresh bridge for each playback session so MSE state is clean.
    const bridge = createBridge(video, log);

    // play() runs the full DASH fetch loop and resolves when EOS is reached.
    await play(url, bridge);
    status.textContent = 'Playback complete.';
  } catch (err) {
    status.className   = 'error';
    status.textContent = `Error: ${err}`;
    console.error('dash-rs error:', err);
  }
});
