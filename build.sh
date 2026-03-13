#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# build.sh  —  Builds two release binaries:
#   1. Native target   → dist/blockchair_downloader
#   2. Windows x86_64  → dist/blockchair_downloader.exe
# ─────────────────────────────────────────────────────────────────────────────
set -e

BINARY="blockchair_downloader"
DIST="dist"
mkdir -p "$DIST"

echo "═══════════════════════════════════════════════════"
echo " Step 1/3 — Add Windows x64 cross-compile target"
echo "═══════════════════════════════════════════════════"
rustup target add x86_64-pc-windows-gnu

echo ""
echo "═══════════════════════════════════════════════════"
echo " Step 2/3 — Build native release binary"
echo "═══════════════════════════════════════════════════"
cargo build --release
NATIVE_TARGET=$(rustc -vV | grep "^host:" | cut -d' ' -f2)
cp "target/release/$BINARY" "$DIST/$BINARY"
echo "✅ Native  → $DIST/$BINARY  (target: $NATIVE_TARGET)"

echo ""
echo "═══════════════════════════════════════════════════"
echo " Step 3/3 — Build Windows x64 release binary"
echo "═══════════════════════════════════════════════════"
# Requires mingw-w64 linker:  sudo apt install gcc-mingw-w64-x86-64
cargo build --release --target x86_64-pc-windows-gnu
cp "target/x86_64-pc-windows-gnu/release/$BINARY.exe" "$DIST/$BINARY.exe"
echo "✅ Windows → $DIST/$BINARY.exe  (target: x86_64-pc-windows-gnu)"

echo ""
echo "═══════════════════════════════════════════════════"
ls -lh "$DIST"/
echo "═══════════════════════════════════════════════════"
echo " Done! Both binaries are in ./$DIST/"
echo "═══════════════════════════════════════════════════"
