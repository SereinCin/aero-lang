#!/usr/bin/env sh
# Aero 1.2.0 — macOS uninstaller
# Thin wrapper around the installer's --uninstall flag.
set -e
HERE="$(cd "$(dirname "$0")" && pwd)"
exec "${HERE}/install_macos.sh" --uninstall "$@"
