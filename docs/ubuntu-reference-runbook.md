# Ubuntu reference runbook

This runbook is for the first live test on the `local_ubuntu` SSH alias. The
alias is global host configuration and is intentionally not stored in this
repository. Do not run this deployment until source and CI validation pass.

## Host baseline

- Ubuntu 24.04 LTS x86_64
- GNOME on Wayland
- PipeWire audio
- Intel/Mesa graphics
- Apollo 0.4.6 or Sunshine 2026.516.143833 on the streaming host
- client and host on the same LAN for the first test

## Install dependencies

```bash
sudo apt update
sudo apt install -y \
  build-essential cmake pkg-config libssl-dev libudev-dev \
  libwayland-dev libxkbcommon-dev libasound2-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-libav gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
  gstreamer1.0-pipewire intel-media-va-driver-non-free
```

The non-free Intel media driver is required for reliable AV1 hardware decoding
on the Alder Lake-N reference device. The free driver advertises AV1 decode but
fails during frame submission. Confirm the active driver and AV1 profile with:

```bash
vainfo --display drm --device /dev/dri/renderD128 | grep -E \
  'Driver version|VAProfileAV1'
```

Optional HDR and performance inspection tools used by the reference validation
are `edid-decode`, `libdrm-tests`, and `intel-gpu-tools`. They are not runtime
dependencies.

The reference account must retain direct media-device access even during SSH
diagnostics or display-session turnover:

```bash
sudo usermod -aG video,render,audio ray-svc
```

Start a new login session after changing supplementary groups. If GStreamer
cached the VA plugin while render access was unavailable, move
`~/.cache/gstreamer-1.0/registry.x86_64.bin` aside and run
`gst-inspect-1.0 vaav1dec` once to rebuild it.

## Build and launch

```bash
git clone --recurse-submodules \
  https://github.com/rustyrayzor/Artemis-Linux.git
cd Artemis-Linux
rustup toolchain install 1.85
cargo +1.85 build --release --locked
sh packaging/install.sh
RUST_LOG=artemis=debug ~/.local/bin/artemis-linux
```

For a normal beta update, build the new release and run the installer again.
It records the existing binary, desktop entry, and icon before replacement.
To restore the prior installation:

```bash
sh packaging/rollback.sh
```

Run the non-secret-bearing readiness check after installation:

```bash
ARTEMIS_REQUIRE_REFERENCE_AV1=1 sh packaging/validate-beta.sh
```

## Native HDR console session

GNOME 46 does not publish `color-management-v1`, so it remains the SDR fallback.
UbuntuLab uses Weston 15.0.1 under `/opt/artemis-hdr` as a side-by-side GDM
session. Build Weston with LCMS color management, the DRM and headless backends,
the GL renderer, and kiosk shell. Apply
`packaging/weston-patches/0001-parametric-stock-srgb.patch` before building.
The patch completes Weston's documented conversion of its stock sRGB profile to
parametric form; without it, the still-unimplemented ICC-to-parametric branch
prevents a PQ output from being enabled.

Install the release binary and session files after backing up the existing
binary, GDM configuration, and AccountsService record:

```bash
install -Dm755 target/release/artemis-linux ~/.local/bin/artemis-linux
sudo install -Dm755 packaging/artemis-hdr/artemis-hdr-launch \
  /usr/local/bin/artemis-hdr-launch
sudo install -Dm755 packaging/artemis-hdr/artemis-hdr-session \
  /usr/local/bin/artemis-hdr-session
sudo install -Dm644 packaging/artemis-hdr/weston.ini \
  /etc/xdg/weston/artemis-hdr.ini
sudo install -Dm644 packaging/artemis-hdr/artemis-hdr.desktop \
  /usr/share/wayland-sessions/artemis-hdr.desktop
```

The checked-in `weston.ini` is calibrated for the reference Hisense HDMI
display and must not be copied to another display without updating its target
primaries and luminance. The session launcher sets
`ARTEMIS_COMPOSITOR_VSYNC=1`; Weston already owns vblank timing, so Artemis
skips the second EGL swap wait while keeping the saved V-Sync preference intact
for other sessions.

Select `artemis-hdr` as the `ray-svc` session in AccountsService/GDM and restart
GDM. The reference installation backup is
`/var/backups/artemis-hdr/20260731T163944`. Restore that directory's GDM,
AccountsService, Weston, and binary copies over their installed paths to roll
back, then restart GDM.

For persistent 5.1 HDMI output, select the available `output:hdmi-surround`
profile with its PipeWire device ID and save it:

```bash
pw-cli set-param DEVICE_ID Profile '{ index: PROFILE_INDEX, save: true }'
wpctl set-default SINK_ID
```

Verify `wpctl status -n` shows `hdmi-surround` and six active ports during a
stream. IDs are runtime values; obtain them from `wpctl status -n` and
`pw-cli enum-params DEVICE_ID EnumProfile` rather than hard-coding them.

## Acceptance sequence

1. Discover the host, then verify manual host entry also works.
2. Start pairing, enter the displayed PIN in Apollo/Sunshine, and confirm the
   host remains paired after restarting Artemis Linux.
3. Refresh and verify the host application list. Confirm configured Apollo box
   art appears after the background load, survives an Artemis restart from the
   local cache, and a missing image retains a readable generated tile.
4. Force H.264 and launch Desktop at 1920x1080, 60 FPS, SDR. Confirm motion
   video and stereo audio to establish the compatibility baseline.
5. Disconnect, select 1440p60, reconnect, and verify Apollo negotiated
   2560x1440 at 60 FPS and the selected bitrate. Confirm Balanced and High
   Quality LAN select 11 and 14 Mbps for AV1. Repeat at 4K60 and confirm the AV1
   choices are 24 and 30 Mbps. Confirm moving the slider selects Custom and
   preserves that value when another resolution or codec is selected.
6. Select HEVC, reconnect at 4K60, and confirm the overlay reports
   `VA-API HEVC (vah265dec)`. Repeat with AV1 and confirm
   `VA-API AV1 (vaav1dec)`. Verify Apollo falls back cleanly when a preferred
   codec is unavailable.
7. Select 5.1 audio and verify Apollo captures six channels, Artemis reports
   `5.1 surround (6 channels)`, and the PipeWire stream exposes named HDMI
   channels. Confirm clean teardown leaves no Artemis or `pw-play` process.
8. Confirm the 10-bit setting is enabled. Select HEVC Main10 and verify the
   overlay reports `10-bit Main10`, then repeat with AV1 Main10. Startup logs
   must report `presentation_bit_depth=10`; on hardware without RGB10A2, confirm
   Artemis remains usable in 8-bit mode and does not advertise Main10.
9. Select HDR10, AV1, 4K60, and Balanced. In the Artemis HDR session, confirm the
   overlay reports `HDR10`, `BT.2020`, `metadata present`,
   `Native HDR10 (BT.2020/PQ)`, and `10-bit Main10`. Confirm ingress, decode,
   and presentation remain near 60 FPS with no callback drops after warm-up.
   `modetest -M i915 -c` must show BT.2020 RGB, a non-empty
   `HDR_OUTPUT_METADATA` blob, and max bpc 10 or greater; `i915_display_info`
   must show an AR30 3840x2160 framebuffer at 60 Hz. Repeat with HEVC. Under
   GNOME 46, confirm native presentation is false and the overlay instead says
   `HDR source to SDR tone map`. H.264 must keep HDR unavailable.
10. Confirm the local presentation preserves the selected stream resolution
   while scaling the image to fit the Artemis window.
11. Enter fullscreen using the on-screen control, exit with Escape, then toggle
   fullscreen on and off with F11. Confirm the video is edge-to-edge with no
   desktop panel, window decoration, application controls, or status border.
12. Enable Show performance diagnostics before streaming or press F10 during a
   stream. Confirm the overlay remains visible in fullscreen and updates its
   incoming, decoded, presented, bandwidth, drop, packet-health, and clock
   values about once per second. Confirm `Pacing` reports `Presentation
   timestamps`, ingress/decode/presentation remain near the requested rate, and
   video/audio clock drift does not grow continuously. Press F10 again to hide
   it.
13. Confirm keyboard press/release, relative mouse movement/buttons/wheel, and
   one connected controller.
14. Disconnect and verify the local stream exits without hanging.
15. Reconnect with Resume, then use End host app and verify `/cancel`.

For unattended beta diagnostics, the same launch path can be exercised without
changing normal UI startup:

```bash
ARTEMIS_AUTOSTART_HOST=192.168.100.128 \
ARTEMIS_AUTOSTART_APP=Desktop \
ARTEMIS_AUTOSTART_PRESET=4K \
ARTEMIS_AUTOSTART_FPS=60 \
ARTEMIS_AUTOSTART_BITRATE_MBPS=40 \
ARTEMIS_AUTOSTART_CODEC=av1 \
ARTEMIS_AUTOSTART_FULLSCREEN=true \
ARTEMIS_AUTOSTOP_AFTER_SECONDS=60 \
ARTEMIS_AUTOSTOP_CANCEL_HOST=true \
./artemis-linux
```

Both the host and application variables are required. The preset accepts
`720p`, `1080p`, `1440p`, `4K`, or `2160p`; FPS accepts `30` or `60`; codec
accepts `auto`, `av1`, `hevc`, or `h264`; and bitrate remains constrained to the
same 1–300 Mbps range as the UI. When bitrate is omitted, Artemis uses the
matching Balanced value for the saved SDR or HDR10 setting. Explicit 90/120 FPS
requests are rejected until the higher-refresh display and frame-pacing paths
are enabled. These variables
are intended for repeatable reference-host diagnostics and are ignored when the
required pair is absent.
The optional autostop value accepts 5–3600 seconds and cleanly disconnects,
closes fullscreen, and exits after the native session reaches connected state.
Setting `ARTEMIS_AUTOSTOP_CANCEL_HOST=true` also sends the authenticated host
`/cancel` request and waits for its result before exiting. The cancel option is
rejected unless an autostop deadline is configured.

Capture `RUST_LOG=artemis=debug` output for any failure. Never include private
keys or certificate contents in a diagnostic bundle.

Apollo/Sunshine adjusts FEC dynamically, but the pinned Moonlight transport
latches the requested bitrate during RTSP negotiation. A different bitrate
therefore requires reconnecting. Treat the overlay's poor-connection warning,
packet issues, and FEC recovery as the supported signal to step down from High
Quality LAN to Balanced; do not claim seamless adaptive bitrate in beta
results.
