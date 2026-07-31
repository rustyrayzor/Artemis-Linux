# Architecture

## Boundary map

```text
artemis-app
  UI state machine, GStreamer, Linux input adapters
       |                         |
       v                         v
artemis-core              artemis-moonlight
discovery, identity,      safe Rust session API
pairing, host HTTP,              |
apps, launch                     v
                          C ABI shim
                                |
                                v
                    pinned moonlight-common-c
```

`artemis-core` is portable and contains no native streaming dependency.
`artemis-moonlight` is the only crate allowed to call unsafe FFI. Its callbacks
copy borrowed native buffers into bounded Rust messages before returning.
`artemis-app` owns Linux presentation and physical input integration.

## Session state

```text
Idle -> Discovering -> HostSelected -> Pairing -> Paired
                                              -> ListingApps
                                              -> Launching
                                              -> Connecting
                                              -> Streaming
                                              -> Stopping -> Paired
Any operational state -> Error -> recoverable prior state
```

Only one `moonlight-common-c` connection may exist in a process. The FFI crate
enforces this because the upstream API is global and not thread-safe for
connection start/stop. A `Session` owns the native handle. Explicit `stop()`
reports errors; `Drop` provides best-effort cleanup during every other exit.

Native callbacks never block on rendering or audio. A bounded channel applies
backpressure by dropping late media packets rather than allowing unbounded
latency or memory growth.

## Security boundary

- The client RSA key and certificate are generated locally.
- Initial pairing uses HTTP as required by the protocol.
- The server certificate returned during pairing is verified by the
  challenge-response transcript before it is persisted.
- All paired commands use mutual TLS and compare the peer certificate byte for
  byte with the stored pin.
- Apollo `art://launch` links contain identifiers only. The client resolves the
  host UUID through the local paired-host store and confirms the application
  against the pinned host's current application list before launching.
- Remote-input AES keys are generated per launch and zeroed on drop.

## Decisions

1. The control plane is implemented in Rust; `moonlight-common-c` is retained
   initially for mature transport, FEC, RTSP, and input protocol behavior.
2. GStreamer supplies codec-specific AV1, HEVC, and H.264 parser/decoder
   pipelines plus the Linux audio baseline.
3. The active SDR profile set supports 720p, 1080p, 1440p, and 4K at 30 or 60
   FPS. Higher refresh rates remain represented in the protocol types for later
   work but are not exposed or accepted by this beta. Balanced bitrates come
   from the selected Moonlight table and are rounded up to a whole Mbps. High
   Quality LAN adds 25% to the unrounded source value, and Custom preserves a
   validated 1–300 Mbps override across other profile changes.
4. GStreamer preserves the negotiated dimensions and frame cadence through
   presentation. A bounded latest-frame queue drops decoded frames when the UI
   cannot keep up rather than building latency.
5. Automatic negotiation advertises advanced codecs only when the matching
   VA-API decoder is installed. Moonlight-common selects AV1, then HEVC, then
   H.264 from the advertised mask; explicit preferences retain safe fallbacks.
6. Main10 negotiation has an end-to-end gate: the selected hardware decoder
   must expose P010, EGL interop must be active, GStreamer must produce
   RGB10A2 GL memory, and the live window framebuffer must report at least ten
   bits per RGB channel. This prevents an advertised 10-bit stream from being
   silently truncated by an 8-bit presentation surface. A pinned eframe 0.31.1
   patch exposes the color-depth request and makes glutin prefer RGB10A2 while
   retaining an in-process 8-bit fallback.
7. Moonlight stereo and 5.1 Opus layouts are validated before pipeline creation.
   Six-channel output preserves Moonlight's mapping and supplies named
   `FL,FR,FC,LFE,RL,RR` PipeWire ports for deterministic HDMI routing.
8. HDR10 requests use HEVC Main10 or AV1 Main10 with BT.2020 negotiation. The
   C shim copies Moonlight's HDR mode and mastering metadata into owned Rust
   values, and GStreamer keeps PQ/BT.2020 tags through P010 hardware decode and
   RGB10A2 GL memory. `NativeHdrSurface` wraps eframe's borrowed Wayland surface,
   negotiates `color-management-v1`, and attaches a BT.2020/PQ image description
   with Moonlight mastering metadata. The dedicated Weston session transforms
   the source into the calibrated HDMI output volume and programs KMS with an
   AR30 framebuffer, BT.2020 RGB, and HDR static metadata. Weston owns vblank
   pacing in this session; the launcher disables the client EGL swap wait to
   avoid double synchronization at 4K60. If the compositor lacks the protocol
   or the display is not HDR-capable, the egui OpenGL callback applies a
   4,096-entry PQ EOTF lookup, BT.2020-to-BT.709 conversion, and a controlled SDR
   tone map instead.

The rationale and maintenance boundary for the eframe patch are recorded in
[`decisions/0001-rgb10a2-window-surface.md`](decisions/0001-rgb10a2-window-surface.md).
The HDR capability boundary and fallback are recorded in
[`decisions/0002-hdr-ingest-and-presentation.md`](decisions/0002-hdr-ingest-and-presentation.md).
The native HDR console path is recorded in
[`decisions/0003-native-hdr-weston-session.md`](decisions/0003-native-hdr-weston-session.md).
