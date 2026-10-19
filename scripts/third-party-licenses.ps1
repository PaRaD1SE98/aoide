# 生成随安装包分发的第三方许可声明：src-tauri/vendor/THIRD_PARTY_LICENSES.html
#   - Rust 依赖：cargo-about 按 Cargo.lock 生成（cargo install cargo-about --features cli）
#   - C/C++ 库与资源：scripts/licenses/native.json 列出的组件，许可证原文在 scripts/licenses/native/
#     （升级 sherpa-onnx、whisper-rs 等时同步更新）
#
# 用法：.\scripts\third-party-licenses.ps1

# 不用 "Stop"：PowerShell 5.1 会把 cargo 写到 stderr 的普通输出当成错误；原生命令一律检查退出码
$ErrorActionPreference = "Continue"

$root = Resolve-Path (Join-Path $PSScriptRoot "..")
$dir = Join-Path $PSScriptRoot "licenses"
$manifestPath = Join-Path $root "src-tauri\Cargo.toml"
$out = Join-Path $root "src-tauri\vendor\THIRD_PARTY_LICENSES.html"
$utf8 = New-Object System.Text.UTF8Encoding $false

cargo fetch --locked --manifest-path $manifestPath
if ($LASTEXITCODE) { throw "cargo fetch failed" }

$tmp = [System.IO.Path]::GetTempFileName()
cargo about generate --offline --locked --fail --manifest-path $manifestPath `
    -c (Join-Path $dir "about.toml") (Join-Path $dir "about.hbs") -o $tmp
if ($LASTEXITCODE) { throw "cargo about failed" }
$html = [System.IO.File]::ReadAllText($tmp, $utf8)
Remove-Item $tmp

$enc = { param($s) [System.Net.WebUtility]::HtmlEncode($s) }
$native = [System.IO.File]::ReadAllText((Join-Path $dir "native.json"), $utf8) | ConvertFrom-Json
$sb = New-Object System.Text.StringBuilder
[void]$sb.Append("<h2>Native libraries and assets</h2>`n  <p>C/C++ libraries compiled into Aoide and files bundled with it:</p>`n")
foreach ($c in $native) {
    $text = [System.IO.File]::ReadAllText((Join-Path $dir "native\$($c.file)"), $utf8)
    $meta = @($c.version, $c.license) | Where-Object { $_ } | ForEach-Object { & $enc $_ }
    [void]$sb.Append("  <div class=`"card`">`n")
    [void]$sb.Append("    <h3>$(& $enc $c.name)</h3>`n")
    [void]$sb.Append("    <p class=`"meta`">$($meta -join ' · ') · <a href=`"$(& $enc $c.url)`">$(& $enc $c.url)</a></p>`n")
    [void]$sb.Append("    <details><summary>License text</summary><pre>$(& $enc $text)</pre></details>`n")
    [void]$sb.Append("  </div>`n")
}
if (-not $html.Contains("<!-- NATIVE -->")) { throw "about.hbs is missing the <!-- NATIVE --> marker" }
$html = $html.Replace("<!-- NATIVE -->", $sb.ToString())

New-Item -ItemType Directory -Force (Split-Path $out) | Out-Null
[System.IO.File]::WriteAllText($out, $html, $utf8)
Write-Host "Third-party licenses -> $out"
