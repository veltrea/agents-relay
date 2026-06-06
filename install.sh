#!/bin/bash
set -e

REPO="https://github.com/veltrea/agents-relay.git"
INSTALL_DIR="$HOME/.agents-relay"
BIN_NAME="agents-relay"

echo "=== agents-relay installer ==="
echo ""

# Rust チェック
if ! command -v cargo &> /dev/null; then
    echo "Error: Rust (cargo) is not installed."
    echo "Install it from https://rustup.rs/"
    exit 1
fi

# Claude Code チェック
if ! command -v claude &> /dev/null; then
    echo "Error: Claude Code CLI is not installed."
    echo "Install it from https://claude.ai/download"
    exit 1
fi

# 既存インストールの確認
if [ -d "$INSTALL_DIR/repo" ]; then
    echo "Updating existing installation..."
    cd "$INSTALL_DIR/repo"
    git pull
else
    echo "Cloning repository..."
    mkdir -p "$INSTALL_DIR"
    git clone "$REPO" "$INSTALL_DIR/repo"
    cd "$INSTALL_DIR/repo"
fi

# ビルド
echo "Building (release)..."
cargo build --release

# バイナリをコピー
cp target/release/$BIN_NAME "$INSTALL_DIR/$BIN_NAME"
echo "Binary installed to $INSTALL_DIR/$BIN_NAME"

# MCP サーバー登録
echo "Registering MCP server..."
claude mcp add --transport stdio --scope user $BIN_NAME -- "$INSTALL_DIR/$BIN_NAME" serve 2>/dev/null || true

echo ""
echo "=== Installation complete ==="
echo ""
echo "Binary:     $INSTALL_DIR/$BIN_NAME"
echo "Config:     $INSTALL_DIR/config.json"
echo "Database:   $INSTALL_DIR/memory.db"
echo ""
echo "CLI usage:"
echo "  $INSTALL_DIR/$BIN_NAME --help"
echo ""
echo "To uninstall:"
echo "  claude mcp remove agents-relay"
echo "  rm -rf $INSTALL_DIR"
