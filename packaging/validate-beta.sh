#!/bin/sh
set -eu

BINARY=${1:-"${XDG_BIN_HOME:-$HOME/.local/bin}/artemis-linux"}
if [ ! -x "$BINARY" ]; then
    echo "Artemis binary not found or not executable: $BINARY" >&2
    exit 1
fi

report=$(mktemp "${TMPDIR:-/tmp}/artemis-diagnostics.XXXXXX")
trap 'rm -f -- "$report"' EXIT HUP INT TERM
"$BINARY" --diagnostics > "$report"

require_value() {
    name=$1
    expected=$2
    if ! grep -Fx "$name=$expected" "$report" >/dev/null; then
        echo "Diagnostic requirement failed: $name=$expected" >&2
        exit 1
    fi
}

require_value target_os linux
require_value gstreamer_plugin_h264parse true
require_value gstreamer_plugin_opusdec true
if ! grep -Eq '^gstreamer_plugin_(pipewiresink|pulsesink)=true$' "$report"; then
    echo "Diagnostic requirement failed: no clocked PipeWire/PulseAudio sink" >&2
    exit 1
fi
if [ "${ARTEMIS_REQUIRE_REFERENCE_AV1:-0}" = "1" ]; then
    require_value decoder_av1_hardware true
    require_value decoder_av1_main10 true
fi

echo "Artemis beta diagnostics passed."
cat "$report"
