---
name: dantesync-deployment
description: Deploy/upgrade DanteSync to Windows and Linux targets. Use when upgrading, installing, or fixing tray icons on remote machines.
---

# DanteSync Deployment Skill

## CRITICAL: Always Use the Standard Install Script

**NEVER manually copy files and restart services.** Always use the official install script:

```powershell
irm https://raw.githubusercontent.com/zbynekdrlik/dantesync/master/install.ps1 | iex
```

## Why the Install Script is Required

The install script:

1. Detects the **actual logged-in user** via `(Get-WmiObject -Class Win32_ComputerSystem).UserName`
2. Creates a **proper scheduled task** with `New-ScheduledTaskPrincipal -LogonType Interactive`
3. Starts the tray in the **correct user session** (not session 0)
4. Sets up **registry autostart** in HKLM for all users
5. Registers in **Add/Remove Programs**

## Deploying to All Targets

### Windows Targets (via SSH)

```bash
# Run install script on Windows target
sshpass -p 'PASSWORD' ssh USER@HOST 'powershell -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/zbynekdrlik/dantesync/master/install.ps1 | iex"'
```

### Deploy to All Windows Targets at Once

```bash
for target in "strih.lan:newlevel@10.77.9.202:newlevel" \
              "iem:iem@10.77.9.231:iem" \
              "stream.lan:newlevel@10.77.9.204:newlevel" \
              "ableton-foh:ableton-foh@10.77.9.230:newlevel" \
              "mbc.lan:newlevel@10.77.9.232:newlevel"; do
  name="${target%%:*}"
  rest="${target#*:}"
  user_host="${rest%%:*}"
  pass="${rest##*:}"

  echo "=== $name ==="
  sshpass -p "$pass" ssh -o ConnectTimeout=15 "$user_host" \
    'powershell -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/zbynekdrlik/dantesync/master/install.ps1 | iex"' &
done
wait
```

### Linux Target (develbox)

```bash
# Download and install
sshpass -p 'newlevel' ssh newlevel@10.77.9.21 '
  sudo systemctl stop dantesync
  curl -sL https://github.com/zbynekdrlik/dantesync/releases/latest/download/dantesync-linux-amd64 -o /tmp/dantesync
  sudo cp /tmp/dantesync /usr/local/bin/dantesync
  sudo chmod +x /usr/local/bin/dantesync
  sudo systemctl start dantesync
'
```

## Starting Tray App Remotely (if needed)

If tray needs to be started without full reinstall, use scheduled task with `/IT` flag:

```bash
# Get logged-in username first
sshpass -p 'PASS' ssh USER@HOST 'query user'

# Create and run task for THAT user (not SSH user!)
sshpass -p 'PASS' ssh USER@HOST '
  schtasks /create /tn TrayStart /tr "C:\Program Files\DanteSync\dantesync-tray.exe" /sc once /st 00:00 /ru LOGGED_IN_USER /IT /f
  schtasks /run /tn TrayStart
'
```

**IMPORTANT:** The `/ru` username must be the **console-logged-in user**, not the SSH user!

## Checking Status

### Verify Tray Running

```bash
sshpass -p 'PASS' ssh USER@HOST 'tasklist | findstr -i tray'
```

### Verify Service Running

```bash
sshpass -p 'PASS' ssh USER@HOST 'sc query dantesync'
```

### Check Logged-in User

```bash
sshpass -p 'PASS' ssh USER@HOST 'query user'
```

### Check Version

```bash
sshpass -p 'PASS' ssh USER@HOST 'powershell -Command "Get-Content \"C:\ProgramData\DanteSync\dantesync.log\" -Head 5"'
```

## Target Reference

| Host        | IP          | SSH User    | Password | Notes                 |
| ----------- | ----------- | ----------- | -------- | --------------------- |
| strih.lan   | 10.77.9.202 | newlevel    | newlevel | NTP Master            |
| iem         | 10.77.9.231 | iem         | iem      | Logged-in: Ableton-PC |
| stream.lan  | 10.77.9.204 | newlevel    | newlevel |                       |
| ableton-foh | 10.77.9.230 | ableton-foh | newlevel |                       |
| mbc.lan     | 10.77.9.232 | newlevel    | newlevel |                       |
| develbox    | 10.77.9.21  | newlevel    | newlevel | Linux                 |

## Common Mistakes to Avoid

1. **Never manually copy exe files** - use install script
2. **Never assume SSH user = logged-in user** - check with `query user`
3. **Never use `schtasks /run` without `/IT` flag** - tray won't show
4. **Never start tray from SSH directly** - it runs in session 0, not user's desktop
5. **Always wait for install to complete** - the script takes ~30-60 seconds
