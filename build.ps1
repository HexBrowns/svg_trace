# build.ps1 — svg_trace (mod2 + aux2) ビルド & デプロイ
param(
    [switch]$NoDeploy,
    [ValidateSet("all", "mod2", "aux2")]
    [string]$Target = "all"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$SrcDir = $PSScriptRoot
$AviUtl2Root = "C:\ProgramData\aviutl2"
$ScriptDir = Join-Path $AviUtl2Root "Script\svg_trace"
$PluginDir = Join-Path $AviUtl2Root "Plugin\svg_trace"
$TargetDir = Join-Path $SrcDir "target\release"

Write-Host "`n=== svg_trace ビルド (Rust) Target=$Target ===" -ForegroundColor Yellow

Push-Location $SrcDir
try {
    if ($Target -eq "all" -or $Target -eq "mod2") {
        cargo build --release -p svg-trace-mod2
        if ($LASTEXITCODE -ne 0) { throw "mod2 build failed ($LASTEXITCODE)" }
    }
    if ($Target -eq "all" -or $Target -eq "aux2") {
        cargo build --release -p svg-trace-aux2
        if ($LASTEXITCODE -ne 0) { throw "aux2 build failed ($LASTEXITCODE)" }
    }
} finally {
    Pop-Location
}

$ModDll = Join-Path $TargetDir "svg_trace.dll"
$AuxDll = Join-Path $TargetDir "svg_trace_aux.dll"

if ($Target -eq "all" -or $Target -eq "mod2") {
    if (-not (Test-Path $ModDll)) { Write-Error "missing $ModDll" }
    $ModOut = Join-Path $SrcDir "svg_trace.mod2"
    Copy-Item -Path $ModDll -Destination $ModOut -Force
    Write-Host "  完了: $ModOut" -ForegroundColor Green
}

if ($Target -eq "all" -or $Target -eq "aux2") {
    if (-not (Test-Path $AuxDll)) { Write-Error "missing $AuxDll" }
    $AuxOut = Join-Path $SrcDir "svg_trace.aux2"
    Copy-Item -Path $AuxDll -Destination $AuxOut -Force
    Write-Host "  完了: $AuxOut" -ForegroundColor Green
}

if (-not $NoDeploy) {
    Write-Host "`n=== デプロイ ===" -ForegroundColor Yellow
    if (-not (Test-Path $ScriptDir)) {
        New-Item -ItemType Directory -Path $ScriptDir -Force | Out-Null
    }
    if (-not (Test-Path $PluginDir)) {
        New-Item -ItemType Directory -Path $PluginDir -Force | Out-Null
    }

    if ($Target -eq "all" -or $Target -eq "mod2") {
        Copy-Item -Path (Join-Path $SrcDir "svg_trace.mod2") -Destination (Join-Path $ScriptDir "svg_trace.mod2") -Force
        Write-Host "  配置: $(Join-Path $ScriptDir 'svg_trace.mod2')" -ForegroundColor DarkGreen
    }

    $Obj2Src = Join-Path $SrcDir "scripts\SVG_TRACE.obj2"
    if (Test-Path $Obj2Src) {
        Copy-Item -Path $Obj2Src -Destination (Join-Path $ScriptDir "SVG_TRACE.obj2") -Force
        Write-Host "  配置: $(Join-Path $ScriptDir 'SVG_TRACE.obj2')" -ForegroundColor DarkGreen
    }

    if ($Target -eq "all" -or $Target -eq "aux2") {
        Copy-Item -Path (Join-Path $SrcDir "svg_trace.aux2") -Destination (Join-Path $PluginDir "svg_trace.aux2") -Force
        Write-Host "  配置: $(Join-Path $PluginDir 'svg_trace.aux2')" -ForegroundColor DarkGreen
    }

    $ExportDir = Join-Path $PluginDir "export"
    if (-not (Test-Path $ExportDir)) {
        New-Item -ItemType Directory -Path $ExportDir -Force | Out-Null
    }
}

Write-Host "`n全ビルド完了`n" -ForegroundColor Yellow
