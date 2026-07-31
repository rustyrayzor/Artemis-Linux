# Artemis eframe patch

This directory is the crates.io source for eframe 0.31.1 under its upstream
MIT/Apache-2.0 license. Artemis changes only the glow native-window path:

- `NativeOptions::color_buffer_bits` requests a preferred RGB channel depth.
- glutin selects a matching deep-color configuration when present and otherwise
  uses a compatible 8-bit configuration from the same enumeration.

See `docs/decisions/0001-rgb10a2-window-surface.md` for the rationale and
upgrade requirements. Do not replace this directory during an eframe update
without revalidating 8-bit fallback and live RGB10A2 creation on UbuntuLab.
