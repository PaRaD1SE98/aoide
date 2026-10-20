# 生成 README 配图（docs/images/{en,zh}/*.png）
#
# 全部在内存中渲染，不截取真实屏幕：
#   - 录音悬浮条：调用程序自身的绘制代码输出 PNG（src-tauri/src/overlay.rs 的 export_capsule_images）
#   - 设置页、结果卡片：真实的 ui/ 页面 + 模拟的 Tauri 接口（scripts/readme/mock.js），由无头浏览器渲染
#   - 桌面、窗口、任务栏：scripts/readme/ 下的场景页面
#
# 用法（PowerShell，项目根目录）：.\scripts\readme-images.ps1

$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..")
$out = Join-Path $root "docs\images"
$tmp = Join-Path ([IO.Path]::GetTempPath()) "aoide-readme"

$browser = @(
    "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe",
    "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe",
    "$env:ProgramFiles\Google\Chrome\Application\chrome.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $browser) { throw "Microsoft Edge or Google Chrome is required" }

Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$tmp\img" | Out-Null

# 1. 录音悬浮条
. (Join-Path $PSScriptRoot "build-env.ps1") | Out-Null
$env:VT_IMG_DIR = "$tmp\img"
Push-Location (Join-Path $root "src-tauri")
try {
    cargo test --lib export_capsule_images --quiet -- --ignored
    if ($LASTEXITCODE -ne 0) { throw "capsule export failed" }
} finally {
    Pop-Location
}

# 2. 界面页面副本 + 模拟接口
Copy-Item (Join-Path $root "ui") "$tmp\ui" -Recurse
Copy-Item (Join-Path $PSScriptRoot "readme\*") $tmp
foreach ($page in "index.html", "result.html") {
    $file = "$tmp\ui\$page"
    $html = [IO.File]::ReadAllText($file)
    $html = $html.Replace('<script src="i18n.js"></script>', '<script src="../mock.js"></script><script src="i18n.js"></script>')
    [IO.File]::WriteAllText($file, $html)
}

# 3. 渲染场景（2 倍分辨率）
$scenes = @(
    @{ name = "states"; w = 760; h = 150 },
    @{ name = "result"; w = 960; h = 540 },
    @{ name = "settings"; w = 1060; h = 700 }
)
function Render($page, $w, $h, $png) {
    $url = "file:///" + ($page -replace '\\', '/')
    # 浏览器把正常输出写到 stderr，用 Start-Process 避免 PowerShell 当成错误
    $browserArgs = @(
        "--headless=new", "--disable-gpu", "--hide-scrollbars", "--force-device-scale-factor=2",
        "--blink-settings=preferredColorScheme=1", "--virtual-time-budget=3000",
        "--user-data-dir=`"$tmp\profile`"", "--window-size=$w,$h", "--screenshot=`"$png`"", "`"$url`""
    )
    Start-Process -FilePath $browser -ArgumentList $browserArgs -Wait -WindowStyle Hidden
}

foreach ($lang in "en", "zh") {
    New-Item -ItemType Directory -Force "$out\$lang" | Out-Null
    # 设置页单独渲染，再由 scene-settings.html 套上窗口外框
    Render "$tmp\ui\index.html?lang=$lang&section=speech" 980 608 "$tmp\settings-page-$lang.png"
    foreach ($s in $scenes) {
        $png = "$out\$lang\$($s.name).png"
        Render "$tmp\scene-$($s.name).html?lang=$lang" $s.w $s.h $png
        Write-Host "  $png"
    }
}
Write-Host "Done."
