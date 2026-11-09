#!/usr/bin/env bash
# Whisper (Rust) - single entry point.
#
#   ./run.sh            server + web UI on http://0.0.0.0:8888 (also prints the LAN address)
#   ./run.sh desktop    native desktop app (same UI, engine runs inside the app)
#   ./run.sh app        build "Whisper.app" into ./dist (drag it to /Applications)
#   ./run.sh build      build everything, run nothing
#   ./run.sh test       run the unit tests
#
# Env: PORT (8888), HOST (0.0.0.0), THREADS, WHISPER_HOME (~/.cache/rust-whisper: models, work files, settings)
set -e
cd "$(dirname "$0")"
MODE="${1:-server}"

command -v ffmpeg >/dev/null || { echo "ffmpeg not found. Install it: brew install ffmpeg"; exit 1; }
command -v cargo >/dev/null || { echo "Rust not found. Install it: curl https://sh.rustup.rs -sSf | sh"; exit 1; }

# ---- toolchain (first run only)
rustup target list --installed 2>/dev/null | grep -q wasm32-unknown-unknown || rustup target add wasm32-unknown-unknown
command -v trunk >/dev/null || cargo install trunk --locked
if ! command -v cmake >/dev/null; then
    # whisper.cpp is built with CMake; install a private copy if the system has none.
    if [ ! -x .tools/bin/cmake ]; then
        echo "==> Installing CMake into .tools (first run only)"
        python3 -m venv .tools
        .tools/bin/pip install --quiet cmake
    fi
    export PATH="$PWD/.tools/bin:$PATH"
fi

# ---- build: web UI (Rust -> WASM) first, it is embedded into the server and desktop binaries
build() {
    echo "==> Building web UI (WASM)"
    (cd web && trunk build --release)
    echo "==> Building server + desktop"
    cargo build --release -p whisper-server -p whisper-desktop
}

case "$MODE" in
    server|web)
        build
        exec ./target/release/whisper-server
        ;;
    desktop)
        build
        exec ./target/release/whisper-desktop
        ;;
    app)
        build
        APP=dist/Whisper.app
        rm -rf "$APP"
        mkdir -p "$APP/Contents/MacOS"
        cp target/release/whisper-desktop "$APP/Contents/MacOS/Whisper"
        cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Whisper</string>
  <key>CFBundleDisplayName</key><string>Whisper</string>
  <key>CFBundleIdentifier</key><string>com.profawie.rust-whisper</string>
  <key>CFBundleExecutable</key><string>Whisper</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
        echo "==> Built $APP"
        ;;
    build)
        build
        ;;
    test)
        cargo test --release -p whisper-core
        ;;
    *)
        echo "Usage: $0 [server|desktop|app|build|test]"
        exit 1
        ;;
esac
