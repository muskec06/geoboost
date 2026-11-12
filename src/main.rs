#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod bench;
mod config;
mod logx;
mod mux;
mod panel;
mod proxy;

#[cfg(not(windows))]
#[path = "nowin.rs"]
mod plat;
#[cfg(windows)]
#[path = "win.rs"]
mod plat;

use app::App;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let has = |f: &str| args.iter().any(|a| a == f);

    let dir = config::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    logx::init(dir.join("geoboost.log"));

    // Emergency switch: undo any leftover proxy/git/env changes and exit.
    if has("--restore") {
        plat::restore_integration();
        return;
    }

    #[cfg(windows)]
    if !plat::single_instance() {
        plat::message_box("GeoBoost is already running. Click its icon in the notification area to open the panel.");
        return;
    }

    // Crash recovery: undo proxy/git/env changes left over from a previous run.
    plat::restore_integration();

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().expect("tokio");
    let app = App::new(rt.handle().clone());
    log_start(&app);

    let (panel_port, auto_connect, first_run) = {
        let c = app.cfg.lock().unwrap();
        (c.panel_port, c.auto_connect, c.server_host.trim().is_empty() || c.psk_protected.is_empty())
    };
    match rt.block_on(config::bind_local(panel_port)) {
        Ok((l, actual)) => {
            if actual != panel_port {
                let mut c = app.cfg.lock().unwrap();
                c.panel_port = actual;
                let _ = config::save(&c);
            }
            rt.spawn(panel::serve(app.clone(), l, actual));
        }
        Err(e) => {
            logx::log(&e);
            #[cfg(windows)]
            plat::message_box(&e);
        }
    }

    if (auto_connect && !first_run) || has("--connect") {
        let a = app.clone();
        rt.spawn(async move {
            let _ = a.connect().await;
        });
    }

    // Test hook: lets automated tests find the per-run panel token.
    if let Ok(p) = std::env::var("GEOBOOST_PANEL_URL_FILE") {
        let _ = std::fs::write(p, app.panel_url());
    }

    #[cfg(windows)]
    {
        let open_now = first_run || (!has("--autostart") && !auto_connect);
        plat::run_gui(app.clone(), open_now);
    }
    #[cfg(not(windows))]
    {
        plat::open_url(&app.panel_url());
        // Headless test mode: run until killed.
        rt.block_on(std::future::pending::<()>());
    }

    app.disconnect_inner();
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
}

fn log_start(app: &App) {
    let _ = app;
    logx::log(concat!("GeoBoost ", env!("CARGO_PKG_VERSION"), " started"));
}
