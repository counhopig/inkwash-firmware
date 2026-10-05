# Flash the authorized Note 4 from an ESP-IDF C++ build.
param(
    [Parameter(Mandatory = $true)]
    [string]$Port,
    [switch]$Monitor,
    [string]$BuildDir
)
$ErrorActionPreference = "Stop"
$repo = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$authorizedMac = "20:6E:F1:B4:7D:E4"
if (-not $BuildDir) {
    $BuildDir = if ($env:INKWASH_CPP_BUILD_DIR) { $env:INKWASH_CPP_BUILD_DIR } else { "C:\ikc" }
}
$partitions = Join-Path $repo "firmware\partitions.csv"
foreach ($file in @("flash_args", "bootloader\bootloader.bin", "partition_table\partition-table.bin", "inkwash.bin")) {
    if (-not (Test-Path -LiteralPath (Join-Path $BuildDir $file) -PathType Leaf)) {
        throw "Refusing to flash: $file not found (build with scripts\build\build-cpp.ps1)."
    }
}
$flashArgs = Get-Content (Join-Path $BuildDir "flash_args") -Raw
if ($flashArgs -notmatch '--flash_mode dio' -or $flashArgs -notmatch '--flash_freq 80m' -or $flashArgs -notmatch '--flash_size 16MB') {
    throw "Refusing to flash: build is not DIO / 80 MHz / 16 MB."
}
if ((Get-FileHash -Algorithm SHA256 $partitions).Hash -ne "b3a31833dfa23b985b89821768fdf4cf5d3b4e23c52a815c551c79e30fc4de74") {
    throw "Refusing to flash: partition layout differs from the authorized Note 4 table."
}
if (-not $env:IDF_PATH) { throw "Export the ESP-IDF environment first." }
$generator = Join-Path $env:IDF_PATH "components\partition_table\gen_esp32part.py"
if (-not (Test-Path -LiteralPath $generator)) { throw "Export the ESP-IDF environment first." }
$expectedTable = [System.IO.Path]::GetTempFileName()
try {
    & python $generator $partitions $expectedTable
    if ($LASTEXITCODE -ne 0) { throw "Partition table generation failed." }
    $builtTable = Join-Path $BuildDir "partition_table\partition-table.bin"
    if ((Get-FileHash -Algorithm SHA256 $expectedTable).Hash -ne (Get-FileHash -Algorithm SHA256 $builtTable).Hash) {
        throw "Refusing to flash: generated partition table does not match the build."
    }
} finally { Remove-Item -LiteralPath $expectedTable -ErrorAction SilentlyContinue }
if (Get-Command esptool.py -ErrorAction SilentlyContinue) { $esptool = "esptool.py" }
elseif (Get-Command esptool -ErrorAction SilentlyContinue) { $esptool = "esptool" }
else { throw "Run from an ESP-IDF shell with esptool installed." }

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

Write-Host "==> $Port is the authorized Note 4 ($mac); flashing C++ firmware"
& $esptool --chip esp32s3 --port $Port --baud 921600 write_flash --flash_size 16MB --flash_mode dio --flash_freq 80m 0x0 (Join-Path $BuildDir "bootloader\bootloader.bin") 0x10000 (Join-Path $BuildDir "partition_table\partition-table.bin") 0x30000 (Join-Path $BuildDir "inkwash.bin")
if ($LASTEXITCODE -ne 0) { throw "esptool write_flash failed: $LASTEXITCODE" }
if ($Monitor) { & python -m serial.tools.miniterm --raw --dtr 0 --rts 0 $Port 115200 }
