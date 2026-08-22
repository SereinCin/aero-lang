#!/usr/bin/env sh
# Aero 1.2.0 — Linux installer
#
# Installs the aero compiler to ~/.aero, adds it to PATH, and creates an
# "Aero Terminal" launcher so you can open a dedicated shell pre-loaded
# with the Aero environment (the Linux analogue of the Windows Start-menu
# shortcut).
#
# Usage:
#   ./install_linux.sh            install for the current user (~/.aero)
#   ./install_linux.sh --prefix  /custom/path
#   ./install_linux.sh --uninstall
#
# The script is idempotent: re-running it just refreshes the files.

set -e

PREFIX="${HOME}/.aero"
UNINSTALL=0

for arg in "$@"; do
    case "$arg" in
        --uninstall) UNINSTALL=1 ;;
        --prefix=*) PREFIX="${arg#--prefix=}" ;;
        --prefix) shift; PREFIX="$1" ;;
    esac
done

if [ "$UNINSTALL" = "1" ]; then
    echo "Removing ${PREFIX} ..."
    rm -rf "${PREFIX}"
    # strip the PATH lines we added from every shell rc we know about
    for rc in "${HOME}/.bashrc" "${HOME}/.zshrc" "${HOME}/.profile"; do
        [ -f "$rc" ] && sed -i '\|Aero environment|d; \|aero/bin|d' "$rc" 2>/dev/null || true
    done
    echo "Done. Close your terminals and reopen them."
    exit 0
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

# Optional core crates (pure Aero libs) — copied for offline use.
if [ -d "${HERE}/plugins" ]; then
    cp -r "${HERE}/plugins" "${PREFIX}/plugins"
fi

# Aero Terminal launcher: opens a fresh shell with Aero in PATH.
cat > "${PREFIX}/bin/aero-term" <<'EOF'
#!/usr/bin/env sh
# Aero Terminal — dedicated shell with the Aero environment loaded.
export AERO_HOME="$HOME/.aero"
export PATH="$AERO_HOME/bin:$PATH"
echo
echo "  Aero 1.2.0 — Aero Programming Language"
echo "  Type \"aero --help\" for usage."
echo
cd "$HOME"
exec "$SHELL"
EOF
chmod +x "${PREFIX}/bin/aero-term"

# Idempotently add ${PREFIX}/bin to PATH in the user's shell rc files.
PATH_LINE="export PATH=\"${PREFIX}/bin:\$PATH\"   # Aero environment"
for rc in "${HOME}/.bashrc" "${HOME}/.zshrc" "${HOME}/.profile"; do
    if [ -f "$rc" ] && ! grep -qF "# Aero environment" "$rc" 2>/dev/null; then
        printf '\n# Aero environment\n%s\n' "$PATH_LINE" >> "$rc"
        echo "  Added PATH entry to $rc"
    fi
done

# Create a desktop launcher so Aero Terminal shows up in the app menu.
if command -v desktop-file-install >/dev/null 2>&1; then
    mkdir -p "${HOME}/.local/share/applications"
    cat > "${HOME}/.local/share/applications/aero-term.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Aero Terminal
Comment=Aero Programming Language terminal
Exec=${PREFIX}/bin/aero-term
Terminal=true
Categories=Development;
EOF
    echo "  Desktop launcher created."
fi

echo
echo "Install complete."
echo "  - Open \"Aero Terminal\" from your app menu, or run:  ${PREFIX}/bin/aero-term"
echo "  - New terminals will have 'aero' on PATH automatically."
echo
