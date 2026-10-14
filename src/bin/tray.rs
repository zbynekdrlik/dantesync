#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! `dantesync-tray`: the clock-state icon in the system tray (Windows and Linux).
//!
//! Both platforms draw the shared view of `dantesync::tray_view` (icon colour, pulse
//! ring, menu text); only the shell differs. Built with the `tray` feature:
//! `cargo build --release --features tray --bin dantesync-tray`.

#[cfg(not(any(windows, target_os = "linux")))]
fn main() {
    println!("dantesync-tray runs on Windows and Linux only.");
}

/// The Linux tray (dantesync#132): a gtk main loop hosts the `tray-icon` indicator
/// (libayatana-appindicator, which falls back to an XEmbed status icon when no
/// StatusNotifier host runs, e.g. tint2). A poller thread GETs the daemon's
/// `127.0.0.1:8898/status` once a second; a 250 ms glib timeout applies the newest
/// reading and handles the menu. No notifications: the Linux daemon `notify-send`s
/// the NO DANTE CLOCK alarm itself (dantesync#114).
#[cfg(target_os = "linux")]
mod linux_app {
    use dantesync::tray_view::{self, IconSpec, TrayState, TrayStatus};
    use gtk::glib;
    use std::fs::{File, OpenOptions};
    use std::net::SocketAddr;
    use std::os::unix::io::AsRawFd;
    use std::sync::mpsc;
    use std::time::Duration;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    /// How often the poller reads the status (and the bound of each read).
    const POLL_INTERVAL: Duration = Duration::from_secs(1);
    /// How often the gtk thread applies the newest reading and the menu events.
    const UI_TICK: Duration = Duration::from_millis(250);

    /// Take the per-user single-instance lock: an exclusive non-blocking `flock` on
    /// the lock file, held for the process lifetime by the returned `File`. `None` (the
    /// reason printed) when another tray already holds it or the file cannot be opened.
    fn single_instance_lock() -> Option<File> {
        let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
        let path = tray_view::lock_file_path(xdg.as_deref());
        let file = match OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!(
                    "dantesync-tray: cannot open lock file {}: {}",
                    path.display(),
                    e
                );
                return None;
            }
        };
        // SAFETY: flock on a file descriptor this process owns and keeps open.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            Some(file)
        } else {
            eprintln!("dantesync-tray: another instance is running, exiting");
            None
        }
    }

    fn icon_from_spec(spec: &IconSpec) -> Icon {
        Icon::from_rgba(
            tray_view::render_icon_rgba(spec),
            tray_view::ICON_SIZE,
            tray_view::ICON_SIZE,
        )
        .expect("a 32x32 RGBA buffer is a valid icon")
    }

    /// The tray and its read-only menu lines, owned by the gtk thread.
    struct Ui {
        tray: TrayIcon,
        status_i: MenuItem,
        mode_i: MenuItem,
        gm_i: MenuItem,
        shown: IconSpec,
        state: Option<TrayState>,
        version: &'static str,
    }

    impl Ui {
        /// Show one reading (`None` = the daemon did not answer: offline).
        fn apply(&mut self, status: Option<&TrayStatus>) {
            let spec = tray_view::icon_spec(status, false);
            if !spec.same_look(&self.shown) {
                if let Err(e) = self.tray.set_icon(Some(icon_from_spec(&spec))) {
                    eprintln!("dantesync-tray: set_icon failed: {}", e);
                }
                self.shown = spec;
            }
            let texts = match status {
                Some(s) => tray_view::status_texts(s, self.version),
                None => tray_view::offline_texts(self.version),
            };
            let state = tray_view::tray_state(status);
            if self.state != Some(state) {
                eprintln!(
                    "dantesync-tray: {} -> {} ({})",
                    self.state.map(TrayState::label).unwrap_or("START"),
                    state.label(),
                    texts.status_line
                );
                self.state = Some(state);
            }
            let _ = self.tray.set_tooltip(Some(texts.tooltip));
            self.status_i.set_text(texts.status_line);
            self.mode_i.set_text(texts.mode_line);
            self.gm_i.set_text(texts.gm_line);
        }
    }

    pub fn main() {
        let _lock = match single_instance_lock() {
            Some(lock) => lock,
            None => return,
        };

        if let Err(e) = gtk::init() {
            eprintln!(
                "dantesync-tray: gtk init failed (no X session / DISPLAY?): {}",
                e
            );
            std::process::exit(1);
        }

        let version = env!("CARGO_PKG_VERSION");
        let status_i = MenuItem::new("Status: Connecting...", false, None);
        let mode_i = MenuItem::new("Mode: --", false, None);
        let gm_i = MenuItem::new("PTP Master: --", false, None);
        let quit_i = MenuItem::new("Quit", true, None);

        let menu = Menu::new();
        let separator = PredefinedMenuItem::separator();
        if let Err(e) = menu.append_items(&[&status_i, &mode_i, &gm_i, &separator, &quit_i]) {
            eprintln!("dantesync-tray: building the menu failed: {}", e);
            std::process::exit(1);
        }

        let connecting = IconSpec::connecting();
        let tray = match TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(format!("DanteSync v{} - Connecting...", version))
            .with_icon(icon_from_spec(&connecting))
            .build()
        {
            Ok(t) => t,
            Err(e) => {
                eprintln!("dantesync-tray: creating the tray icon failed: {}", e);
                std::process::exit(1);
            }
        };

        let endpoint: SocketAddr = tray_view::STATUS_ENDPOINT
            .parse()
            .expect("STATUS_ENDPOINT is a socket address");
        let (tx, rx) = mpsc::channel::<Option<TrayStatus>>();
        std::thread::spawn(move || loop {
            let reading = tray_view::fetch_status(endpoint, POLL_INTERVAL);
            if tx.send(reading).is_err() {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        });

        let mut ui = Ui {
            tray,
            status_i,
            mode_i,
            gm_i,
            shown: connecting,
            state: None,
            version,
        };
        let quit_id = quit_i.id().clone();
        let menu_events = MenuEvent::receiver();

        glib::timeout_add_local(UI_TICK, move || {
            // Only the newest reading matters.
            let mut newest = None;
            while let Ok(reading) = rx.try_recv() {
                newest = Some(reading);
            }
            if let Some(reading) = newest {
                ui.apply(reading.as_ref());
            }
            while let Ok(event) = menu_events.try_recv() {
                if event.id == quit_id {
                    eprintln!("dantesync-tray: quit");
                    gtk::main_quit();
                    return glib::ControlFlow::Break;
                }
            }
            glib::ControlFlow::Continue
        });

        gtk::main();
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux_app::main();
}

#[cfg(windows)]
mod app {
    use serde::Deserialize;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    // dantesync#114: the alarm title is the lib's, so the tray balloon and the daemon
    // log/notify-send stay byte-for-byte the same text.
    use dantesync::clock_alarm::CLOCK_ALARM_TITLE;
    // dantesync#132: the status view, icon, texts and alarm cadence are shared with
    // the Linux tray.
    use dantesync::tray_view::{self, AlarmAction, ClockAlarmCadence, IconSpec, TrayStatus};
    use tokio::io::AsyncReadExt;
    use tokio::net::windows::named_pipe::ClientOptions;
    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem},
        Icon, TrayIconBuilder,
    };
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
    use windows::Win32::System::Threading::CreateMutexW;
    use winit::event::Event;
    use winit::event_loop::{ControlFlow, EventLoopBuilder};
    use winrt_notification::{Sound, Toast};

    // ========================================================================
    // SINGLE INSTANCE CHECK - Prevent multiple tray apps
    // ========================================================================

    struct SingleInstanceGuard {
        _handle: HANDLE,
    }

    impl SingleInstanceGuard {
        /// Try to acquire single-instance lock. Returns None if another instance is running.
        fn try_acquire() -> Option<Self> {
            unsafe {
                let mutex_name: Vec<u16> = "Global\\DanteSyncTrayMutex\0".encode_utf16().collect();
                let handle = CreateMutexW(None, false, PCWSTR(mutex_name.as_ptr()));

                match handle {
                    Ok(h) => {
                        // Check if mutex already existed
                        if let Err(e) = GetLastError() {
                            if e.code() == ERROR_ALREADY_EXISTS.to_hresult() {
                                // Another instance is running - close handle and return None
                                let _ = CloseHandle(h);
                                return None;
                            }
                        }
                        Some(SingleInstanceGuard { _handle: h })
                    }
                    Err(_) => None,
                }
            }
        }
    }

    impl Drop for SingleInstanceGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self._handle);
            }
        }
    }

    // ========================================================================
    // GITHUB RELEASE - For version check
    // ========================================================================

    #[derive(Deserialize, Debug)]
    struct GitHubRelease {
        tag_name: String,
    }

    /// Parse version string (e.g., "v1.6.4" or "1.6.4") into comparable tuple
    fn parse_version(version: &str) -> Option<(u32, u32, u32)> {
        let v = version.trim_start_matches('v');
        let parts: Vec<&str> = v.split('.').collect();
        if parts.len() >= 3 {
            Some((
                parts[0].parse().ok()?,
                parts[1].parse().ok()?,
                parts[2].parse().ok()?,
            ))
        } else {
            None
        }
    }

    /// Compare versions, returns true if remote is newer than local
    fn is_newer_version(local: &str, remote: &str) -> bool {
        match (parse_version(local), parse_version(remote)) {
            (Some(l), Some(r)) => r > l,
            _ => false,
        }
    }

    #[derive(Debug)]
    enum AppEvent {
        Update(TrayStatus),
        Offline,
        NewVersionAvailable(String),
    }

    // ========================================================================
    // TOAST NOTIFICATIONS
    // ========================================================================

    /// Show a Windows toast notification
    fn show_notification(title: &str, message: &str) {
        let _ = Toast::new(Toast::POWERSHELL_APP_ID)
            .title(title)
            .text1(message)
            .sound(Some(Sound::Default))
            .show();
    }

    // ========================================================================
    // VERSION CHECK - GitHub API
    // ========================================================================

    const GITHUB_API_URL: &str =
        "https://api.github.com/repos/zbynekdrlik/dantesync/releases/latest";

    /// Fetch the latest version from GitHub releases
    async fn check_latest_version() -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let client = reqwest::Client::builder()
            .user_agent("DanteSync-Tray")
            .timeout(Duration::from_secs(10))
            .build()?;

        let response = client.get(GITHUB_API_URL).send().await?;
        let release: GitHubRelease = response.json().await?;
        Ok(release.tag_name)
    }

    /// Track previous state for detecting transitions
    #[derive(Default)]
    struct NotificationState {
        was_locked: bool,
        was_nano: bool,
        was_online: bool,
        was_ptp_offline: bool,
        was_ntp_failed: bool,
        first_update: bool,
        // dantesync#114: per-cadence clock-alarm balloon tracking.
        clock_alarm: ClockAlarmCadence,
    }

    /// The tray icon for a spec from the shared view (dantesync#132).
    fn icon_from_spec(spec: &IconSpec) -> Icon {
        Icon::from_rgba(
            tray_view::render_icon_rgba(spec),
            tray_view::ICON_SIZE,
            tray_view::ICON_SIZE,
        )
        .unwrap()
    }

    // ========================================================================
    // MAIN APPLICATION
    // ========================================================================

    pub fn main() {
        // Single-instance check - exit silently if another instance is running
        let _guard = match SingleInstanceGuard::try_acquire() {
            Some(guard) => guard,
            None => {
                // Another instance is already running - exit silently
                return;
            }
        };

        let event_loop = EventLoopBuilder::<AppEvent>::with_user_event()
            .build()
            .unwrap();
        let proxy = event_loop.create_proxy();

        // ====================================================================
        // MENU ITEMS
        // ====================================================================

        let status_i = MenuItem::new("Status: Connecting...", false, None);
        let mode_i = MenuItem::new("Mode: --", false, None);
        let gm_ip_i = MenuItem::new("PTP Master: --", false, None);

        // Service control
        let restart_i = MenuItem::new("Restart Service", true, None);
        let start_stop_i = MenuItem::new("Stop Service", true, None);

        // Utilities
        let log_i = MenuItem::new("Open Log File", true, None);
        let live_log_i = MenuItem::new("View Live Log", true, None);
        let config_i = MenuItem::new("Edit Configuration", true, None);

        // Upgrade - disabled until new version detected
        let upgrade_i = MenuItem::new("Check for Updates...", true, None);

        let quit_i = MenuItem::new("Quit", true, None);

        let menu = Menu::new();
        menu.append(&status_i).unwrap();
        menu.append(&mode_i).unwrap();
        menu.append(&gm_ip_i).unwrap();
        menu.append(&tray_icon::menu::PredefinedMenuItem::separator())
            .unwrap();
        menu.append(&restart_i).unwrap();
        menu.append(&start_stop_i).unwrap();
        menu.append(&tray_icon::menu::PredefinedMenuItem::separator())
            .unwrap();
        menu.append(&log_i).unwrap();
        menu.append(&live_log_i).unwrap();
        menu.append(&config_i).unwrap();
        menu.append(&tray_icon::menu::PredefinedMenuItem::separator())
            .unwrap();
        menu.append(&upgrade_i).unwrap();
        menu.append(&tray_icon::menu::PredefinedMenuItem::separator())
            .unwrap();
        menu.append(&quit_i).unwrap();

        // Colors (Flat UI / Bootstrap-style)
        // Yellow (acquiring) until the first status arrives.
        let yellow_icon = icon_from_spec(&IconSpec::connecting());

        // Wrap in RefCell so we can explicitly drop it on exit to clean up tray icon
        let tray_icon = RefCell::new(Some(
            TrayIconBuilder::new()
                .with_menu(Box::new(menu.clone()))
                .with_tooltip("DanteSync - Connecting...")
                .with_icon(yellow_icon.clone())
                .build()
                .unwrap(),
        ));

        // Spawn status poller thread
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async move {
                loop {
                    // Server pipe is outbound-only (Server -> Client).
                    // We must open as read-only, otherwise we get ACCESS_DENIED.
                    match ClientOptions::new()
                        .write(false)
                        .read(true)
                        .open(r"\\.\pipe\dantesync")
                    {
                        Ok(mut client) => {
                            loop {
                                let mut len_buf = [0u8; 4];
                                if client.read_exact(&mut len_buf).await.is_err() {
                                    break;
                                }
                                let len = u32::from_le_bytes(len_buf) as usize;
                                let mut buf = vec![0u8; len];
                                if client.read_exact(&mut buf).await.is_err() {
                                    break;
                                }

                                match serde_json::from_slice::<TrayStatus>(&buf) {
                                    Ok(status) => {
                                        let _ = proxy.send_event(AppEvent::Update(status));
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "Failed to parse IPC status: {} (len={})",
                                            e,
                                            buf.len()
                                        );
                                    }
                                }
                            }
                            // Connection closed by server (one-shot). Sleep before reconnecting to prevent UI freeze.
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                        Err(_) => {
                            let _ = proxy.send_event(AppEvent::Offline);
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            });
        });

        // ====================================================================
        // VERSION CHECK - Periodic check for updates
        // ====================================================================
        let version_proxy = event_loop.create_proxy();
        let current_version = env!("CARGO_PKG_VERSION").to_string();
        let update_available = Arc::new(AtomicBool::new(false));
        let update_available_clone = update_available.clone();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async move {
                // Initial delay before first check (10 seconds after startup)
                tokio::time::sleep(Duration::from_secs(10)).await;

                loop {
                    // Check for new version
                    match check_latest_version().await {
                        Ok(latest) => {
                            if is_newer_version(&current_version, &latest) {
                                // Only notify if we haven't already
                                if !update_available_clone.load(Ordering::Relaxed) {
                                    update_available_clone.store(true, Ordering::Relaxed);
                                    let _ = version_proxy
                                        .send_event(AppEvent::NewVersionAvailable(latest));
                                }
                            }
                        }
                        Err(_) => {
                            // Silently ignore version check failures
                        }
                    }

                    // Check every 6 hours
                    tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
                }
            });
        });

        let menu_channel = MenuEvent::receiver();
        let version = env!("CARGO_PKG_VERSION");

        // Track state for notifications
        let notification_state = RefCell::new(NotificationState {
            was_locked: false,
            was_nano: false,
            was_online: false,
            was_ptp_offline: false,
            was_ntp_failed: false,
            first_update: true,
            clock_alarm: ClockAlarmCadence::new(),
        });

        event_loop.run(move |event, elwt| {
            elwt.set_control_flow(ControlFlow::Wait);

            match event {
                Event::UserEvent(app_event) => {
                    match app_event {
                        AppEvent::Update(status) => {
                            // ================================================
                            // NOTIFICATIONS - Detect state transitions
                            // ================================================
                            {
                                let mut state = notification_state.borrow_mut();

                                let is_nano = status.mode == "NANO";
                                let is_ptp_offline = status.mode == "NTP-only";

                                // Check for state changes (skip first update)
                                if !state.first_update {
                                    // NTP failure transitions (critical)
                                    if status.ntp_failed && !state.was_ntp_failed {
                                        // #68: ntp_failed now covers BOTH an
                                        // unreachable upstream AND no fresh
                                        // measurement inside the staleness
                                        // window (a node that stopped querying
                                        // at all). "Server unreachable" was
                                        // simply false in the second case.
                                        show_notification(
                                            "DanteSync",
                                            "UTC alignment stale - NTP not tracking"
                                        );
                                    } else if !status.ntp_failed && state.was_ntp_failed {
                                        show_notification(
                                            "DanteSync",
                                            "UTC alignment restored"
                                        );
                                    }
                                    // PTP offline transitions (highest priority)
                                    else if is_ptp_offline && !state.was_ptp_offline {
                                        show_notification(
                                            "DanteSync",
                                            "PTP offline - running NTP-only sync"
                                        );
                                    } else if !is_ptp_offline && state.was_ptp_offline {
                                        show_notification(
                                            "DanteSync",
                                            "PTP restored - resuming PTP sync"
                                        );
                                    }
                                    // NANO mode and lock state transitions: icon color change is sufficient,
                                    // no toast notifications needed

                                    // Service came online
                                    if !state.was_online {
                                        show_notification(
                                            "DanteSync",
                                            "Service connected"
                                        );
                                    }
                                }

                                state.was_locked = status.is_locked;
                                state.was_nano = is_nano;
                                state.was_ptp_offline = is_ptp_offline;
                                state.was_ntp_failed = status.ntp_failed;
                                state.was_online = true;
                                state.first_update = false;
                            }

                            // ================================================
                            // #114: loud, repeating NO-DANTE-CLOCK balloon.
                            // Windows can't toast from the session-0 service, so
                            // the tray raises the per-cadence balloon here from
                            // the /status.clock_alarm field.
                            // ================================================
                            {
                                let mut state = notification_state.borrow_mut();
                                match state.clock_alarm.decide(
                                    &status.clock_alarm,
                                    status.clock_alarm_interval_s,
                                    Instant::now(),
                                ) {
                                    AlarmAction::Raise(message) => {
                                        show_notification(CLOCK_ALARM_TITLE, &message)
                                    }
                                    AlarmAction::Cleared => {
                                        // Alarm just cleared — one reassuring toast.
                                        show_notification(
                                            "DanteSync",
                                            tray_view::CLOCK_RESTORED_MESSAGE,
                                        )
                                    }
                                    AlarmAction::Quiet => {}
                                }
                            }

                            // ================================================
                            // ICON + TEXT - the shared view (dantesync#132)
                            // ================================================
                            let has_update = update_available.load(Ordering::Relaxed);
                            let icon = icon_from_spec(&tray_view::icon_spec(Some(&status), has_update));
                            let texts = tray_view::status_texts(&status, version);
                            let tooltip = texts.tooltip;
                            let status_text = texts.status_line;
                            let mode_text = texts.mode_line;
                            let gm_ip_text = texts.gm_line;

                            if let Some(ref ti) = *tray_icon.borrow() {
                                let _ = ti.set_icon(Some(icon));
                                let _ = ti.set_tooltip(Some(tooltip));
                            }
                            status_i.set_text(status_text);
                            mode_i.set_text(mode_text);
                            gm_ip_i.set_text(gm_ip_text);
                            // Service is running - show Stop option
                            start_stop_i.set_text("Stop Service".to_string());
                            restart_i.set_enabled(true);
                        }
                        AppEvent::Offline => {
                            // Check if we were online before
                            {
                                let mut state = notification_state.borrow_mut();
                                if state.was_online {
                                    show_notification(
                                        "DanteSync",
                                        "Service offline"
                                    );
                                }
                                state.was_online = false;
                                state.was_locked = false;
                            }

                            let has_update = update_available.load(Ordering::Relaxed);
                            let offline_icon = icon_from_spec(&tray_view::icon_spec(None, has_update));
                            let texts = tray_view::offline_texts(version);

                            if let Some(ref ti) = *tray_icon.borrow() {
                                let _ = ti.set_icon(Some(offline_icon));
                                let _ = ti.set_tooltip(Some(texts.tooltip));
                            }
                            status_i.set_text(texts.status_line);
                            mode_i.set_text(texts.mode_line);
                            gm_ip_i.set_text(texts.gm_line);
                            // Service is stopped - show Start option
                            start_stop_i.set_text("Start Service".to_string());
                            restart_i.set_enabled(false);
                        }
                        AppEvent::NewVersionAvailable(new_version) => {
                            // Update menu item text to show available version
                            upgrade_i.set_text(format!("Upgrade to {}", new_version));

                            // Show notification
                            show_notification(
                                "DanteSync",
                                &format!("New version {} available", new_version)
                            );
                        }
                    }
                }
                _ => {
                    if let Ok(event) = menu_channel.try_recv() {
                        if event.id == quit_i.id() {
                            // Explicitly drop tray icon to clean up system tray
                            tray_icon.borrow_mut().take();
                            elwt.exit();
                        } else if event.id == restart_i.id() {
                            // Restart service using PowerShell (requires elevation)
                            let _ = std::process::Command::new("powershell.exe")
                                .args(["-Command", "Start-Process powershell -Verb RunAs -ArgumentList '-Command','Restart-Service dantesync -Force'"])
                                .spawn();
                        } else if event.id == start_stop_i.id() {
                            // Start or Stop service based on current state
                            let is_online = notification_state.borrow().was_online;
                            if is_online {
                                // Service running - stop it
                                let _ = std::process::Command::new("powershell.exe")
                                    .args(["-Command", "Start-Process powershell -Verb RunAs -ArgumentList '-Command','Stop-Service dantesync -Force'"])
                                    .spawn();
                            } else {
                                // Service stopped - start it
                                let _ = std::process::Command::new("powershell.exe")
                                    .args(["-Command", "Start-Process powershell -Verb RunAs -ArgumentList '-Command','Start-Service dantesync'"])
                                    .spawn();
                            }
                        } else if event.id == log_i.id() {
                            let _ = std::process::Command::new("notepad.exe")
                                .arg(r"C:\ProgramData\DanteSync\dantesync.log")
                                .spawn();
                        } else if event.id == live_log_i.id() {
                            let _ = std::process::Command::new("powershell.exe")
                                .args(["-NoExit", "-Command", "Get-Content 'C:\\ProgramData\\DanteSync\\dantesync.log' -Tail 20 -Wait"])
                                .spawn();
                        } else if event.id == config_i.id() {
                            let _ = std::process::Command::new("notepad.exe")
                                .arg(r"C:\ProgramData\DanteSync\config.json")
                                .spawn();
                        } else if event.id == upgrade_i.id() {
                            // Run upgrade via PowerShell IRM (Invoke-RestMethod)
                            // This downloads and executes the install script from GitHub
                            let _ = std::process::Command::new("powershell.exe")
                                .args([
                                    "-Command",
                                    "Start-Process powershell -Verb RunAs -ArgumentList '-NoExit','-Command','irm https://raw.githubusercontent.com/zbynekdrlik/dantesync/master/install.ps1 | iex'"
                                ])
                                .spawn();
                        }
                    }
                }
            }
        }).unwrap();
    }
}

#[cfg(windows)]
fn main() {
    app::main();
}
