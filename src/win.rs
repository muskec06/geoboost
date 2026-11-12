//! Windows integration: DPAPI, system proxy (WinINet), git, user env vars, autostart,
//! single instance, tray icon and session-end handling.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::ptr::{null, null_mut};
use std::sync::{mpsc, Arc, OnceLock};

use serde::{Deserialize, Serialize};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, LocalFree, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Networking::WinInet::{
    InternetQueryOptionW, InternetSetOptionW, INTERNET_OPTION_PER_CONNECTION_OPTION, INTERNET_OPTION_REFRESH,
    INTERNET_OPTION_SETTINGS_CHANGED, INTERNET_PER_CONN_AUTOCONFIG_URL, INTERNET_PER_CONN_FLAGS, INTERNET_PER_CONN_OPTIONW,
    INTERNET_PER_CONN_OPTION_LISTW, INTERNET_PER_CONN_PROXY_BYPASS, INTERNET_PER_CONN_PROXY_SERVER, PROXY_TYPE_DIRECT,
    PROXY_TYPE_PROXY,
};
use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, MessageBoxW, MsgWaitForMultipleObjects, PeekMessageW,
    RegisterClassW, SendMessageTimeoutW, TranslateMessage, HWND_BROADCAST, MB_ICONINFORMATION, MB_OK, MSG, PM_REMOVE,
    QS_ALLINPUT, SMTO_ABORTIFHUNG, SW_SHOWNORMAL, WM_ENDSESSION, WM_QUERYENDSESSION, WM_QUIT, WM_SETTINGCHANGE, WNDCLASSW,
};

use crate::app::{App, Phase};
use crate::config::{data_dir, Config};
use crate::logx::log;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ENV_KEYS: [&str; 4] = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"];

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

unsafe fn from_wide_ptr(p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)))
}

// ---------------------------------------------------------------- DPAPI

pub fn protect(raw: &[u8]) -> Result<String, String> {
    unsafe {
        let inp = CRYPT_INTEGER_BLOB { cbData: raw.len() as u32, pbData: raw.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        let desc = wide("GeoBoost PSK");
        if CryptProtectData(&inp, desc.as_ptr(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) == 0 {
            return Err("Could not encrypt the PSK (DPAPI)".into());
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        Ok(format!("dpapi:{}", hex::encode(v)))
    }
}

pub fn unprotect(s: &str) -> Option<Vec<u8>> {
    let blob = hex::decode(s.strip_prefix("dpapi:")?).ok()?;
    unsafe {
        let inp = CRYPT_INTEGER_BLOB { cbData: blob.len() as u32, pbData: blob.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        if CryptUnprotectData(&inp, null_mut(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) == 0 {
            return None;
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    }
}

// ---------------------------------------------------------------- WinINet proxy

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct ProxySnap {
    flags: u32,
    server: Option<String>,
    bypass: Option<String>,
    autoconfig: Option<String>,
}

fn query_proxy() -> Option<ProxySnap> {
    unsafe {
        let mut opts: [INTERNET_PER_CONN_OPTIONW; 4] = std::mem::zeroed();
        opts[0].dwOption = INTERNET_PER_CONN_FLAGS;
        opts[1].dwOption = INTERNET_PER_CONN_PROXY_SERVER;
        opts[2].dwOption = INTERNET_PER_CONN_PROXY_BYPASS;
        opts[3].dwOption = INTERNET_PER_CONN_AUTOCONFIG_URL;
        let mut list: INTERNET_PER_CONN_OPTION_LISTW = std::mem::zeroed();
        list.dwSize = std::mem::size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32;
        list.pszConnection = null_mut();
        list.dwOptionCount = 4;
        list.pOptions = opts.as_mut_ptr();
        let mut size = list.dwSize;
        if InternetQueryOptionW(null_mut(), INTERNET_OPTION_PER_CONNECTION_OPTION, &mut list as *mut _ as *mut _, &mut size) == 0 {
            return None;
        }
        let take = |o: &INTERNET_PER_CONN_OPTIONW| {
            let p = o.Value.pszValue;
            let s = from_wide_ptr(p);
            if !p.is_null() {
                GlobalFree(p as _);
            }
            s
        };
        Some(ProxySnap { flags: opts[0].Value.dwValue, server: take(&opts[1]), bypass: take(&opts[2]), autoconfig: take(&opts[3]) })
    }
}

fn set_proxy(snap: &ProxySnap) -> bool {
    unsafe {
        let server = snap.server.as_deref().map(wide);
        let bypass = snap.bypass.as_deref().map(wide);
        let auto = snap.autoconfig.as_deref().map(wide);
        let mut opts: [INTERNET_PER_CONN_OPTIONW; 4] = std::mem::zeroed();
        opts[0].dwOption = INTERNET_PER_CONN_FLAGS;
        opts[0].Value.dwValue = snap.flags;
        opts[1].dwOption = INTERNET_PER_CONN_PROXY_SERVER;
        opts[1].Value.pszValue = server.as_ref().map(|v| v.as_ptr() as *mut u16).unwrap_or(null_mut());
        opts[2].dwOption = INTERNET_PER_CONN_PROXY_BYPASS;
        opts[2].Value.pszValue = bypass.as_ref().map(|v| v.as_ptr() as *mut u16).unwrap_or(null_mut());
        opts[3].dwOption = INTERNET_PER_CONN_AUTOCONFIG_URL;
        opts[3].Value.pszValue = auto.as_ref().map(|v| v.as_ptr() as *mut u16).unwrap_or(null_mut());
        let mut list: INTERNET_PER_CONN_OPTION_LISTW = std::mem::zeroed();
        list.dwSize = std::mem::size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32;
        list.pszConnection = null_mut();
        list.dwOptionCount = 4;
        list.pOptions = opts.as_mut_ptr();
        let ok = InternetSetOptionW(
            null_mut(),
            INTERNET_OPTION_PER_CONNECTION_OPTION,
            &list as *const _ as *const _,
            list.dwSize,
        ) != 0;
        InternetSetOptionW(null_mut(), INTERNET_OPTION_SETTINGS_CHANGED, null(), 0);
        InternetSetOptionW(null_mut(), INTERNET_OPTION_REFRESH, null(), 0);
        ok
    }
}

fn bypass_list(cfg: &Config) -> String {
    let mut v = vec![
        "<local>", "localhost", "127.*", "10.*", "192.168.*", "169.254.*", "172.16.*", "172.17.*", "172.18.*",
        "172.19.*", "172.20.*", "172.21.*", "172.22.*", "172.23.*", "172.24.*", "172.25.*", "172.26.*", "172.27.*",
        "172.28.*", "172.29.*", "172.30.*", "172.31.*",
    ]
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();
    for x in cfg.bypass.split(';').map(str::trim).filter(|x| !x.is_empty()) {
        v.push(x.to_string());
    }
    // Never route the tunnel's own server through the tunnel.
    v.push(cfg.server_host.trim().to_string());
    v.join(";")
}

pub fn set_proxy_paused(paused: bool) {
    if let Some(mut s) = query_proxy() {
        s.flags = if paused { PROXY_TYPE_DIRECT } else { PROXY_TYPE_DIRECT | PROXY_TYPE_PROXY };
        set_proxy(&s);
    }
}

// ---------------------------------------------------------------- git

fn git(args: &[&str]) -> Option<std::process::Output> {
    Command::new("git").args(args).creation_flags(CREATE_NO_WINDOW).output().ok()
}

fn git_get_proxy() -> Result<Option<String>, ()> {
    let o = git(&["config", "--global", "--get", "http.proxy"]).ok_or(())?;
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    Ok(if o.status.success() && !s.is_empty() { Some(s) } else { None })
}

fn git_set_proxy(v: Option<&str>) -> bool {
    match v {
        Some(v) => git(&["config", "--global", "http.proxy", v]).map(|o| o.status.success()).unwrap_or(false),
        None => {
            git(&["config", "--global", "--unset-all", "http.proxy"]);
            true
        }
    }
}

// ---------------------------------------------------------------- user environment

fn env_key() -> std::io::Result<RegKey> {
    RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)
}

fn broadcast_env_change() {
    let s = wide("Environment");
    unsafe {
        let mut res = 0usize;
        SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, s.as_ptr() as LPARAM, SMTO_ABORTIFHUNG, 3000, &mut res);
    }
}

// ---------------------------------------------------------------- apply / restore

#[derive(Serialize, Deserialize, Default)]
struct Restore {
    proxy: Option<ProxySnap>,
    git: Option<Option<String>>,
    env: Option<Vec<(String, Option<String>)>>,
}

fn restore_path() -> std::path::PathBuf {
    data_dir().join("restore.json")
}

pub fn apply_integration(cfg: &Config) -> Result<(), String> {
    // If a previous session crashed, put the original settings back first.
    restore_integration();

    let mut rs = Restore::default();
    let mut warnings = Vec::new();
    if cfg.set_system_proxy {
        match query_proxy() {
            Some(s) => rs.proxy = Some(s),
            None => warnings.push("could not read the current proxy settings".to_string()),
        }
    }
    if cfg.set_git {
        match git_get_proxy() {
            Ok(v) => rs.git = Some(v),
            Err(()) => warnings.push("git not found, git setting skipped".to_string()),
        }
    }
    if cfg.set_env {
        if let Ok(k) = env_key() {
            rs.env = Some(ENV_KEYS.iter().map(|n| (n.to_string(), k.get_value::<String, _>(n).ok())).collect());
        }
    }
    // Persist the snapshot BEFORE touching anything, so a crash can always be undone.
    std::fs::create_dir_all(data_dir()).ok();
    std::fs::write(restore_path(), serde_json::to_vec_pretty(&rs).unwrap()).map_err(|e| format!("could not write the restore file: {e}"))?;

    if rs.proxy.is_some() {
        let snap = ProxySnap {
            flags: PROXY_TYPE_DIRECT | PROXY_TYPE_PROXY,
            server: Some(format!("127.0.0.1:{}", cfg.http_port)),
            bypass: Some(bypass_list(cfg)),
            autoconfig: None,
        };
        if !set_proxy(&snap) {
            warnings.push("could not set the system proxy".into());
        }
    }
    if rs.git.is_some() && !git_set_proxy(Some(&format!("socks5h://127.0.0.1:{}", cfg.socks_port))) {
        warnings.push("could not set the git proxy".into());
    }
    if rs.env.is_some() {
        if let Ok(k) = env_key() {
            let http = format!("http://127.0.0.1:{}", cfg.http_port);
            let _ = k.set_value("HTTP_PROXY", &http);
            let _ = k.set_value("HTTPS_PROXY", &http);
            let _ = k.set_value("ALL_PROXY", &format!("socks5h://127.0.0.1:{}", cfg.socks_port));
            let _ = k.set_value("NO_PROXY", &format!("localhost,127.0.0.1,::1,{}", cfg.server_host.trim()));
            broadcast_env_change();
        }
    }
    if warnings.is_empty() {
        Ok(())
    } else {
        Err(warnings.join("; "))
    }
}

/// Restore everything recorded in restore.json (normal disconnect, exit, or crash recovery).
/// Also picks up a leftover restore file from the pre-rename "satmux" builds (v0.1.x).
pub fn restore_integration() {
    if let Some(parent) = data_dir().parent() {
        restore_from(&parent.join("satmux").join("restore.json"));
    }
    restore_from(&restore_path());
}

fn restore_from(p: &std::path::Path) {
    let Ok(data) = std::fs::read(p) else { return };
    let Ok(rs) = serde_json::from_slice::<Restore>(&data) else {
        let _ = std::fs::remove_file(p);
        return;
    };
    if let Some(s) = &rs.proxy {
        set_proxy(s);
    }
    if let Some(g) = &rs.git {
        git_set_proxy(g.as_deref());
    }
    if let Some(env) = &rs.env {
        if let Ok(k) = env_key() {
            for (n, v) in env {
                match v {
                    Some(v) => {
                        let _ = k.set_value(n, v);
                    }
                    None => {
                        let _ = k.delete_value(n);
                    }
                }
            }
            broadcast_env_change();
        }
    }
    let _ = std::fs::remove_file(p);
    log("Previous system settings restored");
}

// ---------------------------------------------------------------- misc

pub fn set_autostart(on: bool) -> Result<(), String> {
    let run = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(r"Software\Microsoft\Windows\CurrentVersion\Run", KEY_READ | KEY_WRITE)
        .map_err(|e| e.to_string())?;
    if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        run.set_value("GeoBoost", &format!("\"{}\" --autostart", exe.display())).map_err(|e| e.to_string())
    } else {
        let _ = run.delete_value("GeoBoost");
        Ok(())
    }
}

pub fn open_url(url: &str) {
    let op = wide("open");
    let u = wide(url);
    unsafe {
        ShellExecuteW(null_mut(), op.as_ptr(), u.as_ptr(), null(), null(), SW_SHOWNORMAL);
    }
}

pub fn message_box(text: &str) {
    let t = wide(text);
    let c = wide("GeoBoost");
    unsafe {
        MessageBoxW(null_mut(), t.as_ptr(), c.as_ptr(), MB_OK | MB_ICONINFORMATION);
    }
}

/// Returns false if another satmux is already running for this user.
pub fn single_instance() -> bool {
    let name = wide("Local\\geoboost-client-v1");
    unsafe {
        let h = CreateMutexW(null(), 0, name.as_ptr());
        !(h.is_null() || GetLastError() == ERROR_ALREADY_EXISTS)
        // The handle is intentionally leaked: it lives as long as the process.
    }
}

// ---------------------------------------------------------------- tray + message loop

static GLOBAL_APP: OnceLock<Arc<App>> = OnceLock::new();

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            if wp != 0 {
                if let Some(app) = GLOBAL_APP.get() {
                    app.disconnect_inner();
                }
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn create_session_window() {
    unsafe {
        let class = wide("geoboost_session_watch");
        let hinst = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        // A hidden top-level window (not message-only) so it receives WM_QUERYENDSESSION/WM_ENDSESSION.
        CreateWindowExW(0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0, null_mut(), null_mut(), hinst, null());
    }
}

fn make_icon(p: Phase) -> tray_icon::Icon {
    let (r, g, b) = match p {
        Phase::Off => (136u8, 135u8, 128u8),
        Phase::Connecting => (239, 159, 39),
        Phase::Connected => (29, 158, 117),
        Phase::Fallback => (226, 75, 74),
    };
    const N: usize = 32;
    let mut px = vec![0u8; N * N * 4];
    let c = (N as f32 - 1.0) / 2.0;
    for y in 0..N {
        for x in 0..N {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let i = (y * N + x) * 4;
            let outer = 15.0;
            if d <= outer {
                // Ring with a hollow centre dot: reads as a "link" glyph at 16 px.
                let a = ((outer - d).min(1.0) * 255.0) as u8;
                let (cr, cg, cb) = if d < 5.5 { (255, 255, 255) } else if d < 8.0 { (r, g, b) } else if d < 10.0 { (255, 255, 255) } else { (r, g, b) };
                px[i] = cr;
                px[i + 1] = cg;
                px[i + 2] = cb;
                px[i + 3] = a;
            }
        }
    }
    tray_icon::Icon::from_rgba(px, N as u32, N as u32).expect("icon")
}

pub fn run_gui(app: Arc<App>, open_panel_now: bool) {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let _ = GLOBAL_APP.set(app.clone());
    create_session_window();

    let menu = Menu::new();
    let open_i = MenuItem::new("Open panel", true, None);
    let conn_i = MenuItem::new("Connect", true, None);
    let quit_i = MenuItem::new("Quit (restore settings)", true, None);
    let _ = menu.append_items(&[&open_i, &conn_i, &PredefinedMenuItem::separator(), &quit_i]);
    let tray = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("GeoBoost — Off")
        .with_icon(make_icon(Phase::Off))
        .build()
    {
        Ok(t) => t,
        Err(e) => {
            message_box(&format!("Could not create the tray icon: {e}"));
            return;
        }
    };

    let (tx, rx) = mpsc::channel::<(Phase, usize, usize)>();
    app.set_on_change(Box::new(move |p, r, t| {
        let _ = tx.send((p, r, t));
    }));
    if open_panel_now {
        open_url(&app.panel_url());
    }

    loop {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT {
                    return;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == *open_i.id() {
                open_url(&app.panel_url());
            } else if ev.id == *conn_i.id() {
                let a = app.clone();
                if a.is_running() {
                    app.rt.spawn(async move { a.disconnect().await });
                } else {
                    app.rt.spawn(async move {
                        if let Err(e) = a.connect().await {
                            let url = a.panel_url();
                            message_box(&format!("Could not connect: {e}"));
                            open_url(&url);
                        }
                    });
                }
            } else if ev.id == *quit_i.id() {
                return;
            }
        }
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                open_url(&app.panel_url());
            }
        }
        while let Ok((p, r, t)) = rx.try_recv() {
            let _ = tray.set_icon(Some(make_icon(p)));
            let tip = if p == Phase::Off { "GeoBoost — Off".to_string() } else { format!("GeoBoost — {} ({r}/{t} carriers)", p.label()) };
            let _ = tray.set_tooltip(Some(tip));
            conn_i.set_text(if p == Phase::Off { "Connect" } else { "Disconnect" });
        }
        unsafe {
            MsgWaitForMultipleObjects(0, null(), 0, 100, QS_ALLINPUT);
        }
    }
}
