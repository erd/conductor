#!/bin/sh
# The plugin's hook command: `conductor-hook.sh <event>` runs `conductor hook <event>`, and
# `conductor-hook.sh watch` runs `conductor watch` in the conductor session only.
#
# The plugin is installed at user scope, so this runs in every Claude session on the machine. It
# exits 0 when the binary isn't installed, so sessions without conductor are unaffected.
# CONDUCTOR_BIN overrides the binary's path.

bin=${CONDUCTOR_BIN:-$HOME/.cargo/bin/conductor}
[ -x "$bin" ] || exit 0

if [ "$1" = watch ]; then
    [ "$CONDUCTOR_ROLE" = conductor ] || exit 0
    exec "$bin" watch
fi
exec "$bin" hook "$1"
