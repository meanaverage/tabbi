mod clipboard;
mod config;
mod debug;
mod dock_menu;
mod fs;
mod files;
mod host;
mod hotkey;
mod keychain;
mod menu;
mod net;
mod pty;
mod process;
mod rdp;
use tauri::Manager;
mod ssh;
mod sync;
mod system;
mod traffic_lights;
mod window;
mod windows;

pub fn run() {
    let background = debug::background();
    let builder = tauri::Builder::default();
    // A test instance doesn't take over the screen at launch (see debug::background).
    #[cfg(target_os = "macos")]
    let builder = builder.activate_ignoring_other_apps(!background);
    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(hotkey::plugin())
        .menu(|app| menu::build(app))
        .on_menu_event(|app, event| menu::on_event(app, event.id().as_ref()))
        .manage(pty::Ptys::default())
        .manage(clipboard::Clipboard::default())
        .manage(ssh::Ssh::default())
        .manage(net::Net::default())
        .manage(files::Files::default())
        .manage(process::Processes::default())
        .manage(rdp::Proxies::default())
        .manage(rdp::Drives::default())
        .manage(traffic_lights::TrafficLights::default())
        .register_uri_scheme_protocol("tabbysync", sync::handle)
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            if background {
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            }
            // The window's size and position are kept between launches (a test instance never restores "visible":
            // the plugin would show the window and focus it).
            let flags = if background {
                tauri_plugin_window_state::StateFlags::all() - tauri_plugin_window_state::StateFlags::VISIBLE
            } else {
                tauri_plugin_window_state::StateFlags::all()
            };
            app.handle().plugin(
                tauri_plugin_window_state::Builder::new()
                    .with_state_flags(flags)
                    .with_filename(config::window_state_path(app.handle())?.to_string_lossy())
                    .build(),
            )?;
            // The window is made here rather than from the config, so that a test instance can stay behind.
            windows::build(app.handle(), "main")?;
            if let Some(id) = debug::web_store() {
                eprintln!("debug: web storage {}", uuid::Uuid::from_bytes(id).hyphenated().to_string().to_uppercase());
            }
            dock_menu::install(app.handle())?;
            debug::start(app.handle());
            Ok(())
        })
        .on_page_load(|webview, payload| {
            if matches!(payload.event(), tauri::webview::PageLoadEvent::Started) {
                webview.state::<process::Processes>().reset(webview.label());
                webview.state::<files::Files>().reset(webview.label());
                webview.state::<rdp::Proxies>().reset(webview.label());
                webview.state::<rdp::Drives>().reset(webview.label());
            }
        })
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                window.state::<process::Processes>().reset(window.label());
                window.state::<files::Files>().reset(window.label());
                window.state::<rdp::Proxies>().reset(window.label());
                window.state::<rdp::Drives>().reset(window.label());
            }
            traffic_lights::on_window_event(window, event);
            dock_menu::window_event(window.label(), event);
            debug::on_window_event(window, event);
        })
        .invoke_handler(tauri::generate_handler![
            rdp::rdp_proxy_start,
            rdp::rdp_proxy_answer,
            rdp::rdp_proxy_stats,
            rdp::rdp_proxy_close,
            rdp::rdp_drives_open,
            rdp::rdp_drives_close,
            rdp::rdp_drive_stat,
            rdp::rdp_drive_list,
            rdp::rdp_drive_open,
            rdp::rdp_drive_read,
            rdp::rdp_drive_write,
            rdp::rdp_drive_truncate,
            rdp::rdp_drive_close,
            rdp::rdp_drive_mkdir,
            rdp::rdp_drive_remove,
            rdp::rdp_drive_rename,
            rdp::rdp_drive_volume,
            process::proc_spawn,
            process::proc_write,
            process::proc_end_stdin,
            process::proc_kill,
            process::proc_release,
            files::files_call,
            host::host_info,
            host::open_external,
            system::list_fonts,
            system::app_quit,
            window::window_set_vibrancy,
            window::window_set_opacity,
            window::window_set_dark_mode,
            window::window_set_docked,
            window::window_frame_rate,
            window::webview_collect_garbage,
            window::webview_pids,
            windows::window_new,
            hotkey::global_hotkey_set,
            dock_menu::dock_menu_set,
            dock_menu::dock_menu_state,
            traffic_lights::window_set_traffic_light_position,
            traffic_lights::window_set_title,
            traffic_lights::window_traffic_light_frame,
            clipboard::clipboard_read,
            clipboard::clipboard_files,
            clipboard::clipboard_write_image,
            clipboard::clipboard_change_count,
            clipboard::clipboard_read_items,
            clipboard::clipboard_write_items,
            clipboard::clipboard_write,
            config::config_read,
            config::config_first_launch,
            config::config_write,
            debug::debug_reply,
            debug::console_log,
            fs::fs_stat,
            fs::fs_read,
            fs::fs_write,
            fs::fs_readdir,
            fs::fs_realpath,
            fs::fs_mkdir,
            fs::fs_remove,
            fs::fs_rename,
            fs::fs_chmod,
            keychain::keychain_get,
            keychain::keychain_set,
            keychain::keychain_delete,
            keychain::keychain_find,
            net::net_connect,
            net::net_attach,
            net::net_write,
            net::net_ack,
            net::net_end,
            net::net_destroy,
            net::net_set_no_delay,
            net::net_listen,
            net::net_close_server,
            net::net_reset,
            pty::pty_spawn,
            pty::pty_attach,
            pty::pty_write,
            pty::pty_ack,
            pty::pty_resize,
            pty::pty_kill,
            pty::pty_release,
            pty::process_children,
            pty::process_cwd,
            pty::default_shell,
            ssh::ssh_release,
            ssh::ssh_reset,
            ssh::key::ssh_parse_key,
            ssh::key::ssh_supported_algorithms,
            ssh::transport::ssh_transport_new,
            ssh::client::ssh_connect,
            ssh::client::ssh_answer_server_key,
            ssh::client::ssh_authenticate,
            ssh::client::ssh_client,
            ssh::client::ssh_ack,
            ssh::client::ssh_debug_read_stream,
            ssh::channel::ssh_channel_activate,
            ssh::channel::ssh_channel_activate_sftp,
            ssh::channel::ssh_channel_request,
            ssh::channel::ssh_channel_data,
            ssh::sftp::ssh_sftp,
            ssh::sftp::ssh_sftp_file_read,
            ssh::sftp::ssh_sftp_file_write,
            ssh::sftp::ssh_sftp_file_flush,
            ssh::sftp::ssh_sftp_file_shutdown,
            ssh::agent::ssh_agent_connect,
            ssh::agent::ssh_agent_write,
            ssh::agent::ssh_agent_close,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tabbi");
}
