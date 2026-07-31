# ADR 0002: Preserve HDR10 ingest and use an explicit SDR fallback

## Status

Accepted for the GNOME 46 fallback. Native console presentation is extended by
[ADR 0003](0003-native-hdr-weston-session.md).

## Context

Apollo and `moonlight-common-c` can negotiate HEVC Main10 or AV1 Main10 and report
the host HDR mode, BT.2020 colorspace, and Sunshine HDR10 mastering metadata. The
initial Artemis path selected Main10 but discarded those values at the C/Rust
boundary. It then copied PQ-encoded RGB into an ordinary compositor window. That
was neither a correct HDR presentation nor a controlled SDR conversion.

The reference HDMI display is HDR-capable: its EDID advertises SMPTE ST 2084,
BT.2020, static metadata type 1, deep color, and 4K60. The Intel DRM connector also
exposes `HDR_OUTPUT_METADATA`, BT.2020 colorspaces, and a 12-bpc maximum.

Ubuntu 24.04 uses GNOME 46 on Wayland. GNOME 46 does not expose the system-level
HDR color-management path needed by a normal application window, and the current
eframe/OpenGL renderer has no API for an HDR colorspace or mastering metadata on
its swapchain. Directly setting DRM connector properties would bypass the
compositor and is not safe for a desktop application.

## Decision

Artemis will:

1. Request BT.2020 and Main10 only when HDR is selected.
2. Carry the per-frame HDR flag, colorspace, and mastering metadata across the C
   FFI boundary.
3. Preserve P010/10-bit precision and BT.2020/PQ signaling through hardware
   decode and GL upload.
4. Detect the connected display's HDR capability separately from the active
   compositor/renderer capability.
5. On the Ubuntu 24.04 reference environment, tone-map HDR10 to an SDR sRGB
   window in a direct egui GPU paint callback and label the result
   `HDR source -> SDR tone map`. A PQ lookup texture avoids expensive per-pixel
   exponentiation, while direct painting avoids a redundant 4K intermediate
   texture pass.
6. Never report native HDR unless the application has an HDR-capable compositor
   protocol, colorspace-aware renderer/swapchain, and confirmed HDR output.
7. Keep 8-bit SDR as an explicit fallback when HDR negotiation, Main10 decode,
   display capability, or presentation setup is unavailable.

## Consequences

- HDR content remains usable and color-correct on the reference environment,
  without washed-out PQ output or silent clipping.
- The direct fallback presentation path can maintain 4K60 on the reference
  Intel GPU while leaving AV1/HEVC decode and the SDR path unchanged.
- The HDMI TV will not enter HDR mode under GNOME 46; diagnostics explain the
  compositor boundary instead of claiming success. The dedicated Weston session
  provides the native path without direct DRM manipulation from Artemis.
- HDR bitrate profiles may be enabled for HEVC/AV1 only after the ingest and
  fallback paths are active. H.264 remains SDR-only.
