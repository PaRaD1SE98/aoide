#!/usr/bin/env bash
# macOS：从源码构建不含 TTS 的 sherpa-onnx 静态库，输出到 src-tauri/vendor/sherpa-onnx
# （src-tauri/.cargo/config.toml 通过 SHERPA_ONNX_LIB_DIR 指向这里）。Windows 版见 build-sherpa-onnx.ps1，
# 不用官方预编译库的原因也写在那里（官方库带着 GPL-3.0 的 espeak-ng）。
#
# 需要 Xcode 命令行工具和 CMake（brew install cmake）。用法：
#   ./scripts/build-sherpa-onnx.sh [工作目录]

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/src-tauri/vendor/sherpa-onnx"
work="${1:-${TMPDIR:-/tmp}/aoide-build}"

# 版本与 Cargo.lock 中的 sherpa-onnx-sys 保持一致（C API 头文件必须匹配）
version="$(grep -A1 '^name = "sherpa-onnx-sys"' "$root/src-tauri/Cargo.lock" | sed -n 's/^version = "\(.*\)"/\1/p')"
[ -n "$version" ] || { echo "Cargo.lock 中找不到 sherpa-onnx-sys" >&2; exit 1; }

if [ -f "$out/VERSION" ] && [ "$(cat "$out/VERSION")" = "$version" ]; then
    echo "sherpa-onnx $version (without TTS) is already built: $out"
    exit 0
fi

src="$work/sherpa-onnx-$version"
build="$work/sherpa-onnx-$version-build"
install="$work/sherpa-onnx-$version-install"

if [ ! -f "$src/CMakeLists.txt" ]; then
    mkdir -p "$work"
    git clone --quiet --depth 1 --branch "v$version" https://github.com/k2-fsa/sherpa-onnx.git "$src"
fi

# 只构建 Apple 芯片版本；最低系统版本与 tauri.macos.conf.json 一致
cmake -S "$src" -B "$build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_OSX_ARCHITECTURES=arm64 \
    -DCMAKE_OSX_DEPLOYMENT_TARGET=11.0 \
    -DCMAKE_INSTALL_PREFIX="$install" \
    -DBUILD_SHARED_LIBS=OFF \
    -DSHERPA_ONNX_ENABLE_TTS=OFF \
    -DSHERPA_ONNX_ENABLE_C_API=ON \
    -DSHERPA_ONNX_ENABLE_BINARY=OFF \
    -DSHERPA_ONNX_BUILD_C_API_EXAMPLES=OFF \
    -DSHERPA_ONNX_ENABLE_PORTAUDIO=OFF \
    -DSHERPA_ONNX_ENABLE_WEBSOCKET=OFF \
    -DSHERPA_ONNX_ENABLE_PYTHON=OFF \
    -DSHERPA_ONNX_ENABLE_TESTS=OFF
cmake --build "$build" --config Release -j "$(sysctl -n hw.ncpu)"
cmake --install "$build" --config Release

rm -rf "$out"
mkdir -p "$out"
cp "$install"/lib/*.a "$out/"

# sherpa-onnx-sys 的 build.rs 固定会链接下面这些库名；关掉 TTS 后不再生成 TTS 相关的库，
# 用空库占位（没有任何符号，链接器不会从中取任何东西）
required=(sherpa-onnx-c-api sherpa-onnx-core kaldi-decoder-core sherpa-onnx-kaldifst-core
    sherpa-onnx-fstfar sherpa-onnx-fst kaldi-native-fbank-core kissfft-float
    piper_phonemize espeak-ng ucd onnxruntime ssentencepiece_core)
tts=" piper_phonemize espeak-ng ucd "
stub="$build/aoide-stub"
mkdir -p "$stub"
for name in "${required[@]}"; do
    if [ -f "$out/lib$name.a" ]; then
        if [[ "$tts" == *" $name "* ]]; then
            echo "lib$name.a 不应出现在关闭 TTS 的构建中" >&2
            exit 1
        fi
        continue
    fi
    if [[ "$tts" != *" $name "* ]]; then
        echo "构建结果缺少 lib$name.a" >&2
        exit 1
    fi
    echo "/* placeholder: TTS is disabled in Aoide's sherpa-onnx build */" > "$stub/empty.c"
    cc -arch arm64 -mmacosx-version-min=11.0 -c "$stub/empty.c" -o "$stub/empty.o"
    ar rcs "$out/lib$name.a" "$stub/empty.o"
done

echo "$version" > "$out/VERSION"
echo "sherpa-onnx $version (without TTS) -> $out"
