#!/usr/bin/env sh
# Aero Terminal — double-click to open a dedicated Aero shell window.
# Installed to ~/Applications/Aero Terminal.command by install_macos.sh.
export AERO_HOME="$HOME/.aero"
export PATH="$AERO_HOME/bin:$PATH"
echo
echo "  Aero 1.2.0 — Aero Programming Language"
echo "  Type \"aero --help\" for usage."
echo
cd "$HOME"
exec "$SHELL"
