#!/usr/bin/env bash

cargo install flutter_rust_bridge_codegen --version 1.80.1 --features uuid
flutter pub get
~/.cargo/bin/flutter_rust_bridge_codegen --rust-input ../src/flutter_ffi.rs --dart-output ./lib/generated_bridge.dart --c-output ./macos/Runner/bridge_generated.h
uv run python - <<'PY'
from pathlib import Path

p = Path('lib/generated_bridge.dart')
text = p.read_text()
text = text.replace(
    'ffi.NativeFunction<ffi.Bool Function(DartPort',
    'ffi.NativeFunction<ffi.Uint8 Function(DartPort',
)
text = text.replace('ffi.Pointer<bool>', 'ffi.Bool')
text = text.replace('ffi.Pointer<ffi.Bool>', 'ffi.Bool')
text = text.replace(
    'typedef bool = ffi.NativeFunction<ffi.Int Function(ffi.Pointer<ffi.Int>)>;\n',
    '',
)
text = text.replace(
    'void store_dart_post_cobject(\n    int ptr,\n  )',
    'void store_dart_post_cobject(\n    DartPostCObject ptr,\n  )',
)
text = text.replace(
    "_lookup<ffi.NativeFunction<ffi.Void Function(ffi.Int)>>(\\n    'store_dart_post_cobject',\\n  )",
    "_lookup<ffi.NativeFunction<ffi.Void Function(DartPostCObject)>>(\\n    'store_dart_post_cobject',\\n  )",
)
text = text.replace(
    '_store_dart_post_cobjectPtr.asFunction<void Function(int)>()',
    '_store_dart_post_cobjectPtr.asFunction<void Function(DartPostCObject)>()',
)
lines = text.splitlines()
out = []
in_as_function = False
for line in lines:
    if line.lstrip().startswith('.asFunction<') or '.asFunction<' in line:
        in_as_function = True
    if in_as_function:
        line = line.replace('ffi.Bool', 'bool')
        if line.rstrip().endswith('>();'):
            in_as_function = False
    stripped = line.lstrip()
    indent = line[:len(line) - len(stripped)]
    if stripped.startswith('ffi.Bool ') and stripped.endswith(','):
        line = indent + 'bool ' + stripped[len('ffi.Bool '):]
    if stripped.endswith('{') and 'ffi.Bool ' in stripped:
        line = line.replace('ffi.Bool ', 'bool ')
    out.append(line)
p.write_text('\n'.join(out) + '\n')

freezed = Path('lib/generated_bridge.freezed.dart')
if freezed.exists():
    text = freezed.read_text()
    text = text.replace(
        'ffi.NativeFunction<ffi.Int Function(ffi.Pointer<ffi.Int>)>',
        'bool',
    )
    text = text.replace(
        'ffi.NativeFunction<Int Function(Pointer<Int>)>',
        'bool',
    )
    freezed.write_text(text)
PY
# call `flutter clean` if cargo build fails
# export LLVM_HOME=/Library/Developer/CommandLineTools/usr/
cargo build --features flutter
flutter run $@
