// Prevents an additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // elevated copy started by the "repair" button: hosts file + CA, then exit (Windows only)
    #[cfg(windows)]
    {
        if args.len() == 4 && args[1] == "--admin-setup" {
            std::process::exit(forever_launcher_lib::admin_setup(&args[2], &args[3]));
        }
        if args.len() == 3 && args[1] == "--admin-remove" {
            std::process::exit(forever_launcher_lib::admin_remove(&args[2]));
        }
    }
    if args.iter().any(|arg| arg == "--play") {
        forever_launcher_lib::set_autoplay(true);
    }
    // WebKitGTK: the DMA-BUF renderer gives a blank window on some GPU/Wayland setups (NVIDIA...)
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
    forever_launcher_lib::run()
}
