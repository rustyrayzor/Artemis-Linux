# Artemis Linux

Artemis Linux is a Rust desktop client for Apollo and Sunshine game-streaming
hosts. It is a ground-up Linux client based on the GameStream/Moonlight
protocol behavior used by
[MobinYengejehi/Artemis](https://github.com/MobinYengejehi/Artemis).

The first supported vertical slice includes:

- local-network `_nvstream._tcp` discovery and manual host entry;
- PIN pairing with a persisted client certificate and per-host certificate pin;
- paired host status and application listing, including certificate-pinned Apollo/Sunshine box
  art with a bounded local cache and generated fallback tiles;
- a TV-friendly computer and application browser aligned with Artemis desktop
  actions, with mouse, keyboard, and living-room-sized focus targets;
- negotiated AV1, HEVC, or H.264 SDR video at 720p, 1080p, 1440p, or 4K and
  30/60 FPS through GStreamer, with stereo or 5.1 Opus played through PipeWire;
- AV1 Main10 and HEVC Main10 HDR10 presentation through the Wayland color
  management protocol in the dedicated Weston HDMI session, with an SDR
  tone-map fallback on ordinary compositors;
- keyboard, relative mouse, wheel, and one game controller;
- borderless, edge-to-edge fullscreen presentation with an on-screen control,
  F11 toggle, and Escape-to-exit behavior;
- a toggleable in-stream performance overlay with F10 access, encoded
  bandwidth, receive/decode/presentation rates, queue drops, packet health,
  decoder path, video/audio clock drift, and the active frame-pacing mode;
- disconnect, host application cancellation, and RAII cleanup.

The Rust control plane owns discovery, pairing, XML/HTTP, launch, and lifecycle.
The pinned `moonlight-common-c` submodule owns the real-time transport and input
protocol behind a small C ABI shim.

## Reference environment

The initial target is Ubuntu 24.04 LTS x86_64, PipeWire, and an Intel/Mesa
graphics stack. GNOME Wayland remains the normal SDR desktop and controlled HDR
tone-map fallback. A side-by-side Weston 15.0.1 kiosk session supplies native
BT.2020/PQ HDR10 on the HDMI console without replacing GNOME or upgrading the
host OS. Automatic codec selection advertises AV1 and HEVC only when matching
VA-API hardware decoders are present, then retains H.264 as the compatibility
fallback. Explicit codec preferences can use installed software decoders, with
a 4K60 performance warning.

## Build on Ubuntu 24.04

```bash
sudo apt update
sudo apt install -y \
  build-essential cmake pkg-config libssl-dev libudev-dev \
  libwayland-dev libxkbcommon-dev libasound2-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-libav gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
  gstreamer1.0-pipewire pipewire-bin \
  intel-media-va-driver-non-free

git submodule update --init --recursive
rustup toolchain install 1.85
cargo +1.85 build --release --locked
sh packaging/install.sh
```

The installer is user-scoped: it installs the executable, desktop entry, icon,
and `art:` URL registration below the standard XDG user directories. Each
update first records a timestamped snapshot below
`~/.local/state/artemis-linux/install/backups/`. Run
`sh packaging/rollback.sh` from the same release bundle to restore the most
recent snapshot. The dedicated HDR console session remains an explicit,
reference-hardware installation because its Weston output profile is calibrated
to the connected TV.

The non-free Intel media driver is required for reliable AV1 hardware decoding
on the Ubuntu 24.04 Alder Lake-N reference device. The free driver advertises
AV1 decode on this hardware but fails when submitting frames.

Client identity and host pins are stored below the platform configuration
directory, normally `~/.config/artemis-linux/`. Private-key files are created
with owner-only permissions on Unix.

For a redaction-safe beta environment report:

```bash
./target/release/artemis-linux --diagnostics > artemis-diagnostics.txt
```

The report includes OS/session, kernel, graphics devices, and required
GStreamer plugin availability. It does not read Artemis identity keys,
certificate contents, or host pins.

## Browser controls

Select a computer or application to open or stream it. Right-click a tile, or
focus it and press `Shift+F10`, to open its action menu.

Computer actions include server configuration, all applications (including
locally hidden entries), a control-connection test, local paired-host removal,
and host details. Application actions include streaming on the primary display,
hide/show, details, creation of a Linux application shortcut, and export of a
`.desktop` launcher to `~/Downloads`.

Apollo 0.3.7 and newer can launch an application directly from its WebUI.
Install `packaging/artemis-linux.desktop`, register it as the `art:` URL
handler, then use **Go to Server Config** and select the play button on Apollo's
Applications page:

```bash
install -Dm644 packaging/artemis-linux.desktop \
  ~/.local/share/applications/artemis-linux.desktop
update-desktop-database ~/.local/share/applications
xdg-mime default artemis-linux.desktop x-scheme-handler/art
```

Apollo's `art://launch` link is accepted only when its host UUID matches a
locally paired host. Artemis retrieves the current pinned application list and
matches the requested application UUID before starting the saved stream
profile.

The settings button opens a responsive desktop settings page for resolution,
frame rate, bitrate, display mode, codec, audio, mouse, controller, and
diagnostic preferences. Settings and locally hidden applications are stored in
the Artemis configuration directory and do not change the Apollo/Sunshine host.

## Current scope

The beta supports 8-bit 4:2:0 AV1, HEVC, and H.264 at 1280x720, 1920x1080,
2560x1440, or 3840x2160 and 30 or 60 FPS, subject to host, decoder, display, and
network capability. Higher refresh rates remain disabled until their display
mode and frame-pacing paths are validated. HEVC Main10 and AV1 Main10 are
negotiated only when hardware P010 decode, EGL transfer, RGB10A2 presentation,
and a 10-bit window surface are all available; otherwise the 10-bit setting
explains the failing capability and remains disabled. Bitrate is adjustable from
1 to 300 Mbps. Balanced follows the linked
[SDR and HDR bitrate tables](https://docs.google.com/spreadsheets/d/1XF01BCk_syQeiqugPUqTl-pNTDDA6dHlZCpMhGwcv0w)
and rounds up to a whole Mbps. High Quality LAN adds 25% before rounding, while
Custom preserves the selected slider value when resolution, frame rate, or
codec changes. The local window scales the native stream texture without
lowering source resolution. Frame pacing uses GStreamer's clocked sink to
schedule decoded video from the host presentation timestamps. It is enabled by
default and can be disabled for the lowest-latency latest-frame behavior. 5.1
audio is available when the host capture
endpoint and Linux HDMI/PipeWire profile both expose six channels. HDR signaling
and mastering metadata cross the Moonlight FFI, P010 hardware-decode, RGB10A2,
and Wayland color-management paths. The dedicated Artemis HDR session presents
native BT.2020/PQ and programs the DRM connector with a 10-bit framebuffer,
BT.2020 RGB, and HDR static metadata. GNOME 46 continues to use the GPU SDR
tone-map fallback. Diagnostics distinguish both paths explicitly.
Multi-controller support, touch/pen, remote Internet traversal, and Apollo
virtual-display controls remain outside this slice. Artemis carries a
small pinned eframe patch that prefers an RGB10A2 EGL surface and safely falls
back to 8-bit. UbuntuLab now validates a 10-bit window plus hardware AV1 Main10
with a negotiated 3840x2160, 60 FPS stream profile. Compositor-level reserved
shortcut capture remains outside the beta. In the HDR session Weston owns
vblank pacing, so Artemis
automatically disables its client swap interval while preserving the user's
V-Sync preference for other sessions; this avoids a second wait that reduced
4K HDR presentation to roughly 30 FPS.

The pinned `moonlight-common-c` transport fixes the encoder bitrate for the
life of an RTSP session. Apollo/Sunshine can dynamically vary FEC within that
budget, but a bitrate change requires reconnecting the stream. Artemis therefore
uses conservative Balanced and High Quality LAN profiles and reports live loss,
recovery, and delivered bandwidth rather than pretending to provide seamless
mid-session bitrate adaptation. Automatic bitrate changes will remain gated on
an upstream transport API that can adjust without oscillation or a visible
reconnect.

See [docs/architecture.md](docs/architecture.md) and
[docs/ubuntu-reference-runbook.md](docs/ubuntu-reference-runbook.md).

## License

GPL-3.0-only. The linked `moonlight-common-c` dependency is GPL-3.0.
