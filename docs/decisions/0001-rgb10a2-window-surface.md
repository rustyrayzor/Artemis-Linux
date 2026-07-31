# ADR 0001: Request RGB10A2 through a pinned eframe patch

## Status

Accepted on July 31, 2026.

## Context

Artemis can negotiate HEVC Main10 and AV1 Main10, decode P010 with VA-API, and
convert the result to an RGB10A2 OpenGL texture. Upstream eframe 0.31.1 asks
glutin for an 8-bit RGBA window configuration and does not expose a color-depth
option. That final 8-bit surface prevented honest end-to-end Main10 output.

## Decision

Pin a source copy of eframe 0.31.1 under `vendor/eframe` and add one optional
`NativeOptions::color_buffer_bits` field for the glow renderer. When Artemis
requests ten bits, the glutin picker enumerates configurations compatible with
the normal 8-bit minimum, prefers an RGB configuration with at least ten bits
per color channel, and retains the first 8-bit configuration as a fallback.

Artemis still measures the live framebuffer after context creation. Main10 is
advertised only when that measurement is at least ten bits and the existing
decoder, EGL, P010, and RGB10A2 checks also pass.

## Consequences

- UbuntuLab produces a genuine 10/10/10/2 EGL surface without replacing egui or
  the zero-copy GStreamer path.
- Machines without a deep-color configuration continue to start in 8-bit mode.
- eframe upgrades require reapplying or retiring this small patch after checking
  whether upstream exposes an equivalent option.
- This enables SDR Main10 presentation. HDR still requires Moonlight HDR
  metadata propagation, BT.2020/PQ handling, compositor signaling, and validated
  display behavior; those are separate changes.

## Alternatives considered

- A Vulkan/libplacebo renderer offers the strongest long-term HDR path but is a
  much larger change and is not required to unblock Main10.
- Replacing the full UI renderer would duplicate working eframe window and input
  integration.
- Advertising Main10 on the former 8-bit surface was rejected because it would
  silently discard precision.
