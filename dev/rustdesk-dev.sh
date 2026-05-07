#!/usr/bin/env bash
set -euo pipefail

cd /workspace

cmd="${1:-shell}"
shift || true

prepare_bridge() {
    pushd flutter >/dev/null
    flutter pub get
    popd >/dev/null
    flutter_rust_bridge_codegen --rust-input ./src/flutter_ffi.rs --dart-output ./flutter/lib/generated_bridge.dart --c-output ./flutter/macos/Runner/bridge_generated.h
    cp ./flutter/macos/Runner/bridge_generated.h ./flutter/ios/Runner/bridge_generated.h
}

case "${cmd}" in
    init)
        git config --global --add safe.directory "*"
        prepare_bridge
        ;;
    codegen)
        prepare_bridge
        ;;
    run)
        prepare_bridge
        cargo build --features flutter
        cd flutter
        flutter run -d linux "$@"
        ;;
    server)
        mkdir -p /config/log
        pkill -f "flutter run -d linux" >/dev/null 2>&1 || true
        nohup /usr/local/bin/rustdesk-dev run "$@" >/config/log/rustdesk-flutter.log 2>&1 &
        ;;
    server-stop)
        pkill -f "flutter run -d linux" >/dev/null 2>&1 || true
        pkill -f "rustdesk-dev run" >/dev/null 2>&1 || true
        ;;
    build)
        prepare_bridge
        uv run python ./build.py --flutter --hwcodec --unix-file-copy-paste "$@"
        ;;
    build-skip-cargo)
        prepare_bridge
        uv run python ./build.py --flutter --skip-cargo "$@"
        ;;
    doctor)
        rustc --version
        cargo --version
        flutter --version
        flutter doctor -v
        "${VCPKG_ROOT}/vcpkg" version
        ;;
    shell)
        exec bash "$@"
        ;;
    *)
        exec "$cmd" "$@"
        ;;
esac
