# 生成 README 演示动图（docs/images/{en,zh}/demo.webp）
#
# 与 readme-images.ps1 一样全部在内存中渲染，不录真实屏幕：
#   - 悬浮条：程序自身的绘制代码按 30 fps 导出逐帧图片（src-tauri/src/overlay.rs 的 export_capsule_frames）
#   - 场景：scripts/readme/scene-demo.html，画面完全由时间 t 决定
#   - 录制：scripts/readme/record-demo.mjs 通过 DevTools 协议驱动无头浏览器逐帧截图
#   - 编码：ffmpeg（winget install Gyan.FFmpeg）
#
# 用法（PowerShell，项目根目录）：.\scripts\readme-demo.ps1 [-Mp4]
#   -Mp4：另外输出 MP4（docs 之外的临时目录），方便发到视频网站或社交平台

param([switch]$Mp4)

# 不用 "Stop"：PowerShell 5.1 会把 cargo / ffmpeg 写到 stderr 的普通输出当成错误；原生命令一律检查退出码
$ErrorActionPreference = "Continue"
$root = Resolve-Path (Join-Path $PSScriptRoot "..")
$out = Join-Path $root "docs\images"
$tmp = Join-Path ([IO.Path]::GetTempPath()) "aoide-demo"

$browser = @(
    "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe",
    "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe",
    "$env:ProgramFiles\Google\Chrome\Application\chrome.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $browser) { throw "Microsoft Edge or Google Chrome is required" }
if (-not (Get-Command ffmpeg -ErrorAction SilentlyContinue)) { throw "ffmpeg is required (winget install Gyan.FFmpeg)" }
if (-not (Get-Command node -ErrorAction SilentlyContinue)) { throw "Node.js 22+ is required" }

Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$tmp\img" | Out-Null

# 1. 悬浮条逐帧图片
. (Join-Path $PSScriptRoot "build-env.ps1") | Out-Null
$env:VT_IMG_DIR = "$tmp\img"
Push-Location (Join-Path $root "src-tauri")
try {
    cargo test --lib export_capsule_frames --quiet -- --ignored
    if ($LASTEXITCODE -ne 0) { throw "capsule frame export failed" }
} finally {
    Pop-Location
}

# 2. 场景页面
Copy-Item (Join-Path $PSScriptRoot "readme\*") $tmp

# 3. 逐帧录制（逻辑尺寸 960x540，2 倍分辨率）并编码
$durationMs = 19500
$fps = 30
foreach ($lang in "en", "zh") {
    $frames = "$tmp\frames-$lang"
    $url = "file:///" + ("$tmp\scene-demo.html" -replace '\\', '/') + "?lang=$lang&record=1"
    Write-Host "Recording $lang..."
    node "$tmp\record-demo.mjs" $browser $url $frames $durationMs $fps 960 540 2
    if ($LASTEXITCODE -ne 0) { throw "recording failed" }

    New-Item -ItemType Directory -Force "$out\$lang" | Out-Null
    $webp = "$out\$lang\demo.webp"
    ffmpeg -v error -y -framerate $fps -i "$frames\frame-%05d.png" `
        -vf "scale=1280:-1:flags=lanczos" -c:v libwebp_anim -quality 82 -compression_level 6 -loop 0 $webp
    if ($LASTEXITCODE -ne 0) { throw "webp encoding failed" }
    Write-Host "  $webp ($([math]::Round((Get-Item $webp).Length / 1MB, 2)) MB)"

    if ($Mp4) {
        $mp4 = "$tmp\aoide-demo-$lang.mp4"
        ffmpeg -v error -y -framerate $fps -i "$frames\frame-%05d.png" `
            -c:v libx264 -pix_fmt yuv420p -crf 18 -preset slow -movflags +faststart $mp4
        if ($LASTEXITCODE -ne 0) { throw "mp4 encoding failed" }
        Write-Host "  $mp4"
    }
}
Write-Host "Done."
