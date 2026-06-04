<#
.SYNOPSIS
  清理 Electron 打包中的无用文件，大幅减小体积
.DESCRIPTION
  删除不需要的语言包、显卡驱动、视频解码等文件
#>
param(
    [string]$AppDir = "dist\win-unpacked"
)

$target = Join-Path (Get-Location) $AppDir
if (!(Test-Path $target)) {
    Write-Host "目录不存在: $target" -ForegroundColor Red
    exit 1
}

Write-Host "=== 清理 Electron 无用文件 ===" -ForegroundColor Cyan
Write-Host "目标: $target"

$deletedBytes = 0

# 1. 保留 zh-CN + en-US，删除其余 53 个语言包
$localeDir = Join-Path $target "locales"
if (Test-Path $localeDir) {
    $keep = @("zh-CN.pak", "en-US.pak")
    Get-ChildItem $localeDir -Filter *.pak | Where-Object { $_.Name -notin $keep } | ForEach-Object {
        $deletedBytes += $_.Length
        Remove-Item $_.FullName -Force
        Write-Host "  删语言包: $($_.Name) ($( [math]::Round($_.Length/1KB) ) KB)" -ForegroundColor DarkGray
    }
    Write-Host "  保留: $($keep -join ', ')" -ForegroundColor Green
}

# 2. 删除 DirectX 编译器等无用 DLL
$unusedDlls = @(
    "dxcompiler.dll",
    "vk_swiftshader.dll",
    "libGLESv2.dll",
    "libEGL.dll",
    "dxil.dll",
    "ffmpeg.dll"
)
foreach ($dll in $unusedDlls) {
    $path = Join-Path $target $dll
    if (Test-Path $path) {
        $len = (Get-Item $path).Length
        $deletedBytes += $len
        Remove-Item $path -Force
        Write-Host "  删 DLL: $dll ($( [math]::Round($len/1MB,1) ) MB)" -ForegroundColor DarkGray
    }
}

# 3. d3dcompiler_47.dll - 系统自带，可删
$d3dPath = Join-Path $target "d3dcompiler_47.dll"
if (Test-Path $d3dPath) {
    $len = (Get-Item $d3dPath).Length
    $deletedBytes += $len
    Remove-Item $d3dPath -Force
    Write-Host "  删 DLL: d3dcompiler_47.dll ($( [math]::Round($len/1MB,1) ) MB)" -ForegroundColor DarkGray
}

# 4. 删除 elevate.exe
$elevatePath = Join-Path $target "resources\elevate.exe"
if (Test-Path $elevatePath) {
    $len = (Get-Item $elevatePath).Length
    $deletedBytes += $len
    Remove-Item $elevatePath -Force
    Write-Host "  删: elevate.exe ($( [math]::Round($len/1KB) ) KB)" -ForegroundColor DarkGray
}

$savedMB = [math]::Round($deletedBytes / 1MB, 1)
Write-Host "`n=== 共释放 $savedMB MB ===" -ForegroundColor Green

$finalSize = (Get-ChildItem $target -Recurse -File | Measure-Object -Property Length -Sum).Sum
Write-Host "最终安装目录大小: $( [math]::Round($finalSize/1MB, 1) ) MB" -ForegroundColor Cyan

$exePath = Join-Path $target "self_clipboard.exe"
if (Test-Path $exePath) {
    Write-Host "`n自检通过: exe 存在" -ForegroundColor Green
}
