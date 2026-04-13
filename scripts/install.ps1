#Requires -Version 5.1
<#
.SYNOPSIS
    BeeOS vnc-bridge installer for Windows.

.DESCRIPTION
    Downloads the vnc-bridge binary from GitHub Release, writes a config file,
    and optionally registers a Windows Service via sc.exe.

.EXAMPLE
    .\install.ps1 -Mqtt wss://mqtt.beeos.ai/mqtt -Token <TOKEN> -Topic devices/<ID>

.EXAMPLE
    # One-liner (PowerShell 5+):
    irm https://raw.githubusercontent.com/beeos-ai/vnc-bridge/main/scripts/install.ps1 | iex
#>
param(
    [Parameter(Mandatory)][string]$Mqtt,
    [Parameter(Mandatory)][string]$Token,
    [Parameter(Mandatory)][string]$Topic,
    [string]$VncAddr    = "127.0.0.1:5900",
    [string]$IceServers = "[]",
    [string]$Version    = "latest",
    [string]$InstallDir = "$env:ProgramFiles\BeeOS"
)

$ErrorActionPreference = "Stop"
$Repo = "beeos-ai/vnc-bridge"
$Target = "x86_64-pc-windows-msvc"

# --- Resolve version ---
if ($Version -eq "latest") {
    $redirect = (Invoke-WebRequest -Uri "https://github.com/$Repo/releases/latest" `
        -MaximumRedirection 0 -ErrorAction SilentlyContinue -UseBasicParsing `
        ).Headers.Location 2>$null
    if ($redirect -match 'v(\d+\.\d+\.\d+)') {
        $Version = $Matches[1]
    } else {
        $latestApi = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -UseBasicParsing
        $Version = $latestApi.tag_name -replace '^v', ''
    }
    Write-Host "Resolved latest version: $Version"
}

$DownloadUrl = "https://github.com/$Repo/releases/download/v$Version/vnc-bridge-$Version-$Target.zip"

# --- Download and extract ---
$TempZip = Join-Path $env:TEMP "vnc-bridge.zip"
$TempDir = Join-Path $env:TEMP "vnc-bridge-extract"

Write-Host "Downloading vnc-bridge v$Version for Windows x86_64..."
Invoke-WebRequest -Uri $DownloadUrl -OutFile $TempZip -UseBasicParsing

if (Test-Path $TempDir) { Remove-Item $TempDir -Recurse -Force }
Expand-Archive -Path $TempZip -DestinationPath $TempDir -Force

# --- Install binary ---
if (!(Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

$ExeSrc = Join-Path $TempDir "vnc-bridge.exe"
$ExeDst = Join-Path $InstallDir "vnc-bridge.exe"
Copy-Item $ExeSrc -Destination $ExeDst -Force
Write-Host "Binary installed to $ExeDst"

# --- Add to PATH if needed ---
$machinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
if ($machinePath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$machinePath;$InstallDir", "Machine")
    Write-Host "Added $InstallDir to system PATH (restart shell to take effect)"
}

# --- Write config ---
$ConfigDir = Join-Path $env:APPDATA "BeeOS"
if (!(Test-Path $ConfigDir)) {
    New-Item -ItemType Directory -Path $ConfigDir -Force | Out-Null
}
$ConfigFile = Join-Path $ConfigDir "vnc-bridge.env"

@"
VNC_ADDR=$VncAddr
MQTT_BROKER_URL=$Mqtt
MQTT_TOKEN=$Token
DEVICE_TOPIC=$Topic
ICE_SERVERS=$IceServers
"@ | Set-Content -Path $ConfigFile -Encoding UTF8

Write-Host "Config saved to $ConfigFile"

# --- Cleanup temp files ---
Remove-Item $TempZip -Force -ErrorAction SilentlyContinue
Remove-Item $TempDir -Recurse -Force -ErrorAction SilentlyContinue

# --- Register Windows Service (best-effort) ---
$ServiceName = "BeeOSVncBridge"
$BinPath = "`"$ExeDst`" --vnc $VncAddr --mqtt $Mqtt --token $Token --topic $Topic --ice-servers `'$IceServers`'"

$existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host "Stopping existing service..."
    Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
    sc.exe delete $ServiceName | Out-Null
    Start-Sleep -Seconds 1
}

try {
    sc.exe create $ServiceName binPath= $BinPath start= auto displayname= "BeeOS VNC Bridge" | Out-Null
    sc.exe description $ServiceName "VNC TCP to WebRTC DataChannel bridge with MQTT signaling" | Out-Null
    sc.exe start $ServiceName | Out-Null
    Write-Host ""
    Write-Host "Windows Service '$ServiceName' installed and started."
    Write-Host "  Status:  sc.exe query $ServiceName"
    Write-Host "  Stop:    sc.exe stop $ServiceName"
    Write-Host "  Remove:  sc.exe delete $ServiceName"
} catch {
    Write-Host ""
    Write-Host "Could not register Windows Service (requires Administrator)."
    Write-Host "Run manually:"
    Write-Host "  vnc-bridge.exe --vnc $VncAddr --mqtt $Mqtt --token `$Token --topic $Topic --ice-servers '$IceServers'"
}

Write-Host ""
Write-Host "vnc-bridge installation complete!"
