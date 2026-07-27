# Artemis Linux

Artemis Linux is a Rust desktop client for Apollo and Sunshine game-streaming
hosts. It is a ground-up Linux client based on the GameStream/Moonlight
protocol behavior used by
[MobinYengejehi/Artemis](https://github.com/MobinYengejehi/Artemis).

The first supported vertical slice includes:

- local-network `_nvstream._tcp` discovery and manual host entry;
- PIN pairing with a persisted client certificate and per-host certificate pin;
- paired host status and application listing;
- H.264/1080p60 SDR video through GStreamer, with stereo Opus decoded through
  GStreamer and played through PipeWire;
- keyboard, relative mouse, wheel, and one game controller;
- disconnect, host application cancellation, and RAII cleanup.

The Rust control plane owns discovery, pairing, XML/HTTP, launch, and lifecycle.
The pinned `moonlight-common-c` submodule owns the real-time transport and input
protocol behind a small C ABI shim.

## Reference environment

The initial target is Ubuntu 24.04 LTS x86_64, GNOME Wayland, PipeWire, and an
Intel/Mesa graphics stack. The software H.264 decoder is the baseline.
Hardware-decoder selection and broader hardware coverage follow through beta
diagnostics.

## Build on Ubuntu 24.04

```bash
sudo apt update
sudo apt install -y \
  build-essential cmake pkg-config libssl-dev libudev-dev \
  libwayland-dev libxkbcommon-dev libasound2-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-libav gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
  gstreamer1.0-pipewire pipewire-bin

git submodule update --init --recursive
rustup toolchain install 1.85
cargo +1.85 run --release -p artemis-app
```

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

## Current scope

The first release deliberately fixes the stream profile to H.264, 1920x1080,
60 FPS, SDR, stereo audio, and one controller. HEVC/AV1, HDR, surround audio,
multi-controller support, touch/pen, remote Internet traversal, and
Apollo-specific virtual-display controls are outside this first slice.

See [docs/architecture.md](docs/architecture.md) and
[docs/ubuntu-reference-runbook.md](docs/ubuntu-reference-runbook.md).

## License

GPL-3.0-only. The linked `moonlight-common-c` dependency is GPL-3.0.
