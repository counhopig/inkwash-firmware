param(
    [switch]$Release
)

$ErrorActionPreference = "Stop"
$projectDir = Join-Path $PSScriptRoot "..\rust-firmware"

# Locate ESP-IDF without hardcoding an install path: honor $env:IDF_PATH
# when set, else probe the conventional install locations and pick the
# newest match. (Mirrors scripts/build-rust.sh; Windows side remains
# unverified on a real toolchain - see docs.)
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
        $candidates += @(Get-Item -Path $pattern -Directory -ErrorAction SilentlyContinue)
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

if (-not $env:LIBCLANG_PATH) {
    # esp-idf-sys's bindgen step needs espup's esp-clang, which clang-sys does
    # not find on its own. Locate it under the active `esp` rustup toolchain
    # instead of hardcoding a path, so this works on any machine.
    $espToolchain = (rustup toolchain list -v 2>$null) |
        Where-Object { $_ -match '^esp\s' } |
        ForEach-Object { ($_ -split '\s+')[-1] } |
        Select-Object -First 1

    if ($espToolchain) {
        $clangDir = Get-ChildItem -Path (Join-Path $espToolchain "xtensa-esp32-elf-clang") -Filter "esp-clang" -Recurse -Directory -ErrorAction SilentlyContinue |
            ForEach-Object {
                Get-ChildItem -Path $_.FullName -Include "bin", "lib" -Directory -ErrorAction SilentlyContinue |
                    Where-Object { Get-ChildItem -Path $_.FullName -Filter "libclang.dll" -ErrorAction SilentlyContinue }
            } |
            Select-Object -First 1 -ExpandProperty FullName

        if ($clangDir) {
            $env:LIBCLANG_PATH = $clangDir
        }
    }

    if (-not $env:LIBCLANG_PATH) {
        throw "Could not locate esp-clang's libclang.dll under the 'esp' rustup toolchain. Install it with 'espup install', or set LIBCLANG_PATH manually."
    }
}

Push-Location $projectDir
try {
    $cargoArgs = @("build")
    if ($Release) {
        $cargoArgs += "--release"
    }

    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}
