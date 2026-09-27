# Flashes a Zectrix Note 4 only after proving the port holds that exact board.
# PowerShell counterpart of scripts/flash-note4.sh; the checks are the same.
#
# Usage:
#   .\scripts\flash-note4.ps1 -Port COM5 [-Monitor] [-Elf D:\espbuild\...\inkwash-note4]
#   .\scripts\flash-note4.ps1 -Port COM5 -Cpp [-Monitor] [-BuildDir C:\ikc]
#
# -Cpp flashes the C++ firmware built by scripts\build-cpp.ps1 instead of the
# Rust one. The identity checks are the same for both.
#
# A serial-port name is not an identity: another ESP32, or a Note 4C with a
# different panel, may enumerate on the same COM port. Before writing anything
# this script requires an ESP32-S3 with 16 MB flash whose MAC is the authorized
# Note 4 ($env:INKWASH_NOTE4_MAC, default below), then flashes it as 16 MB DIO
# 80 MHz with this repository's partition table.
param(
    [Parameter(Mandatory = $true)]
    [string]$Port,
    [string]$Elf,
    [switch]$Monitor,
    [switch]$Cpp,
    [string]$BuildDir
)

$ErrorActionPreference = "Stop"
$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$authorizedMac = if ($env:INKWASH_NOTE4_MAC) { $env:INKWASH_NOTE4_MAC } else { "20:6E:F1:B4:7D:E4" }

if ($Cpp) {
    if (-not $BuildDir) {
        $BuildDir = if ($env:INKWASH_CPP_BUILD_DIR) { $env:INKWASH_CPP_BUILD_DIR } else { "C:\ikc" }
    }
    $cppFiles = @("flash_args", "bootloader\bootloader.bin", "partition_table\partition-table.bin", "inkwash.bin")
    foreach ($file in $cppFiles) {
        if (-not (Test-Path -LiteralPath (Join-Path $BuildDir $file) -PathType Leaf)) {
            throw "Refusing to flash: $(Join-Path $BuildDir $file) not found (build with scripts\build-cpp.ps1)."
        }
    }
    $flashArgs = Get-Content (Join-Path $BuildDir "flash_args") -Raw
    if ($flashArgs -notmatch '--flash_mode dio' -or $flashArgs -notmatch '--flash_freq 80m' -or $flashArgs -notmatch '--flash_size 16MB') {
        throw "Refusing to flash: $BuildDir\flash_args is not DIO / 80 MHz / 16 MB."
    }
} elseif (-not $Elf) {
    # Same default as scripts\build-rust.ps1.
    $targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { "C:\ikw" }
    $Elf = Join-Path $targetRoot "xtensa-esp32s3-espidf\release\inkwash-note4"
}
$bootloader = if ($Cpp) { $null } else { Join-Path (Split-Path -Parent $Elf) "bootloader.bin" }
$partitions = Join-Path $repo "rust-firmware\partitions.csv"

$required = if ($Cpp) { @($partitions) } else { @($Elf, $bootloader, $partitions) }
foreach ($file in $required) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) {
        throw "Refusing to flash: $file not found (build with scripts\build-rust.ps1 -Release)."
    }
}
if ($authorizedMac -notmatch '^([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}$') {
    throw "Refusing to flash: INKWASH_NOTE4_MAC '$authorizedMac' is not a MAC address."
}

if (Get-Command esptool.py -ErrorAction SilentlyContinue) {
    $esptool = "esptool.py"
} elseif (Get-Command esptool -ErrorAction SilentlyContinue) {
    $esptool = "esptool"
} else {
    throw "Refusing to flash: esptool is required to identify the board (run from an ESP-IDF shell)."
}
if (-not $Cpp -and -not (Get-Command espflash -ErrorAction SilentlyContinue)) {
    throw "Refusing to flash: espflash is required (cargo install espflash)."
}

function Invoke-Probe {
    param([string[]]$ProbeArgs)
    $output = (& $esptool --chip esp32s3 --port $Port @ProbeArgs 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) {
        Write-Host $output
        throw "Refusing to flash: esptool could not probe $Port."
    }
    return $output
}

$identity = Invoke-Probe @("read_mac")
# esptool 4 prints "Chip is ESP32-S3 ...", esptool 5 "Chip type: ESP32-S3 ...".
if ($identity -notmatch '(?im)^(Chip is|Chip type:)\s*ESP32-S3\b') {
    throw "Refusing to flash: $Port is not identified as an ESP32-S3."
}
$macMatch = [regex]::Match($identity, '(?im)^MAC:\s*((?:[0-9a-f]{2}:){5}[0-9a-f]{2})')
$mac = if ($macMatch.Success) { $macMatch.Groups[1].Value } else { "unknown" }
if ($mac.ToLowerInvariant() -ne $authorizedMac.ToLowerInvariant()) {
    throw "Refusing to flash: $Port has MAC '$mac', not the authorized Note 4 $authorizedMac."
}
$flash = Invoke-Probe @("flash_id")
if ($flash -notmatch '(?im)^Detected flash size:\s*16MB\s*$') {
    throw "Refusing to flash: $Port is not identified as a 16 MB flash device."
}

if ($Cpp) {
    Write-Host "==> $Port is the authorized Note 4 ($mac); flashing the C++ firmware from $BuildDir"
    Push-Location $BuildDir
    try {
        & $esptool --chip esp32s3 --port $Port --baud 921600 --before default_reset --after hard_reset write_flash '@flash_args'
        if ($LASTEXITCODE -ne 0) { throw "esptool write_flash failed with exit code $LASTEXITCODE" }
    } finally {
        Pop-Location
    }
    if ($Monitor) {
        # DTR/RTS stay low: on the USB Serial/JTAG port they reset the chip.
        & python -m serial.tools.miniterm --raw --dtr 0 --rts 0 $Port 115200
    }
    return
}

Write-Host "==> $Port is the authorized Note 4 ($mac); flashing $Elf"
$flashArgs = @(
    "flash", "--port", $Port, "--chip", "esp32s3",
    "--flash-size", "16mb", "--flash-mode", "dio", "--flash-freq", "80mhz",
    "--bootloader", $bootloader,
    "--partition-table", $partitions, "--partition-table-offset", "0x10000",
    "--non-interactive"
)
if ($Monitor) { $flashArgs += "--monitor" }
$flashArgs += $Elf
& espflash @flashArgs
if ($LASTEXITCODE -ne 0) {
    throw "espflash failed with exit code $LASTEXITCODE"
}
