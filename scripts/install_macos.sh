#!/usr/bin/env sh
# Aero 1.2.0 — macOS installer
#
# Installs the aero compiler to ~/.aero, adds it to PATH, and creates
# "Aero Terminal.command" — a double-clickable launcher in ~/Applications
# that opens a new Terminal window pre-loaded with the Aero environment
# (the macOS analogue of the Windows Start-menu shortcut).
#
# Usage:
#   ./install_macos.sh            install for the current user (~/.aero)
#   ./install_macos.sh --uninstall
#
# Requires Xcode Command Line Tools for the clang link step during AOT
# compilation (aero drives clang, not gcc, on macOS).

set -e

PREFIX="${HOME}/.aero"
UNINSTALL=0

for arg in "$@"; do
    case "$arg" in
        --uninstall) UNINSTALL=1 ;;
    esac
done

if [ "$UNINSTALL" = "1" ]; then
    echo "Removing ${PREFIX} ..."
    rm -rf "${PREFIX}"
    rm -f "${HOME}/Applications/Aero Terminal.command"
    for rc in "${HOME}/.zprofile" "${HOME}/.zshrc" "${HOME}/.bash_profile"; do
        [ -f "$rc" ] && sed -i '' '\|# Aero environment|d; \|aero/bin|d' "$rc" 2>/dev/null || true
    done
    echo "Done. Close your terminals and reopen them."
    exit 0
fi

# Gate on the C toolchain up front; missing clang is the #1 install failure.
if ! command -v clang >/dev/null 2>&1; then
    echo "clang not found. Install Xcode Command Line Tools first:"
    echo "    xcode-select --install"
    exit 1
fi

HERE="$(cd "$(dirname "$0")" && pwd)"

echo "Installing Aero 1.2.0 to ${PREFIX} ..."
mkdir -p "${PREFIX}/bin"

if [ -f "${HERE}/bin/aero" ]; then
    cp "${HERE}/bin/aero" "${PREFIX}/bin/aero"
else
    echo "bin/aero not found next to this script."
    echo "Build it first:  cargo build --release  (in the source tree)"
    exit 1
fi

if [ -d "${HERE}/plugins" ]; then
    cp -r "${HERE}/plugins" "${PREFIX}/plugins"
fi

# Aero Terminal: a double-clickable .command that opens a new Terminal
# window with the Aero environment loaded.
mkdir -p "${HOME}/Applications"
cat > "${HOME}/Applications/Aero Terminal.command" <<'EOF'
#!/usr/bin/env sh
# Aero Terminal — double-click to open a dedicated Aero shell window.
export AERO_HOME="$HOME/.aero"
export PATH="$AERO_HOME/bin:$PATH"
echo
echo "  Aero 1.2.0 — Aero Programming Language"
echo "  Type \"aero --help\" for usage."
echo
cd "$HOME"
exec "$SHELL"
EOF
chmod +x "${HOME}/Applications/Aero Terminal.command"

# Idempotently add ${PREFIX}/bin to PATH (zsh is the default shell on macOS).
PATH_LINE="export PATH=\"${PREFIX}/bin:\$PATH\"   # Aero environment"
for rc in "${HOME}/.zprofile" "${HOME}/.zshrc" "${HOME}/.bash_profile"; do
    if [ -f "$rc" ] && ! grep -qF "# Aero environment" "$rc" 2>/dev/null; then
        printf '\n# Aero environment\n%s\n' "$PATH_LINE" >> "$rc"
        echo "  Added PATH entry to $rc"
    fi
done

echo
echo "Install complete."
echo "  - Double-click \"Aero Terminal.command\" in your ~/Applications folder."
echo "  - New Terminal windows will have 'aero' on PATH automatically."
echo
