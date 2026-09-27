# Flashes a Zectrix Note 4 only after proving the port holds that exact board.
# PowerShell counterpart of scripts/flash-note4.sh; the checks are the same.
#
# Usage:
#   .\scripts\flash-note4.ps1 -Port COM5 [-Monitor] [-Elf D:\espbuild\...\inkwash-note4]
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
    [switch]$Monitor
)

$ErrorActionPreference = "Stop"
$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$authorizedMac = if ($env:INKWASH_NOTE4_MAC) { $env:INKWASH_NOTE4_MAC } else { "20:6E:F1:B4:7D:E4" }

if (-not $Elf) {
    # Same default as scripts\build-rust.ps1.
    $targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { "C:\ikw" }
    $Elf = Join-Path $targetRoot "xtensa-esp32s3-espidf\release\inkwash-note4"
}
$bootloader = Join-Path (Split-Path -Parent $Elf) "bootloader.bin"
$partitions = Join-Path $repo "rust-firmware\partitions.csv"

foreach ($file in @($Elf, $bootloader, $partitions)) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) {
        throw "Refusing to flash: $file not found (build with scripts\build-rust.ps1 -Release)."
    }
}
if ($authorizedMac -notmatch '^([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}$') {
    throw "Refusing to flash: INKWASH_NOTE4_MAC '$authorizedMac' is not a MAC address."
}

if (Get-Command esptool.py -ErrorAction SilentlyContinue) {
    $esptool = @("esptool.py")
} elseif (Get-Command esptool -ErrorAction SilentlyContinue) {
    $esptool = @("esptool")
} else {
    throw "Refusing to flash: esptool is required to identify the board (run from an ESP-IDF shell)."
}
if (-not (Get-Command espflash -ErrorAction SilentlyContinue)) {
    throw "Refusing to flash: espflash is required (cargo install espflash)."
}

function Invoke-Probe {
    param([string[]]$ProbeArgs)
    $exe = $esptool[0]
    $output = (& $exe @($esptool | Select-Object -Skip 1) --chip esp32s3 --port $Port @ProbeArgs 2>&1 | Out-String)
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
