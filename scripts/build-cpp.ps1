# Builds the C++ firmware (firmware/) with ESP-IDF 5.5.
#
# Usage: .\scripts\build-cpp.ps1 [-BuildDir C:\ikc]
#
# The build directory defaults to C:\ikc: ESP-IDF on Windows fails on long
# object paths, and the checkout usually lives deep in a workspace.
param(
    [string]$BuildDir
)

$ErrorActionPreference = "Stop"

# Locate ESP-IDF without hardcoding an install path: honor $env:IDF_PATH
# when set, else probe the conventional install locations and pick the
# newest match. (Mirrors scripts/build-rust.sh; the Windows side is
# unverified on a real toolchain.)
$idfRoot = $null
if ($env:IDF_PATH) {
    $idfRoot = $env:IDF_PATH
} else {
    $candidates = @()
    foreach ($pattern in @(
        (Join-Path $env:USERPROFILE "esp\esp-idf"),
        (Join-Path $env:USERPROFILE "esp\esp-idf-*"),
        (Join-Path $env:USERPROFILE ".espressif\frameworks\esp-idf-*"),
        "C:\Espressif\frameworks\esp-idf*"
    )) {
        $candidates += @(
            Get-Item -Path $pattern -ErrorAction SilentlyContinue |
                Where-Object { $_.PSIsContainer }
        )
    }
    $idfRoot = $candidates |
        Sort-Object FullName -Descending |
        Select-Object -First 1 -ExpandProperty FullName
}

if (-not $idfRoot -or -not (Test-Path (Join-Path $idfRoot "export.ps1"))) {
    throw "Could not locate ESP-IDF. Set `$env:IDF_PATH or install it conventionally (see README)."
}
$env:IDF_PATH = $idfRoot

# Tools dir follows the installer layout: C:\Espressif (offline installer)
# or ~\.espressif (idf.py online installer).
if (-not $env:IDF_TOOLS_PATH) {
    if (Test-Path "C:\Espressif") { $env:IDF_TOOLS_PATH = "C:\Espressif" }
    else { $env:IDF_TOOLS_PATH = Join-Path $env:USERPROFILE ".espressif" }
}

# Prepend the tools' python env and bundled git (export.ps1 needs both);
# each is optional - fall back to whatever is on PATH.
$prepend = @()
$pythonEnv = Get-ChildItem -Path (Join-Path $env:IDF_TOOLS_PATH "python_env") -Directory -ErrorAction SilentlyContinue |
    Sort-Object Name -Descending | Select-Object -First 1
if ($pythonEnv) { $prepend += (Join-Path $pythonEnv.FullName "Scripts") }
$gitCmd = Get-ChildItem -Path (Join-Path $env:IDF_TOOLS_PATH "tools\idf-git\*\cmd") -Directory -ErrorAction SilentlyContinue |
    Sort-Object FullName -Descending | Select-Object -First 1
if ($gitCmd) { $prepend += $gitCmd.FullName }
if ($prepend.Count -gt 0) {
    $env:Path = (($prepend -join ";") + ";" + $env:Path)
}

. (Join-Path $idfRoot "export.ps1")

if ($env:CARGO_HOME) {
    $cargoBin = Join-Path $env:CARGO_HOME "bin"
    if ((Test-Path $cargoBin) -and ($env:Path -notlike "*$cargoBin*")) {
        $env:Path = "$cargoBin;$env:Path"
    }
}

$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$project = Join-Path $repo "firmware"
if (-not (Test-Path (Join-Path $project "components\lvgl\lvgl.h"))) {
    Write-Host "Fetching the LVGL submodule"
    & git -C $repo submodule update --init --depth 1 firmware/components/lvgl
    if ($LASTEXITCODE -ne 0) { throw "git submodule update failed" }
}
if (-not $BuildDir) {
    $BuildDir = if ($env:INKWASH_CPP_BUILD_DIR) { $env:INKWASH_CPP_BUILD_DIR } else { "C:\ikc" }
}
& idf.py -C $project -B $BuildDir build
if ($LASTEXITCODE -ne 0) {
    throw "idf.py build failed with exit code $LASTEXITCODE"
}
Write-Host "Firmware image: $(Join-Path $BuildDir 'inkwash.bin')"
Write-Host "Flash it with: .\scripts\flash-note4.ps1 -Port COMx -Cpp -Monitor"
