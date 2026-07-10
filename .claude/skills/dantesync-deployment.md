---
name: dantesync-deployment
description: Deploy/upgrade DanteSync to the managed fleet (Linux cam boxes + Windows strih/stream). Use when releasing a new version, upgrading the fleet, or fixing a stuck tray icon.
---

# DanteSync Deployment Skill

**REWRITTEN 2026-07-10 (#47 rollout)** — the previous version of this file said "NEVER
manually copy files and restart services" and pointed at `install.ps1`/`install.sh`
as the ONLY sanctioned path. That is stale: a direct stop-service → swap-binary →
start-service deploy (no reinstall, no reboot) is the actual, verified, standard
procedure — it's what `dantesync-ownership.md`'s "Release/deploy path" already
documented, and what a live 9-box fleet rollout (cam1-6, imag-nb, strih, stream) just
confirmed end-to-end (v1.8.18, #47). Reserve `install.ps1`/`install.sh` for a
genuinely FRESH machine that has never had DanteSync installed (they also lay down
the systemd unit / scheduled task / registry autostart from scratch) — for a routine
version bump on an already-provisioned box, the direct swap below is simpler, faster,
and does not touch autostart/service registration at all.

## Current fleet (verify against camera-box's `targets.md` if in doubt — that file is
the canonical IP list; this is a quick reference, not the source of truth)

| Box | IP | Platform | Transport |
|---|---|---|---|
| cam1 | 10.77.9.61 | Linux (root) | `mcp__linux-cam1__Shell` |
| cam2 | 10.77.9.62 | Linux (root) | `mcp__linux-cam2__Shell` |
| cam3 | 10.77.9.63 | Linux (root) | `mcp__linux-cam3__Shell` — **read-only root fs**, see gotcha below |
| cam4 | 10.77.9.64 | Linux (root) | `mcp__linux-cam4__Shell` |
| cam5 | 10.77.9.65 | Linux (root) | no MCP server — `sshpass -p "$DEVICE_ROOT_PW" ssh root@10.77.9.65` |
| cam6 | 10.77.9.66 | Linux (root) | no MCP server — `sshpass -p "$DEVICE_ROOT_PW" ssh root@10.77.9.66` |
| imag-nb | 10.77.9.182 | Linux (`newlevel` user) | `mcp__linux-imag-nb__Shell` — **sudo needs a password**, see gotcha below |
| strih | 10.77.9.202 | Windows (service `dantesync` + tray) | `mcp__win-strih__Shell` |
| stream | 10.77.9.204 | Windows (service `dantesync` + tray) | `mcp__win-stream-snv__Shell` |

Credentials: cam1-6 SSH is root/`$DEVICE_ROOT_PW` (the standard cam-fleet root
password — see camera-box's own ops skill / memory for the value, never hardcode it
in a committed file). imag-nb SSH/sudo is `newlevel`/`$IMAG_NB_PW` (same value as its
own user login password). Windows boxes are reached via MCP only in this procedure
(no SSH needed) — the MCP session already authenticates.

## Canary-first release rollout (the standard procedure — #47 proved this live)

1. **Build + release**: bump `Cargo.toml` version, merge dev→master. CI's
   `auto-release` job auto-tags `vX.Y.Z` and triggers `release.yml`, which publishes
   `dantesync-linux-amd64` + `dantesync-windows-amd64.exe` +
   `dantesync-tray-windows-amd64.exe` (+ `.sha256` for each) as GitHub release assets.
   No manual tagging needed — just wait for `release.yml` to go green.
2. **Download + verify once, locally (dev1)**:
   ```bash
   gh release download vX.Y.Z --repo zbynekdrlik/dantesync \
     --pattern "dantesync-linux-amd64*" --pattern "dantesync-windows-amd64.exe*" \
     --pattern "dantesync-tray-windows-amd64.exe*" -D /tmp/dantesync-vX.Y.Z
   cd /tmp/dantesync-vX.Y.Z && sha256sum -c dantesync-linux-amd64.sha256
   sha256sum dantesync-windows-amd64.exe dantesync-tray-windows-amd64.exe   # compare by eye to the .sha256 files
   ```
3. **Canary ONE box first — cam4, not strih/stream.** Deploy (below), then verify
   BOTH: (a) the HTTP status endpoint responds correctly
   (`curl http://<ip>:8898/status`), AND (b) the clock actually still syncs —
   `journalctl -u dantesync -n 20` shows normal `[PTP] PROD`→`[PTP] LOCK` progression
   (a fresh restart takes ~30-40s to re-settle from PROD to LOCK; that's normal, not
   a regression). Only proceed to the rest of the fleet once the canary is LOCKED.
4. **Roll to the remaining Linux fleet** (cam1-3, cam5-6, imag-nb), then the Windows
   boxes (strih, stream) LAST.
5. **Final live proof**: `curl http://10.77.9.202:8898/status` and
   `curl http://10.77.9.204:8898/status` from dev1 (the exact acceptance camera-box's
   own tickets check for) — both must return 200 with `is_locked: true`.

## Deploying to a Linux box (cam1-6, imag-nb) — direct binary swap

**Via MCP (cam1-4, imag-nb) — upload the binary with `scp`/`sshpass` first (NOT the
MCP `FileUpload` tool — base64-encoding a ~2.5MB binary produces a ~3.3MB string that
blows up the calling agent's own context for no benefit; scp is direct and cheap):**

```bash
# DEVICE_ROOT_PW: the cam-fleet root password — NOT committed, see camera-box's own
# ops skill / memory for the value; export it from your password store before this.
sshpass -p "$DEVICE_ROOT_PW" scp dantesync-linux-amd64 root@<ip>:/tmp/dantesync-vX.Y.Z
```

Then, over the box's own MCP `Shell` tool (or plain SSH for cam5/cam6, which have no
MCP server):

```bash
set -e
systemctl stop dantesync
cp /usr/local/bin/dantesync /usr/local/bin/dantesync.bak-vOLD
chmod +x /tmp/dantesync-vX.Y.Z
mv /tmp/dantesync-vX.Y.Z /usr/local/bin/dantesync
systemctl start dantesync
sleep 2
dantesync --version
systemctl is-active dantesync
```

### Gotcha — cam3's root filesystem is READ-ONLY (unlike cam1/2/4/5/6)

`cp`/`mv` into `/usr/local/bin` on cam3 fails with `Read-only file system`. Remount
first, deploy, then remount back:

```bash
mount -o remount,rw /
# ... the swap steps above ...
mount -o remount,ro / 2>/dev/null || true
```

This matches the project's own boot-hardening convention (camera-box's own ops
skill documents the same `mount -o remount,rw /` pattern for its appliance boxes) —
cam3 apparently provisioned with a read-only root at some point; the rest of the
current fleet did not. Don't assume any box's root fs mode — try the plain swap
first, remount-rw only if it fails.

### Gotcha — imag-nb's `newlevel` user needs an interactive sudo password

The `linux-imag-nb` MCP `Shell` tool runs as the unprivileged `newlevel` user with
NO passwordless sudo (`sudo -n true` → `a password is required`). Pipe the password
into every privileged command via `sudo -S`:

```bash
echo "$IMAG_NB_PW" | sudo -S systemctl stop dantesync 2>/dev/null
echo "$IMAG_NB_PW" | sudo -S cp /usr/local/bin/dantesync /usr/local/bin/dantesync.bak-vOLD 2>/dev/null
echo "$IMAG_NB_PW" | sudo -S mv /tmp/dantesync-vX.Y.Z /usr/local/bin/dantesync 2>/dev/null
echo "$IMAG_NB_PW" | sudo -S systemctl start dantesync 2>/dev/null
```

(The `2>/dev/null` suppresses the `[sudo] password for newlevel:` prompt line so it
doesn't clutter output — the command still runs correctly either way.)

## Deploying to a Windows box (strih, stream) — direct binary swap over MCP

Both strih and stream have outbound internet access — have the box download the
release asset directly (matches `dantesync-ownership.md`'s documented release path)
rather than round-tripping the ~2.5MB binary through the MCP `FileUpload` tool's
base64 encoding:

```powershell
$ProgressPreference = 'SilentlyContinue'
Invoke-WebRequest -Uri "https://github.com/zbynekdrlik/dantesync/releases/download/vX.Y.Z/dantesync-windows-amd64.exe" -OutFile "$env:TEMP\dantesync-vX.Y.Z.exe" -UseBasicParsing
Invoke-WebRequest -Uri "https://github.com/zbynekdrlik/dantesync/releases/download/vX.Y.Z/dantesync-tray-windows-amd64.exe" -OutFile "$env:TEMP\dantesync-tray-vX.Y.Z.exe" -UseBasicParsing
Get-FileHash "$env:TEMP\dantesync-vX.Y.Z.exe" -Algorithm SHA256 | Select-Object Hash
Get-FileHash "$env:TEMP\dantesync-tray-vX.Y.Z.exe" -Algorithm SHA256 | Select-Object Hash
# compare both hashes by eye against the release's .sha256 assets before proceeding
```

Then swap the SERVICE binary and the TRAY binary separately — the tray is a running
user-session process with the .exe file locked, so it must be killed before it can
be overwritten:

```powershell
Stop-Service -Name dantesync
Copy-Item "C:\Program Files\DanteSync\dantesync.exe" "C:\Program Files\DanteSync\dantesync.exe.bak-vOLD" -Force
$trayProc = Get-Process -Name "dantesync-tray" -ErrorAction SilentlyContinue
if ($trayProc) { Stop-Process -Id $trayProc.Id -Force; Start-Sleep -Seconds 1 }
Copy-Item "C:\Program Files\DanteSync\dantesync-tray.exe" "C:\Program Files\DanteSync\dantesync-tray.exe.bak-vOLD" -Force
Copy-Item "$env:TEMP\dantesync-vX.Y.Z.exe" "C:\Program Files\DanteSync\dantesync.exe" -Force
Copy-Item "$env:TEMP\dantesync-tray-vX.Y.Z.exe" "C:\Program Files\DanteSync\dantesync-tray.exe" -Force
Start-Service -Name dantesync
Start-Process "C:\Program Files\DanteSync\dantesync-tray.exe"
Start-Sleep -Seconds 2
& "C:\Program Files\DanteSync\dantesync.exe" --version
Get-Service -Name dantesync | Format-Table -AutoSize
Get-Process -Name "dantesync-tray" | Format-Table Id,ProcessName -AutoSize
```

### Gotcha — `Copy-Item` on the running `dantesync.exe` itself succeeds (Windows
allows overwriting a running exe's file on disk; the old image stays mapped in
memory until the process restarts), but `dantesync-tray.exe` fails with "the process
cannot access the file" if you skip the kill step above — Windows locks a GUI/tray
process's own exe file more strictly than a service binary's.

### New inbound port checklist (e.g. the #47 HTTP status endpoint on :8898)

Check for an existing firewall rule before assuming one is needed:

```powershell
Get-NetFirewallRule -DisplayName "*dantesync*" | Format-Table DisplayName,Enabled,Direction,Action -AutoSize
Get-NetFirewallPortFilter | Where-Object {$_.LocalPort -eq 8898} | Format-Table -AutoSize
```

On strih/stream (2026-07-10) neither box had ANY firewall rule for dantesync/8898,
and the port was reachable from dev1 immediately after the service restart anyway —
don't assume a firewall block exists; verify with a live `curl`/`Invoke-WebRequest`
from the actual REMOTE caller (not `localhost` on the box itself, which proves
nothing about LAN reachability) before troubleshooting a firewall that may not be
the problem.

## Fresh install (a box that has NEVER run DanteSync)

Use the install script — it lays down the systemd unit / Windows service /
scheduled task / registry autostart from scratch, none of which the direct-swap
procedure above touches:

```powershell
irm https://raw.githubusercontent.com/zbynekdrlik/dantesync/master/install.ps1 | iex
```

```bash
curl -sL https://raw.githubusercontent.com/zbynekdrlik/dantesync/master/install.sh | sudo bash
```

## Checking status (any box)

```bash
# Linux
dantesync --version
systemctl is-active dantesync
journalctl -u dantesync -n 20 --no-pager   # look for [PTP] LOCK / [NTP] offset:...us

# Windows (PowerShell)
& "C:\Program Files\DanteSync\dantesync.exe" --version
Get-Service -Name dantesync
Get-Process -Name dantesync-tray -ErrorAction SilentlyContinue

# Any box with #47's HTTP endpoint (default enabled, port 8898) — reachable from ANY
# other machine on the LAN, not just the box itself:
curl http://<ip>:8898/status
```
