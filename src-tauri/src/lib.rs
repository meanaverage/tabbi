mod clipboard;
mod config;
mod debug;
mod fs;
mod host;
mod keychain;
mod net;
mod pty;
mod ssh;
mod sync;
mod traffic_lights;

pub fn run() {
    let background = debug::background();
    let builder = tauri::Builder::default();
    // A test instance doesn't take over the screen at launch (see debug::background).
    #[cfg(target_os = "macos")]
    let builder = builder.activate_ignoring_other_apps(!background);
    builder
        .plugin(tauri_plugin_dialog::init())
        .manage(pty::Ptys::default())
        .manage(clipboard::Clipboard::default())
        .manage(ssh::Ssh::default())
        .manage(net::Net::default())
        .manage(traffic_lights::TrafficLights::default())
        .register_uri_scheme_protocol("tabbysync", sync::handle)
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            if background {
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            }
            // The window is made here rather than from the config, so that a test instance can stay behind.
            let config = app.config().app.windows.first().cloned().ok_or("no window config")?;
            let builder = tauri::WebviewWindowBuilder::from_config(app.handle(), &config)?
                .focused(!background)
                .visible(!background);
            #[cfg(target_os = "macos")]
            let builder = match debug::web_store() {
                Some(id) => {
                    eprintln!("debug: web storage {}", uuid::Uuid::from_bytes(id).hyphenated().to_string().to_uppercase());
                    builder.data_store_identifier(id)
                }
                None => builder,
            };
            let window = builder.build()?;
            if background {
                debug::keep_behind(&window);
            }
            traffic_lights::redraw(&window.as_ref().window());
            debug::start(app.handle());
            Ok(())
        })
        .on_window_event(|window, event| {
            traffic_lights::on_window_event(window, event);
            debug::on_window_event(window, event);
        })
        .invoke_handler(tauri::generate_handler![
            host::host_info,
            host::open_external,
            traffic_lights::window_set_traffic_light_position,
            traffic_lights::window_set_title,
            traffic_lights::window_traffic_light_frame,
            clipboard::clipboard_read,
            clipboard::clipboard_write,
            config::config_read,
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
