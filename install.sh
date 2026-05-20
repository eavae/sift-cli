#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR="${HOME}/.local/bin"
BINARIES=("sift")

echo "==> Installing sift to ${INSTALL_DIR}..."

# Ensure install directory exists
if [[ ! -d "${INSTALL_DIR}" ]]; then
    echo "    Creating ${INSTALL_DIR}..."
    mkdir -p "${INSTALL_DIR}"
fi

# Always build release binaries
if ! command -v cargo &>/dev/null; then
    echo "Error: cargo not found. Please install Rust: https://rustup.rs/"
    exit 1
fi

echo "    Building release binaries with cargo..."
cargo build --release

for bin in "${BINARIES[@]}"; do
    cp "target/release/${bin}" "${INSTALL_DIR}/${bin}"
    chmod +x "${INSTALL_DIR}/${bin}"
    echo "==> Installed: ${INSTALL_DIR}/${bin}"
done

# Check if install dir is in PATH
if [[ ":${PATH}:" != *":${INSTALL_DIR}:"* ]]; then
    echo ""
    echo "WARNING: ${INSTALL_DIR} is not in your PATH."
    echo "         Add the following to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
    echo ""
    echo "    export PATH=\"\${HOME}/.local/bin:\${PATH}\""
    echo ""
fi

echo "Done."
