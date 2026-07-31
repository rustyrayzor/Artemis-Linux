# ADR 0003: Use a dedicated Weston session for native HDR10

## Status

Accepted and validated on UbuntuLab.

## Context

The Hisense HDMI display and Intel Alder Lake-N DRM connector support ST 2084,
BT.2020 RGB, HDR static metadata, and deep color at 3840x2160 60 Hz. GNOME 46 on
Ubuntu 24.04 does not expose `color-management-v1`, so a normal desktop window
cannot safely declare its BT.2020/PQ content. Upgrading the host OS would also
unnecessarily risk the unrelated Docker/Coolify workloads on the reference
server.

Weston 15.0.1 exposes the standard Wayland color-management protocol and can
program the KMS HDR properties, but its LCMS implementation still creates the
stock sRGB fallback as an ICC profile while ICC-to-parametric transforms are not
implemented. That prevents a parametric PQ output from enabling. Its source
already identifies converting the stock profile to parametric form as the
intended fix.

## Decision

1. Install Weston, libdisplay-info, and Wayland protocols side-by-side below
   `/opt/artemis-hdr`; do not replace Ubuntu's compositor packages.
2. Apply the minimal stock-sRGB patch in `packaging/weston-patches` so all
   default-sRGB-to-PQ transforms use the implemented parametric path.
3. Run Weston as a GDM kiosk session on the reference Intel DRM card and HDMI
   connector. Keep the output profile and HDR static metadata calibrated to the
   connected Hisense display.
4. Attach a BT.2020/ST 2084 image description plus Moonlight mastering metadata
   to eframe's existing Wayland surface. Preserve the foreign display/surface
   ownership boundary and fall back to the SDR tone mapper on any negotiation
   failure.
5. Request an RGB10A2 EGL surface and render decoded AV1/HEVC Main10 frames
   without converting their PQ values in the native path.
6. Let Weston own vblank timing. The session sets
   `ARTEMIS_COMPOSITOR_VSYNC=1`, which disables the client's additional EGL swap
   wait and prevents 4K HDR from falling to half refresh.
7. Keep GNOME available as the SDR desktop fallback and retain explicit backups
   of GDM, AccountsService, compositor config, and the installed Artemis binary.

## Consequences

- The validated KMS state is 3840x2160 at 60 Hz with an AR30 framebuffer,
  BT.2020 RGB, max-bpc 12, and a non-empty HDR static-metadata blob.
- Live Apollo validation uses VA-API AV1 Main10, preserves BT.2020/PQ and source
  mastering metadata, presents approximately 59-60 FPS after warm-up, and
  routes all six Moonlight channels to the persistent HDMI 5.1 sink.
- GNOME still receives the controlled SDR tone-map; the capability report never
  labels that path native HDR.
- The checked-in output calibration is reference-display-specific. Broader beta
  hardware must derive or request its own display profile instead of reusing the
  Hisense values.
