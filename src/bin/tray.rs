#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    println!("This utility is for Windows only.");
}

#[cfg(windows)]
mod app {
    use tray_icon::{TrayIconBuilder, menu::{Menu, MenuItem, MenuEvent}, Icon};
    use winit::event_loop::{EventLoop, ControlFlow, EventLoopBuilder, EventLoopProxy};
    use winit::event::Event;
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::io::AsyncReadExt;
    use serde::Deserialize;
    use std::time::Duration;

    #[derive(Deserialize, Debug, Clone)]
    struct SyncStatus {
        pub offset_ns: i64,
        pub drift_ppm: f64,
        pub gm_uuid: Option<[u8; 6]>,
        pub settled: bool,
        pub updated_ts: u64,
    }

    #[derive(Debug)]
    enum AppEvent {
        Update(SyncStatus),
        Offline,
    }

    fn generate_icon(r: u8, g: u8, b: u8) -> Icon {
        let width = 32;
        let height = 32;
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..height {
            for _ in 0..width {
                rgba.push(r);
                rgba.push(g);
                rgba.push(b);
                rgba.push(255);
            }
        }
        Icon::from_rgba(rgba, width, height).unwrap()
    }

    pub fn main() {
        let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build().unwrap();
        let proxy = event_loop.create_proxy();
        
        let quit_i = MenuItem::new("Quit", true, None);
        let status_i = MenuItem::new("Status: Connecting...", false, None);
        let menu = Menu::new();
        menu.append(&status_i).unwrap();
        menu.append_items(&[&tray_icon::menu::PredefinedMenuItem::separator(), &quit_i]).unwrap();

        let red_icon = generate_icon(255, 0, 0);
        let green_icon = generate_icon(0, 255, 0);
        let yellow_icon = generate_icon(255, 255, 0);

        let tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_tooltip("Dante Time Sync - Connecting...")
            .with_icon(red_icon.clone())
            .build()
            .unwrap();

        // Spawn poller thread
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            
            rt.block_on(async move {
                loop {
                    match ClientOptions::new().open(r"\\.\pipe\dantetimesync") {
                        Ok(mut client) => {
                            loop {
                                let mut len_buf = [0u8; 4];
                                if client.read_exact(&mut len_buf).await.is_err() { break; }
                                let len = u32::from_le_bytes(len_buf) as usize;
                                let mut buf = vec![0u8; len];
                                if client.read_exact(&mut buf).await.is_err() { break; }
                                
                                if let Ok(status) = serde_json::from_slice::<SyncStatus>(&buf) {
                                    let _ = proxy.send_event(AppEvent::Update(status));
                                }
                            }
                        }
                        Err(_) => {
                            let _ = proxy.send_event(AppEvent::Offline);
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            });
        });

        let menu_channel = MenuEvent::receiver();

        event_loop.run(move |event, elwt| {
            elwt.set_control_flow(ControlFlow::Wait);

            match event {
                Event::UserEvent(app_event) => {
                    match app_event {
                        AppEvent::Update(status) => {
                            let icon = if !status.settled {
                                yellow_icon.clone()
                            } else if status.offset_ns.abs() < 1_000_000 { 
                                green_icon.clone()
                            } else {
                                red_icon.clone()
                            };
                            
                            let text = format!("Offset: {} µs\nDrift: {:.3} ppm", status.offset_ns / 1000, status.drift_ppm);
                            let short_text = format!("Offset: {} µs", status.offset_ns / 1000);
                            
                            let _ = tray_icon.set_icon(Some(icon));
                            let _ = tray_icon.set_tooltip(Some(format!("Dante Time Sync\n{}", text)));
                            status_i.set_text(short_text);
                        }
                        AppEvent::Offline => {
                            let _ = tray_icon.set_icon(Some(red_icon.clone()));
                            let _ = tray_icon.set_tooltip(Some("Dante Time Sync - Service Offline".to_string()));
                            status_i.set_text("Service Offline");
                        }
                    }
                }
                _ => {
                    if let Ok(event) = menu_channel.try_recv() {
                        if event.id == quit_i.id() {
                            elwt.exit();
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