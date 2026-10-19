# 从源码构建不含 TTS 的 sherpa-onnx 静态库，输出到 src-tauri/vendor/sherpa-onnx
# （src-tauri/.cargo/config.toml 通过 SHERPA_ONNX_LIB_DIR 指向这里）
#
# 为什么不用官方预编译库：官方静态库带着 TTS 用的 espeak-ng（GPL-3.0），会被链接进 aoide.exe，
# 让整个安装包都受 GPL 约束。Aoide 只用语音识别，关掉 TTS 后剩下的都是宽松许可证（Apache-2.0 / MIT 等）。
#
# 用法（需要先 dot-source 构建环境）：
#   . .\scripts\build-env.ps1
#   .\scripts\build-sherpa-onnx.ps1

param(
    # 源码和编译中间文件放在这里（路径尽量短，sherpa-onnx 仓库里有很深的目录）
    [string]$WorkDir = (Join-Path $env:LOCALAPPDATA "aoide-build")
)
# 不用 "Stop"：PowerShell 5.1 会把 git / cmake 写到 stderr 的普通输出当成错误；原生命令一律检查退出码
$ErrorActionPreference = "Continue"

$root = Resolve-Path (Join-Path $PSScriptRoot "..")
$out = Join-Path $root "src-tauri\vendor\sherpa-onnx"

# 版本与 Cargo.lock 中的 sherpa-onnx-sys 保持一致（C API 头文件必须匹配）
$lock = Get-Content (Join-Path $root "src-tauri\Cargo.lock") -Raw
if ($lock -notmatch 'name = "sherpa-onnx-sys"\s+version = "([^"]+)"') { throw "Cargo.lock 中找不到 sherpa-onnx-sys" }
$version = $Matches[1]

if (Test-Path (Join-Path $out "VERSION")) {
    if ((Get-Content (Join-Path $out "VERSION") -Raw).Trim() -eq $version) {
        Write-Host "sherpa-onnx $version (without TTS) is already built: $out"
        return
    }
}

$src = Join-Path $WorkDir "sherpa-onnx-$version"
$build = Join-Path $WorkDir "sherpa-onnx-$version-build"
$install = Join-Path $WorkDir "sherpa-onnx-$version-install"

if (-not (Test-Path (Join-Path $src "CMakeLists.txt"))) {
    New-Item -ItemType Directory -Force $WorkDir | Out-Null
    git -c core.longpaths=true clone --quiet --depth 1 --branch "v$version" https://github.com/k2-fsa/sherpa-onnx.git $src
    if ($LASTEXITCODE) { throw "git clone failed" }
}

# 与 build-env.ps1 设置的 CMAKE_MSVC_RUNTIME_LIBRARY 一致：静态 CRT（/MT）
# CMAKE_VS_PLATFORM_NAME：sherpa-onnx 靠它判断 x64，Ninja 生成器不会设置，需手动指定
# 编译器固定用 MSVC（PATH 里的 LLVM 是给 bindgen 用的，不能让 CMake 选中 clang）
cmake -S $src -B $build -G Ninja `
    -DCMAKE_BUILD_TYPE=Release `
    -DCMAKE_VS_PLATFORM_NAME=x64 `
    -DCMAKE_C_COMPILER=cl -DCMAKE_CXX_COMPILER=cl `
    -DCMAKE_INSTALL_PREFIX="$install" `
    -DBUILD_SHARED_LIBS=OFF `
    -DSHERPA_ONNX_USE_STATIC_CRT=ON `
    -DSHERPA_ONNX_ENABLE_TTS=OFF `
    -DSHERPA_ONNX_ENABLE_C_API=ON `
    -DSHERPA_ONNX_ENABLE_BINARY=OFF `
    -DSHERPA_ONNX_BUILD_C_API_EXAMPLES=OFF `
    -DSHERPA_ONNX_ENABLE_PORTAUDIO=OFF `
    -DSHERPA_ONNX_ENABLE_WEBSOCKET=OFF `
    -DSHERPA_ONNX_ENABLE_PYTHON=OFF `
    -DSHERPA_ONNX_ENABLE_TESTS=OFF
if ($LASTEXITCODE) { throw "cmake configure failed" }
cmake --build $build --config Release
if ($LASTEXITCODE) { throw "cmake build failed" }
cmake --install $build --config Release
if ($LASTEXITCODE) { throw "cmake install failed" }

Remove-Item $out -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $out | Out-Null
Get-ChildItem (Join-Path $install "lib") -Filter *.lib | Copy-Item -Destination $out

# sherpa-onnx-sys 的 build.rs 固定会链接下面这些库名；关掉 TTS 后不再生成 TTS 相关的库，
# 用空库占位（没有任何符号，链接器不会从中取任何东西）
$required = "sherpa-onnx-c-api", "sherpa-onnx-core", "kaldi-decoder-core", "sherpa-onnx-kaldifst-core",
    "sherpa-onnx-fstfar", "sherpa-onnx-fst", "kaldi-native-fbank-core", "kissfft-float",
    "piper_phonemize", "espeak-ng", "ucd", "onnxruntime", "ssentencepiece_core"
$tts = "piper_phonemize", "espeak-ng", "ucd"
$stubDir = Join-Path $build "aoide-stub"
New-Item -ItemType Directory -Force $stubDir | Out-Null
foreach ($name in $required) {
    if (Test-Path (Join-Path $out "$name.lib")) {
        if ($tts -contains $name) { throw "$name.lib 不应出现在关闭 TTS 的构建中" }
        continue
    }
    if ($tts -notcontains $name) { throw "构建结果缺少 $name.lib" }
    Set-Content (Join-Path $stubDir "empty.c") "/* placeholder: TTS is disabled in Aoide's sherpa-onnx build */"
    cl /nologo /c (Join-Path $stubDir "empty.c") /Fo"$(Join-Path $stubDir 'empty.obj')" | Out-Null
    if ($LASTEXITCODE) { throw "cl failed" }
    lib /nologo /OUT:"$(Join-Path $out "$name.lib")" (Join-Path $stubDir "empty.obj")
    if ($LASTEXITCODE) { throw "lib failed" }
}

Set-Content (Join-Path $out "VERSION") $version
Write-Host "sherpa-onnx $version (without TTS) -> $out"
