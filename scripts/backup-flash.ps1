param(
  [Parameter(Mandatory = $true)]
  [string]$Port,

  [string]$OutDir = "backups",
  [int]$Baud = 921600
)

$ErrorActionPreference = "Stop"
$AuthorizedMac = "20:6E:F1:B4:7D:E4"
$FlashSizeBytes = 0x1000000

function Invoke-EsptoolProbe {
  param([string[]]$CommandArgs)

  $output = (& esptool.py @CommandArgs 2>&1 | Out-String)
  if ($LASTEXITCODE -ne 0) {
    throw "esptool probe failed with exit code $LASTEXITCODE`n$output"
  }
  return $output
}

$identity = Invoke-EsptoolProbe -CommandArgs @("--chip", "esp32s3", "--port", $Port, "read_mac")
if ($identity -notmatch "(?im)^Chip is ESP32-S3\b") {
  throw "Refusing backup: $Port is not identified as an ESP32-S3."
}
if ($identity -notmatch "(?im)^MAC:\s*20:6e:f1:b4:7d:e4\s*$") {
  throw "Refusing backup: $Port does not have authorized Note 4 MAC $AuthorizedMac."
}

$flash = Invoke-EsptoolProbe -CommandArgs @("--chip", "esp32s3", "--port", $Port, "flash_id")
if ($flash -notmatch "(?im)^Detected flash size:\s*16MB\s*$") {
  throw "Refusing backup: $Port is not identified as a 16 MB Flash device."
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$timestamp = Get-Date -Format "yyyyMMdd-HHmmss"
$outFile = Join-Path $OutDir "note4-factory-$timestamp.bin"

Write-Host "Reading full 16 MiB flash from $Port to $outFile"
& esptool.py --chip esp32s3 --port $Port --baud $Baud read_flash 0x0 $FlashSizeBytes $outFile
if ($LASTEXITCODE -ne 0) {
  throw "Flash backup failed with exit code $LASTEXITCODE"
}

$backup = Get-Item -LiteralPath $outFile
if ($backup.Length -ne $FlashSizeBytes) {
  throw "Flash backup has unexpected size $($backup.Length); expected $FlashSizeBytes bytes."
}
$sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $outFile).Hash.ToLowerInvariant()
$manifest = [ordered]@{
  device = "Zectrix Note 4"
  chip = "ESP32-S3"
  mac = $AuthorizedMac
  flash_size_bytes = $FlashSizeBytes
  port = $Port
  captured_at_utc = (Get-Date).ToUniversalTime().ToString("o")
  image = $backup.Name
  image_size_bytes = $backup.Length
  sha256 = $sha256
}
$manifestFile = "$outFile.json"
$manifest | ConvertTo-Json | Set-Content -LiteralPath $manifestFile -Encoding utf8

Write-Host "Backup complete: $outFile"
Write-Host "Manifest: $manifestFile"
