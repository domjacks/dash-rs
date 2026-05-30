# Architecture

```
DashSession
├── ManifestLoader ──── SegmentFetcher (trait) ←── StubSegmentFetcher
│       └── quick-xml + serde (pure-Rust MPD parser)   └── BridgeFetcher       (wasm_player)
│                                              └── HttpSegmentFetcher  (http_fetch example)
├── AbrController
│   ├── ThroughputRule
│   ├── BufferOccupancyRule
│   ├── InsufficientBufferRule
│   └── SwitchHistoryRule
└── MediaBuffer (trait) ←── NullBuffer            (headless / tests)
                         └── StubBuffer            (inspectable in-memory buffer)
                         └── BridgeMediaBuffer     (wasm_player — MSE via JS bridge)
                         └── GstMediaBuffer        (sink-gstreamer feature)
                         └── (your implementation)

GapController  (owned by DashSession; caller drives via session.tick_gap())
└── tick_gap(position_s, buffered, PlaybackState) → Option<GapSeekTarget>
    └── on gap/stall: caller seeks renderer, then calls session.seek_to()
        └── seek_to() automatically resets the gap controller (on_seek())
```

Data flow per segment:

1. `AbrController::select` picks a `QualityDecision` from `AbrMetrics`.
2. `DashSession` resolves the segment URL from the MPD `SegmentTemplate` for the current period.
3. `SegmentFetcher::fetch` retrieves the bytes.
4. `MediaBuffer::append(mt, data, is_init)` forwards the bytes to the decoder.
5. At period boundaries, `fetch_next_segment` transparently advances to the next `Period` and re-fetches init segments before continuing.
