# DASH Spec Compliance Tracker

A living checklist tracking `dash-rs`'s conformance to **ISO/IEC 23009-1** (MPEG-DASH Part 1)
and the **DASH-IF Interoperability Points** (IOP) guidelines.

Items are grouped by priority:

| Symbol | Meaning |
|---|---|
| ✅ | Implemented and tested |
| 🔴 P0 | Blocks real-world streams — fix immediately |
| 🟠 P1 | Affects live-stream quality or important DASH profiles |
| 🟡 P2 | Robustness, edge cases, optional profiles |

---

## ✅ Completed

All items below are implemented and covered by tests.

| # | Item | Spec ref |
|---|---|---|
| 1 | `SegmentTimeline r=-1` infinite-repeat walk — arithmetic snapping `(target − t) % d == 0`; bounded by next `S@t` | §5.3.9.6.4 |
| 2 | `timestampOffset = Period@start − presentationTimeOffset/timescale` | §5.3.9.6 |
| 3 | `period_start_s` stored in `TrackState`; `seek_to` uses `position − period_start_s` for segment-number formula | §5.3.9.6 |
| 4 | `refresh_manifest` re-snaps live edge; advances `segment_number` / `initial_segment_number` when behind | §5.4.4 |
| 5 | `SegmentList@startNumber` honoured; array index = `seg_num − start_number` | §5.3.9.3 |
| 6 | `$Number$` substitution in Timeline mode passes `seg_num` (was 0) | §5.3.9.4 |
| 7 | `MPD@profiles` parsed | §5.3.1.2 |
| 8 | `MPD@minBufferTime` seeds `buffer_target_ms`; `min_buffer_time_ms()` accessor | §5.3.1.2 |
| 9 | `<Initialization>` child element takes priority over `@initialization` attribute | §5.3.9.5 |
| 10 | `should_refresh_manifest`: absent `@minimumUpdatePeriod` falls back to one segment duration | §5.4 |
| 11 | `AdaptationSet@codecs` parsed; `adaptation_mime_type` checks AS-level codec first, then all Representations | §5.3.7.2 |
| 12 | `refresh_manifest` resets `next_segment_time` when it has slid out of the new live SegmentTimeline | §5.4.4 |
| 13 | `BaseURL` document order preserved for equal-weight tiers; random permutation only when weights differ | §5.10.1.3 / DASH-IF IOP §6.7 |
| 14 | `MPD@timeShiftBufferDepth` parsed; `fetch_next_segment` advances `segment_number` past DVR floor; `time_shift_buffer_depth_ms()` accessor | §5.3.1.2 / §5.4.3 |
| 15 | HTTP 404 on live segment: CDN not blacklisted; `fetch_next_segment` waits one segment duration and retries once | §5.4.4 |
| 16 | `current_metrics` buffer level = `(buffered_end − position_s).max(0) × 1000 ms`; `position_s: f64` parameter added | — |
| 17 | Multi-period playback: `try_advance_period` transparent at period boundaries; `seek_to` uses `period_start_s` | §5.3.2.1 |
| 18 | XLink `onLoad` resolution for `<Period>` and `<AdaptationSet>` | §5.5 |
| 19 | `SegmentBase` (byte-range) with `sidx` lazy fetch | §5.3.9.2 |
| 20 | `SegmentList` with `<SegmentURL>` elements | §5.3.9.3 |
| 21 | `SegmentTemplate` `$Number$` and `$Time$` addressing | §5.3.9.4 |
| 22 | `UTCTiming` schemes: `http-xsdate`, `http-iso`, `direct` | §5.8.5 |
| 23 | Multiple audio `AdaptationSet`s: highest-bandwidth tier wins | DASH-IF IOP §4.3 |
| 24 | CDN blacklisting with configurable cooldown; all-blacklisted override | DASH-IF IOP §6.7 |
| 25 | Gap / stall detection and jump via `GapController` | — |
| 26 | In-band DASH `emsg` events broadcast via `subscribe_events` | §5.10.3 |
| 27 | `ServiceDescription` target latency and playback rate range parsed | §5.14 |
| 28 | Live-edge snapping from `availabilityStartTime` + `suggestedPresentationDelay` | §5.3.1.2 |
| 29 | BaseURL multi-level resolution (MPD → Period → AdaptationSet → Representation) | §5.10.1.3 |
| 30 | `MPD@suggestedPresentationDelay` overrides default safety margin | §5.3.1.2 |
| 31 | `MPD@availabilityEndTime` enforced: `fetch_next_segment` returns `EndOfStream` when wall clock passes the declared end time; `availability_end_time_s()` accessor added | §5.3.1.2 |
| 32 | UTCTiming `http-head` scheme: `SegmentFetcher::head_date` default method + `parse_rfc2822_date` built-in parser; BBC IPTV and DVB streams no longer silently skip clock sync | §5.8.5.7 |
| 33 | `seek_to` re-anchors `next_segment_time` for SegmentTimeline tracks: `TrackState.timescale` stored; ticks computed as `round((pos − period_start) × timescale)` | §5.3.9.4 |
| 34 | `time_shift_buffer_depth_ms` re-parsed on every `refresh_manifest` so DVR window changes in live manifests are picked up immediately | §5.3.1.2 / §5.4.3 |
| 35 | `period_start_s` re-derived from current period on `refresh_manifest` so `seek_to` uses correct value after live multi-period manifest updates | §5.3.2.1 |
| 36 | `SegmentTemplate@availabilityTimeOffset` parsed and propagated; `compute_live_edge_segment` adds ATO to elapsed time so low-latency CMAF streams can access partially-produced segments at the live edge; `@availabilityTimeComplete=false` parsed with a warning (chunked transfer not yet supported) | §5.3.9.4 / DASH-LL |
| 37 | `prft` (ProducerReferenceTimeBox) parsed from fMP4 segments; `DashSession::last_producer_reference_time()` accessor; wall-clock NTP→Unix conversion built-in; enables end-to-end latency measurement for low-latency live streams | §8.10.5 (ISO 14496-12) |
| 38 | Throughput model upgraded to dual-EWMA (fast 3 s + slow 8 s half-lives, time-based α) + harmonic-mean over last 4 samples; estimate is min(harmonic, fast_ema, slow_ema) — conservative, resists bursty outliers; matches dash.js `ThroughputController.js` strategy | — |
| 39 | `MediaSource::is_type_supported(mime)` default method (returns `true`); `from_mpd_with_utc` checks it before `add_source_buffer`; unsupported video track is skipped with a `warn!` and the session falls back to audio-only rather than failing; `BridgeMediaSource` in WASM player delegates to `MediaSource.isTypeSupported()` | — |
| 40 | Audio `AdaptationSet` tie-breaking: when two sets have equal maximum bandwidth the one that appears first in document order is preferred (main audio before alternate / audio-description); `find_adaptation` uses enumerated index as a secondary descending key in `max_by_key` | DASH-IF IOP §4.3 |
| 41 | WASM player `play()` buffer gate uses whichever track is active (`Video` if present, else `Audio`) so audio-only fallback sessions are back-pressured correctly | — |
| 42 | Live SegmentTimeline (`r=-1`) live-edge snap: `snap_to_timeline_grid` snaps the computed live-edge tick target down to the nearest grid-aligned entry; `next_segment_time` is set during session construction so the first fetch lands at the live edge, not at the beginning of the time-shift window | §5.3.9.6.4 |
| 43 | `live_presentation_start_s()` returns the actual snapped `next_segment_time / timescale` for SegmentTimeline tracks, ensuring `seekTo()` in the WASM player seeks to the exact first-segment PTS rather than an approximation from the `$Number$` formula | §5.3.9.4 |
| 44 | WASM player `current_metrics` now passes `video.currentTime` (via `getCurrentTime()` bridge) as `position_s` instead of a hardcoded `0.0`, fixing the hugely inflated `buffer_level_ms` for epoch-timestamp live streams (BBC IPTV) where content sits at ~1.778×10⁹ s | — |
 | 45 | `DashSession::apply_live_pts_remap()` — opt-in method that shifts `SourceBuffer.timestampOffset` on all active sinks by `-(initial_segment_number − start_number) × seg_dur_s` (Number) or `-(next_segment_time / timescale)` (Timeline) so that epoch-scale fMP4 content lands near presentation time 0 in the browser; `live_presentation_start_s()` returns ≈ 0 after the call; WASM player calls this automatically before the fetch loop on live streams; GStreamer players must NOT call it (they use probe-based PTS subtraction via `set_live_mode(true)`); fixes the Safari stall where `video.currentTime` is reset to 0 before content is buffered at the live-edge PTS | — |
 | 46 | `DashSession::seek_to` corrected for remapped live streams: `position_s` received from the browser is in remapped (≈0-based) time; the segment-number formula now adds `live_pts_remap_offset_s` back before dividing, and the returned snapped value subtracts it, so both the segment number and the caller's seek target are correct; backward-compatible because offset is 0.0 when `apply_live_pts_remap` has not been called | — |
 | 47 | Callers access buffered time ranges directly via `MediaBuffer::buffered(media_type)` and pass them to `DashSession::tick_gap`; `DashSession` does not store the buffer so it cannot expose a `buffered()` accessor directly | — |
 | 48 | WASM player `tick_gap` loop integrated: at every fetch iteration `session.tick_gap(position_s, &buffered, state)` is called with the live `video.currentTime` and playback state (via bridge); when a seek is recommended the player calls `bridge.seek_to`, `session.seek_to`, and `session.notify_playing()` — mirrors dash.js `GapController.js` | — |
 | 49 | `extract_segment_timing` no longer sets `total_segments` from a SegmentTimeline that contains an `r=-1` (infinite-repeat) entry; such timelines are unbounded and EOS must be determined by `availabilityEndTime` or segment fetch failure, not by segment count — fixes premature `EndOfStream` on first fetch of any live `r=-1` stream | §5.3.9.6.4 |
 | 50 | Formal metrics collection (`DashMetrics`) — three bounded ring-buffer lists (cap 100, matching dash.js `MAX_SAVED_METRICS_REQUESTS`): `HttpRequestRecord` (per `fetch_next_segment` call: wall_ms, segment_number, url, bytes_received, transfer_duration_ms, throughput_bps, quality_index), `RepresentationSwitchRecord` (per quality switch: wall_ms, from_index, to_index, reason), `BufferLevelSample` (per fetch at ABR decision time: wall_ms, level_ms); exposed via `DashSession::metrics()` (`&DashMetrics`); timestamps are absolute Unix milliseconds; module at `src/session/metrics.rs` | dash.js `DashMetrics.js` |
 | 51 | `DroppedFramesRule` — downgrades quality when the ratio of dropped frames to total frames exceeds a configurable threshold; implemented in `src/abr/dropped_frames.rs`; registered in `AbrController::default()`; covered by tests in `tests/abr_rules.rs` | dash.js `DroppedFramesRule.js` |
 | 52 | `AbandonRequestsRule` — abandons a slow segment fetch and drops quality when the projected completion time would exceed a multiplier of the remaining segment duration; implemented in `src/abr/abandon_requests.rs`; registered in `AbrController::default()`; covered by tests in `tests/abr_rules.rs` | dash.js `AbandonRequestsRule.js` |
 | 53 | `SwitchHistoryRule` — imposes a cooldown after a quality switch to prevent oscillation; clock-injected via `AbrMetrics::now_ms` (sourced from `Clock::now_ms()`) so the cooldown works correctly on `no_std` targets; implemented in `src/abr/switch_history.rs`; registered in `AbrController`; covered by tests in `tests/abr_rules.rs` | dash.js `SwitchHistoryRule.js` |
 | 54 | `DashSession::set_max_bitrate` / `clear_max_bitrate` / `max_bitrate` — per-track ABR bitrate cap; implemented in `src/session/mod.rs`; tested in `tests/session.rs` | — |
 | 55 | `PlaybackDriver::enable_pruning` — opt-in buffer pruning behind the playback position; `run()` calls `prune_all_tracks` each iteration when enabled; implemented in `src/session/driver.rs` | — |
 | 56 | `PlaybackDriver::on_quality_change` — callback fired by `run()` when the ABR controller switches quality; carries previous index, new index, and `AbrDecisionReason`; implemented in `src/session/driver.rs` | — |
 | 57 | `PlaybackDriver::pending_seek_s` / `has_pending_seek` — destructive / non-destructive user-seek API; `run()` calls `pending_seek_s()` each iteration and interrupts buffer-gate waits via `has_pending_seek()`; implemented in `src/session/driver.rs` | — |

---

## 🔴 P0 — Blocks real-world streams

All Tier 1 / P0 items are now implemented. No outstanding items.

## 🟠 P1 — Important for live streams and ABR quality

### P1-4: Low-latency partial / chunked segments not supported

**Gap**: `FetchResponse` holds complete `Bytes`.  Progressive byte delivery (required for `@availabilityTimeComplete=false` CMAF) is not possible with the current trait.

**Fix**:
- Add a `Stream<Item=Result<Bytes, FetchError>>` body variant to `FetchResponse`, or a second `fetch_streaming` method on `SegmentFetcher`.
- Gate behind a Cargo feature (`fetch-ll`) so it does not add dependencies to the default build.

**Files**: `src/fetch/mod.rs`

---

### P1-6: Catch-up playback rate control missing

**Gap**: For live streams with a target latency, `CatchupController.js` (dash.js) adjusts playback rate to converge.  `ServiceDescription` latency and rate are parsed (`target_latency_ms`, `playback_rate_range`) but nothing drives the renderer rate.

**Fix**:
1. Add `set_playback_rate(rate: f64)` to the `MediaBuffer` trait.
2. `DashSession` computes `rate = f(actual_latency, target_latency, playback_rate_range)` and calls it.

**Files**: `src/media_buffer.rs`, `src/session/mod.rs`

---

### P1-7: Content steering (`<ContentSteering>`) not implemented

**Gap**: DASH-IF Content Steering (and §5.3.13) defines a steering server that dynamically orders CDN candidates.  dash-rs uses static document-order / weight-based ordering.

**Fix**: Parse `<ContentSteering serverURL="..." defaultServiceLocation="..." queryBeforeStart="..."/>`.  On session start and periodically, fetch the steering manifest and apply the ordered service-location list.

**Files**: new `src/session/steering.rs`, `src/session/mod.rs`

---

### P1-8: MPD Patch (`<PatchLocation>`) not implemented

**Gap**: §5.15 defines `<PatchLocation>` for incremental live manifest updates using XPath patch operations.  `refresh_manifest` always fetches the full MPD, which increases bandwidth for large manifests.

**Fix**: Parse `<PatchLocation>` from the manifest.  In `refresh_manifest_if_due`, if a patch URL is present, fetch and apply the patch document rather than re-fetching the full MPD.

**Files**: `src/session/mod.rs`, new `src/session/patch.rs`

---

### P1-9: L2ARule (low-latency Lyapunov ABR) missing

**Gap**: For low-latency streams, the throughput and buffer rules perform poorly.  dash.js uses `L2ARule.js` based on Lyapunov optimisation.

**Fix**: Implement in `src/abr/l2a.rs`; register in `AbrController::default()`.

---

### P1-10: LoLpRule (learning-based low-latency ABR) missing

**Gap**: `LoLpRule.js` / `LearningAbrController.js` in dash.js.  Requires a QoE evaluator and an online weight-selector.

**Fix**: Implement in `src/abr/lolp.rs`; deferred until L2A is done.

---

### P1-11: `<EssentialProperty>` unknown scheme silently ignored (non-conformant)

**Gap**: §5.8.4 — when a client encounters an `<EssentialProperty>` element whose `@schemeIdUri` it does not recognise it **must not play** that Representation or AdaptationSet.  dash-rs currently ignores unknown `EssentialProperty` elements and plays the stream regardless, which is non-conformant.

**Fix**: Parse `EssentialProperty` elements on `AdaptationSet` and `Representation`.  In `find_adaptation` / `extract_representations`, return `DashError::UnsupportedEssentialProperty(scheme_uri)` for any unknown scheme.  Add a registry of well-known schemes that the library handles (e.g. HDR, accessibility roles) and skip the error for those.

**Files**: `src/error.rs` (new variant), `src/session/mpd.rs`

---

### P1-12: `<ContentProtection>` parse and pssh extraction missing

**Gap**: `<ContentProtection>` elements are not parsed.  No `pssh` box extraction from init segments.  Without this, callers cannot implement key-system initialisation (Widevine, PlayReady, FairPlay) even if they have their own CDM.

**Fix**:
1. Parse `ContentProtection` descriptors on `Representation` / `AdaptationSet` and expose them on `TrackState` (scheme URI, value string, `default_KID`).
2. Extract `pssh` boxes from init segments and expose via a `DashSession::pssh_for_track(media_type)` accessor.
3. CDM/EME integration (key requests, licence exchange) belongs in examples.

**Files**: `src/manifest/parser.rs`, `src/session/mod.rs`, `src/mp4.rs`

---

### P1-13: Subtitle / text tracks missing

**Gap**: Text `AdaptationSet`s (TTML, WebVTT, IMSC) are ignored at construction.  Mandatory for accessibility (WCAG 2.1, EU AVMSD, Ofcom, FCC).

**Fix**: Add `MediaType::Text`.  In `from_mpd`, create a third `TrackState` for text.  A `TextParser` trait with TTML and WebVTT implementations is needed; the scheduling path alone (without a parser) would unblock third-party text renderers.

**Files**: `src/session/mod.rs`, `src/media_buffer.rs` (new `MediaType` variant), new `src/text/`

---

## 🟡 P2 — Robustness and additional profiles

### P2-1: `Representation@qualityRanking` not parsed

**Gap**: §5.3.5.2 `@qualityRanking` is an ordering hint for representations of equal bandwidth.

**Fix**: Parse `qualityRanking: Option<u64>` in `Representation`.  Use as secondary sort key after bandwidth in `extract_representations`.

---

### P2-2: `AdaptationSet@bitstreamSwitching` not honoured

**Gap**: §5.3.3.2 — when `true`, the encoder guarantees seamless random-access switching between representations without re-sending the init segment.  dash-rs always re-sends the init segment on quality switch.

**Fix**: Parse `bitstreamSwitching: Option<bool>`.  When `true`, skip the init-segment re-append in `fetch_next_segment_inner`.

---

### P2-3: `AdaptationSet@subsegmentAlignment` not checked

**Gap**: §5.3.3.2 — guarantees that subsegment boundaries across tracks align (relevant for gapless A/V sync).  Not parsed or enforced.

---

### P2-4: `AdaptationSet@group` coordination not implemented

**Gap**: §5.3.3.2 `@group` allows adaptive switching across multiple `AdaptationSet` elements of the same media type.  dash-rs selects exactly one set per media type at construction time.

---

### P2-5: `<SupplementalProperty>` not parsed

**Gap**: §5.8.5 — `<SupplementalProperty>` elements are optional descriptor extensions used for HDR signalling, codec parameters, etc.  They are currently silently ignored.

**Fix**: Parse as a generic `Vec<Descriptor>` (scheme URI + value string) on `AdaptationSet` and `Representation`.  Expose via a `descriptors()` accessor.

**Note**: `<EssentialProperty>` (§5.8.4) is a stricter variant — unknown essential properties mandate playback refusal and have been promoted to P1-11.

---

### P2-6: `<SubRepresentation>` not parsed

**Gap**: §5.3.6 describes embedded streams (thumbnail sprites, embedded audio).

---

### P2-7: `MPD@publishTime` not parsed

**Gap**: Used by some implementations for cache invalidation on manifest refresh.  Not parsed.

---

### P2-8: `XLink onRequest` deferral not implemented

**Gap**: `xlink:actuate="onRequest"` means the linked content is fetched on demand (e.g. when a Period is about to start).  Currently logged and skipped; `onLoad` is implemented.

---

### P2-10: DVB/ISO metrics reporting missing

**Gap**: `MetricsReporting.js` / `DVBReporting` — periodic HTTP posting to a `@dvb:metrics` reporting server.  Requires formal metrics collection (implemented — see item 50 in Completed).

---

### P2-11: CMCD request parameters not attached

**Gap**: Common Media Client Data (CTA-5004) query parameters.

**Fix**: Gate behind a `cmcd` Cargo feature.  Build CMCD params from `AbrMetrics` + session state; attach to `FetchRequest`.

---

### P2-12: CMSD response header parsing missing

**Gap**: Common Media Server Data (CTA-5006) in `CMSD-Dynamic` / `CMSD-Static` response headers.

**Fix**: Gate behind a `cmsd` feature.  Parse from `FetchResponse` headers (requires adding a `headers` field to `FetchResponse`).

---

### P2-13: DRM / CDM-EME integration missing

**Gap**: CDM and EME integration (key requests, licence exchange, key-system selection) is not implemented.  Platform EME/CDM integration belongs in examples.

**Note**: `<ContentProtection>` parsing and `pssh` extraction have been promoted to P1-12 as the foundation required by all DRM-protected streams.

---

### P2-14: Per-representation codec capability filtering missing

**Gap**: `DashSession::from_mpd` now skips entire tracks whose MIME type is not supported by the `MediaSource` (via `is_type_supported`), but does not filter *individual representations* within a track.  A manifest that mixes supported and unsupported codec variants within one `AdaptationSet` (e.g. `avc1` and `hev1` representations in the same video set) will include all representations in the ABR list, and the fetcher may attempt to append unsupported codec data to the source buffer.

**Fix**: After `compute_track_reinit` builds the representation list, call `is_type_supported` for each representation's full MIME+codec string and remove those that return `false`.  Only skip the whole track if the filtered list is empty.

**Files**: `src/session/mpd.rs` (`compute_track_reinit`), `src/session/mod.rs` (`from_mpd_with_utc`)

---

### P2-15: Thumbnails / trick-play missing

**Gap**: `role=thumbnail` `AdaptationSet` not parsed.

**Fix**: Add `DashSession::thumbnail_at(time_s) -> Option<ThumbnailTile>` returning sprite-sheet URL + crop rect.

---

### P2-16: Common Access Token (CAT) missing

**Gap**: ETSI TS 103 485 `commonAccessToken` JWT not attached to segment request headers.

**Fix**: Add `cat_token: Option<String>` to `FetchRequest`; add `DashSession::set_cat_token(token)`.

---

### P2-17: Extended URL Query Info (`<ExtendedUrlQueryInfo>`) missing

**Gap**: Appends service-specific query parameters to segment URLs at fetch time (CDN token auth).

**Fix**: Parse the element in `from_mpd`; append parameters in `expand_template` / `resolve_effective_base_url`.

---

### P2-18: `SegmentBase` byte-range `SegmentList` entries (`@mediaRange`) missing

**Gap**: `resolve_segment_url` returns `DashError::SegmentUrlUnresolvable` for `<SegmentURL mediaRange="…">` entries.

**Fix**: Parse `mediaRange` as a `(u64, u64)` byte range and attach it to `FetchRequest::byte_range`.

---

### P2-19: Offline download not implemented

**Gap**: No download coordinator that writes segments to a directory with a manifest index.

**Scope**: Out of scope until P1 items are complete.
