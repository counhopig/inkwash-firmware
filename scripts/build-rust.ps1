param(
    [switch]$Release,
    [switch]$Diagnostic,
    # esp-idf-sys refuses to build on Windows when its output directory path
    # is too long (it asks for a project path of at most 10 characters, and a
    # `subst` drive does not help). Build into a short directory instead of
    # rust-firmware\target. Override with -TargetDir or $env:CARGO_TARGET_DIR.
    [string]$TargetDir
)

$ErrorActionPreference = "Stop"
$projectDir = Join-Path $PSScriptRoot "..\rust-firmware"

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
    if (-not $TargetDir) {
        $TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { "C:\ikw" }
    }
    $defaults = @("sdkconfig.defaults")
    if ($Diagnostic) {
        $defaults += "sdkconfig.diagnostic.defaults"
        # Keep diagnostic objects apart from production ones, as on Linux.
        $TargetDir = $TargetDir.TrimEnd('\') + "-d"
    }
    $targetRoot = $TargetDir
    $env:CARGO_TARGET_DIR = $targetRoot
    Write-Host "Cargo target directory: $targetRoot"

    $partitionHash = "v2:" + (Get-FileHash -Algorithm SHA256 "partitions.csv").Hash.ToLowerInvariant()
    $partitionStamp = Join-Path $targetRoot ".inkwash-partitions.sha256"
    if ((Test-Path $targetRoot) -and
        ((-not (Test-Path $partitionStamp)) -or
         ((Get-Content $partitionStamp -Raw).Trim() -ne $partitionHash))) {
        & cargo clean
        if ($LASTEXITCODE -ne 0) {
            throw "cargo clean for partition-table rebuild failed with exit code $LASTEXITCODE"
        }
    }

    # sdkconfig.defaults reaches partitions.csv by a path relative to
    # rust-firmware\target, which is wrong for any other target directory.
    # Override it with the absolute path in a generated defaults file, loaded
    # last so it wins. Written after any `cargo clean`, which would delete it.
    New-Item -ItemType Directory -Force $targetRoot | Out-Null
    $partitionsCsv = (Resolve-Path "partitions.csv").Path -replace '\\', '/'
    $partitionDefaults = Join-Path (Resolve-Path $targetRoot).Path "partitions.sdkconfig.defaults"
    Set-Content -Path $partitionDefaults -Encoding Ascii -Value @(
        "CONFIG_PARTITION_TABLE_CUSTOM_FILENAME=`"$partitionsCsv`"",
        "CONFIG_PARTITION_TABLE_FILENAME=`"$partitionsCsv`""
    )
    $defaults += $partitionDefaults
    $env:ESP_IDF_SDKCONFIG_DEFAULTS = $defaults -join ";"

    $cargoArgs = @("build")
    if ($Release) {
        $cargoArgs += "--release"
    }

    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }
    New-Item -ItemType Directory -Force $targetRoot | Out-Null
    Set-Content -Path $partitionStamp -Value $partitionHash -Encoding Ascii
    $profileDir = if ($Release) { "release" } else { "debug" }
    Write-Host "Firmware ELF: $(Join-Path $targetRoot "xtensa-esp32s3-espidf\$profileDir\inkwash-note4")"
} finally {
    Pop-Location
}
