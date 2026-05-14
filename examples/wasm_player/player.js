/**
 * dash-rs WASM player — MSE bridge.
 *
 * Exports createBridge(video, log) which returns the bridge object passed to
 * play() from the WASM module.  Every browser Web API call that dash-rs needs
 * goes through one of the methods on the returned object.
 *
 * Reading this file top-to-bottom shows the complete set of Web APIs involved
 * in MPEG-DASH playback.  UI wiring, the on-screen logger, and WASM
 * initialisation all live in app.js.
 *
 * WASM calls these methods via the JsBridge extern type in src/lib.rs.
 * Rust never touches browser objects directly.
 */

export function createBridge(video, log) {

  // MSE state — shared across all bridge methods via closure.
  let mediaSource;            // MediaSource instance
  let objectUrl;              // blob: URL handed to <video src>
  const sourceBuffers = {};   // opaque id (string) → SourceBuffer
  let nextSbId = 0;

  // Debug-panel state — latest status snapshot pushed from Rust each loop tick.
  let _latestStatus = null;

  return {

    // ── Clock ──────────────────────────────────────────────────────────────────

    // performance.now() — monotonic milliseconds since navigation start.
    nowMs() {
      return performance.now();
    },

    // Date.now() / 1000 — seconds since the Unix epoch.
    unixNowS() {
      return Date.now() / 1000;
    },

    // ── Utility ────────────────────────────────────────────────────────────────

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

    // Return the current playback position of the <video> element in seconds.
    // Passed to DashSession.current_metrics() so the ABR buffer-level
    // calculation uses the actual position rather than a hardcoded 0.0 (which
    // would make buffer_level_ms hugely wrong for epoch-timestamp live streams
    // where content sits at ~1.78e9 s in the browser timeline).
    getCurrentTime() {
      return video.currentTime;
    },

    // Return true while the video element is paused.
    // Passed to DashSession.tick_gap() so that gap jumps are suppressed while
    // the user has paused playback — mirrors dash.js GapController._shouldCheckForGaps().
    isPaused() {
      return video.paused;
    },

    // ── MediaSource lifecycle ──────────────────────────────────────────────────

    // MediaSource.isTypeSupported(mimeType) — static capability check.
    // Returns false for codecs the browser cannot decode (e.g. HEVC on Chrome).
    // dash-rs calls this before addSourceBuffer so unsupported tracks are
    // skipped gracefully rather than throwing a NotSupportedError.
    isTypeSupported(mimeType) {
      return MediaSource.isTypeSupported(mimeType);
    },

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

    // ── SourceBuffer operations ────────────────────────────────────────────────

    // Append raw fMP4 bytes to a SourceBuffer.
    // Returns a Promise that resolves when the browser fires 'updateend',
    // meaning the data has been parsed and is now in the decode pipeline.
    // appendBuffer() can throw synchronously (e.g. QuotaExceededError /
    // "buffer not sufficient") if the SourceBuffer is full, so the call is
    // placed *inside* the Promise constructor so the rejection stays in the
    // Promise chain and surfaces as a Rust DashError rather than an uncaught
    // JS exception.
    appendBuffer(id, data) {
      const sb = sourceBuffers[id];
      log('MSE', `sourceBuffers[${id}].appendBuffer(${data.byteLength} bytes)`);
      return new Promise((resolve, reject) => {
        try {
          sb.appendBuffer(data);
        } catch (e) {
          reject(e);
          return;
        }
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

    // ── SourceBuffer attributes ────────────────────────────────────────────────

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

    // ── Fetch API ──────────────────────────────────────────────────────────────

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

    // ── Debug panel ────────────────────────────────────────────────────────────

    // Called by Rust once per fetch-loop iteration with a compact JSON string
    // describing the current session state (built by build_status_json in lib.rs).
    // We parse it immediately so getLatestStatus() returns a plain object.
    reportStatus(json) {
      try { _latestStatus = JSON.parse(json); } catch (_) { /* ignore */ }
    },

    // Return the last parsed status object (or null before the first tick).
    // Called by the setInterval updater in app.js.
    getLatestStatus() {
      return _latestStatus;
    },

  };
}
