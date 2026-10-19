# 为当前 PowerShell 会话准备构建环境（dot-source 执行：. .\scripts\build-env.ps1）
#   - 加载 Visual Studio 开发者环境（MSVC 编译器、Windows SDK 头文件）
#   - 设置 Vulkan SDK（whisper.cpp 的 GPU 后端）和 LLVM（bindgen 生成绑定）路径

$installer = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer"
$env:Path = "$installer;$env:Path"
$vs = & "$installer\vswhere.exe" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw "未找到带 C++ 工具链的 Visual Studio（需要「使用 C++ 的桌面开发」工作负载）" }
Import-Module (Join-Path $vs "Common7\Tools\Microsoft.VisualStudio.DevShell.dll")
Enter-VsDevShell -VsInstallPath $vs -SkipAutomaticLocation -DevCmdArguments "-arch=x64 -host_arch=x64" | Out-Null

if (-not $env:VULKAN_SDK) {
    $sdk = Get-ChildItem C:\VulkanSDK -Directory -ErrorAction SilentlyContinue | Sort-Object Name | Select-Object -Last 1
    if (-not $sdk) { throw "未找到 Vulkan SDK，请先安装：winget install KhronosGroup.VulkanSDK" }
    $env:VULKAN_SDK = $sdk.FullName
}
if (-not $env:LIBCLANG_PATH) {
    $env:LIBCLANG_PATH = "$env:ProgramFiles\LLVM\bin"
}
$env:Path = "$env:VULKAN_SDK\Bin;$env:LIBCLANG_PATH;$env:LOCALAPPDATA\Microsoft\WinGet\Links;$env:Path"
# Visual Studio 生成器在 whisper.cpp 嵌套的 vulkan-shaders-gen 子工程里找不到编译器，改用 Ninja
$env:CMAKE_GENERATOR = "Ninja"
# 与 Rust 的 +crt-static（src-tauri/.cargo/config.toml）保持一致，whisper.cpp 也用静态 CRT
$env:CMAKE_MSVC_RUNTIME_LIBRARY = "MultiThreaded"
# whisper.cpp 声明的最低 CMake 版本是 3.5，CMP0091 默认 OLD 会忽略上面这行，需显式打开
$env:CMAKE_POLICY_DEFAULT_CMP0091 = "NEW"

# 安装包附带 Vulkan Loader：没有 Vulkan 驱动的机器（虚拟机等）也能启动，自动退回 CPU
$vendor = Join-Path $PSScriptRoot "..\src-tauri\vendor"
if (-not (Test-Path "$vendor\vulkan-1.dll")) {
    New-Item -ItemType Directory -Force $vendor | Out-Null
    Copy-Item "$env:SystemRoot\System32\vulkan-1.dll" $vendor
}

Write-Host "Build environment ready (VS: $vs, Vulkan: $env:VULKAN_SDK)"
