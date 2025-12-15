# Dante Time Sync Installer for Windows
# Run as Administrator in PowerShell

$ErrorActionPreference = "Stop"

$RepoOwner = "zbynekdrlik"
$RepoName = "dantetimesync"
$InstallDir = "C:\Program Files\DanteTimeSync"
$ServiceName = "dantetimesync"

Write-Host ">>> Dante Time Sync Windows Installer <<<" -ForegroundColor Cyan

# 1. Check for Npcap/WinPcap
if (!(Test-Path "C:\Windows\System32\Packet.dll")) {
    Write-Warning "Npcap or WinPcap does not appear to be installed (Packet.dll missing)."
    Write-Host "Please install Npcap from https://npcap.com/dist/npcap-1.79.exe (Select 'Install Npcap in WinPcap API-compatible Mode')" -ForegroundColor Yellow
    Write-Host "Press Enter to continue if you have installed it, or Ctrl+C to exit..."
    Read-Host
}

# 2. Create Directory
if (!(Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

# 3. Download Latest Release
Write-Host "Fetching latest release..."
try {
    $LatestReleaseUrl = "https://api.github.com/repos/$RepoOwner/$RepoName/releases/latest"
    $ReleaseInfo = Invoke-RestMethod -Uri $LatestReleaseUrl
} catch {
    Write-Error "Failed to fetch release info. Check internet connection."
}

$Asset = $ReleaseInfo.assets | Where-Object { $_.name -like "*windows-amd64.exe" }
$TrayAsset = $ReleaseInfo.assets | Where-Object { $_.name -like "*dantetray-windows-amd64.exe" }

if (!$Asset) {
    Write-Error "Could not find Windows asset in latest release."
}

$ExePath = "$InstallDir\dantetimesync.exe"
$TrayPath = "$InstallDir\dantetray.exe"

Write-Host "Downloading $($Asset.name)..."
Invoke-WebRequest -Uri $Asset.browser_download_url -OutFile $ExePath

if ($TrayAsset) {
    Write-Host "Downloading $($TrayAsset.name)..."
    Invoke-WebRequest -Uri $TrayAsset.browser_download_url -OutFile $TrayPath
} else {
    Write-Warning "Tray application not found in release."
}

# 4. Install Service
Write-Host "Installing Service..."
# Stop if exists
$Service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($Service) {
    Stop-Service -Name $ServiceName -Force
    Start-Sleep -Seconds 2
    sc.exe delete $ServiceName
    Start-Sleep -Seconds 1
}

# Create Service
# binPath needs to include --service flag
$BinPath = "`"$ExePath`" --service"
sc.exe create $ServiceName binPath= $BinPath start= auto DisplayName= "Dante Time Sync"
sc.exe description $ServiceName "Synchronizes system time with Dante PTP Master"

# 5. Start Service
Write-Host "Starting Service..."
Start-Service -Name $ServiceName

# 6. Setup Tray App (Startup)
if (Test-Path $TrayPath) {
    Write-Host "Setting up Tray App to run at startup..."
    $Trigger = New-ScheduledTaskTrigger -AtLogon
    $Action = New-ScheduledTaskAction -Execute $TrayPath
    $Principal = New-ScheduledTaskPrincipal -GroupId "BUILTIN\Users" -RunLevel Highest
    Register-ScheduledTask -TaskName "DanteTray" -Trigger $Trigger -Action $Action -Principal $Principal -Force | Out-Null
    
    Write-Host "Starting Tray App..."
    Start-Process -FilePath $TrayPath
}

Write-Host "Installation Complete!" -ForegroundColor Green
Write-Host "Service '$ServiceName' is running."