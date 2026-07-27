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
  gstreamer1.0-pipewire
```

## Build and launch

```bash
git clone --recurse-submodules \
  https://github.com/rustyrayzor/Artemis-Linux.git
cd Artemis-Linux
rustup toolchain install 1.85
cargo +1.85 build --release --locked
RUST_LOG=artemis=debug ./target/release/artemis-linux
```

## Acceptance sequence

1. Discover the host, then verify manual host entry also works.
2. Start pairing, enter the displayed PIN in Apollo/Sunshine, and confirm the
   host remains paired after restarting Artemis Linux.
3. Refresh and verify the host application list.
4. Launch Desktop at H.264 1920x1080, 60 FPS, SDR and confirm motion video and
   stereo audio.
5. Disconnect, select 1440p60, reconnect, and verify Apollo negotiated
   2560x1440 at 60 FPS. Repeat at 4K60 and verify 3840x2160 at 60 FPS.
6. Confirm the local presentation remains scaled to fit the Artemis window at
   each resolution.
7. Confirm keyboard press/release, relative mouse movement/buttons/wheel, and
   one connected controller.
8. Disconnect and verify the local stream exits without hanging.
9. Reconnect with Resume, then use End host app and verify `/cancel`.

Capture `RUST_LOG=artemis=debug` output for any failure. Never include private
keys or certificate contents in a diagnostic bundle.
