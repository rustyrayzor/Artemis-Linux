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
- Remote-input AES keys are generated per launch and zeroed on drop.

## Decisions

1. The control plane is implemented in Rust; `moonlight-common-c` is retained
   initially for mature transport, FEC, RTSP, and input protocol behavior.
2. GStreamer supplies a dependable Linux software decode/audio baseline.
3. The initial H.264 SDR stereo profile set is intentionally limited to
   1080p60, 1440p60, and 4K60. Their bitrates follow Moonlight's defaults:
   20, 40, and 80 Mbps respectively.
4. Hardware acceleration is deferred until beta diagnostics can report GPU,
   driver, decoder, compositor, and negotiated stream details.
