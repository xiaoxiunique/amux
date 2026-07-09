//! macOS control-center endpoints: list running / installed apps, open & quit
//! apps, fetch app icons, and capture full-screen / per-app-window screenshots.
//!
//! Ported from the Agent Port reference service. All shell-outs (`ps`,
//! `/usr/sbin/screencapture`, `sips`, PlistBuddy, `osascript`, `/usr/bin/open`,
//! `xcrun simctl`) are macOS-specific; the handlers still compile on other
//! platforms but their capture/launch calls will simply fail at runtime.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, Response, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::serve::server::{is_authed, json_response, AppState};

/// A running foreground GUI application on the host Mac.
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RunningApp {
    name: String,
    path: String,
    pid: u32,
    memory_bytes: u64,
    cpu_percent: f64,
}

fn collect_running_apps() -> Vec<RunningApp> {
    let Some(out) = command_stdout("ps", &["-axo", "pid=,rss=,pcpu=,args="]) else {
        return Vec::new();
    };
    let mut by_path: HashMap<String, RunningApp> = HashMap::new();
    let user_apps = env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| format!("{h}/Applications/"));
    for line in out.lines() {
        let line = line.trim_start();
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(rss), Some(pcpu)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let args: String = fields.collect::<Vec<_>>().join(" ");
        let Some(idx) = args.find(".app/Contents/MacOS/") else {
            continue;
        };
        let bundle_path = &args[..idx + 4]; // include ".app"
        // Drop nested helper/framework/XPC bundles — but NOT real apps that ship
        // inside another app (e.g. Simulator/Instruments under
        // Xcode.app/Contents/Developer/Applications). Only skip when the bundle
        // lives under a known helper subdirectory.
        const HELPER_DIRS: &[&str] = &[
            "/Contents/Frameworks/",
            "/Contents/PlugIns/",
            "/Contents/XPCServices/",
            "/Contents/Library/",
            "/Contents/Helpers/",
            "/Contents/Resources/",
        ];
        if HELPER_DIRS.iter().any(|dir| bundle_path.contains(dir)) {
            continue;
        }
        // Only user-facing apps (those in an Applications folder, i.e. shown in
        // the Dock). Excludes /System/Library/CoreServices system agents like
        // Notification Center / Control Center / Spotlight.
        let is_dock_app = bundle_path.starts_with("/Applications/")
            || bundle_path.starts_with("/System/Applications/")
            || user_apps
                .as_ref()
                .is_some_and(|prefix| bundle_path.starts_with(prefix));
        if !is_dock_app {
            continue;
        }
        let name = bundle_path
            .rsplit('/')
            .next()
            .unwrap_or(bundle_path)
            .trim_end_matches(".app")
            .to_string();
        let pid = pid.parse::<u32>().unwrap_or(0);
        let mem = rss.parse::<u64>().unwrap_or(0) * 1024; // rss is KB
        let cpu = pcpu.parse::<f64>().unwrap_or(0.0);

        by_path
            .entry(bundle_path.to_string())
            .and_modify(|app| {
                app.memory_bytes += mem;
                if cpu > app.cpu_percent {
                    app.cpu_percent = cpu;
                }
            })
            .or_insert(RunningApp {
                name,
                path: bundle_path.to_string(),
                pid,
                memory_bytes: mem,
                cpu_percent: cpu,
            });
    }
    let mut apps: Vec<RunningApp> = by_path.into_values().collect();
    apps.sort_by(|a, b| b.memory_bytes.cmp(&a.memory_bytes));
    apps
}

/// An installed `.app` bundle on disk.
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct InstalledApp {
    name: String,
    path: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAppRequest {
    path: Option<String>,
}

/// Scan the standard Applications folders for installed `.app` bundles (one
/// level deep, e.g. /Applications/Utilities).
fn collect_installed_apps() -> Vec<InstalledApp> {
    let mut dirs = vec![
        "/Applications".to_string(),
        "/System/Applications".to_string(),
    ];
    if let Ok(home) = env::var("HOME") {
        if !home.is_empty() {
            dirs.push(format!("{home}/Applications"));
        }
    }
    let mut apps: Vec<InstalledApp> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for dir in dirs {
        scan_apps_dir(&dir, true, &mut apps, &mut seen);
    }
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}

fn scan_apps_dir(
    dir: &str,
    recurse: bool,
    apps: &mut Vec<InstalledApp>,
    seen: &mut std::collections::HashSet<String>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if file_name.ends_with(".app") && !file_name.starts_with('.') {
            let path_str = path.to_string_lossy().into_owned();
            if seen.insert(path_str.clone()) {
                apps.push(InstalledApp {
                    name: file_name.trim_end_matches(".app").to_string(),
                    path: path_str,
                });
            }
        } else if recurse && path.is_dir() && !file_name.starts_with('.') {
            scan_apps_dir(&path.to_string_lossy(), false, apps, seen);
        }
    }
}

static APP_ICON_CACHE: LazyLock<Mutex<HashMap<String, Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static APP_ICON_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Render an app bundle's icon to a 128px PNG (cached by bundle path). Returns
/// None for apps whose icon lives in an asset catalog (no `.icns`).
fn app_icon_png(bundle_path: &str) -> Option<Vec<u8>> {
    if let Some(cached) = lock_recover(&APP_ICON_CACHE).get(bundle_path) {
        return Some(cached.clone());
    }
    let resources = format!("{bundle_path}/Contents/Resources");
    let info_plist = format!("{bundle_path}/Contents/Info.plist");
    let mut icns: Option<String> = command_stdout(
        "/usr/libexec/PlistBuddy",
        &["-c", "Print :CFBundleIconFile", &info_plist],
    )
    .and_then(clean_command_output)
    .map(|name| {
        let name = name.trim();
        if name.ends_with(".icns") {
            format!("{resources}/{name}")
        } else {
            format!("{resources}/{name}.icns")
        }
    })
    .filter(|p| Path::new(p).exists());
    if icns.is_none() {
        if let Ok(entries) = fs::read_dir(&resources) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("icns") {
                    icns = Some(p.to_string_lossy().into_owned());
                    break;
                }
            }
        }
    }
    let icns = icns?;
    let id = APP_ICON_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let out = std::env::temp_dir().join(format!("agentport-icon-{id}.png"));
    let out_str = out.to_string_lossy().into_owned();
    let status = Command::new("sips")
        .args(["-s", "format", "png", "-Z", "128", &icns, "--out", &out_str])
        .output()
        .ok()?;
    if !status.status.success() {
        return None;
    }
    let bytes = fs::read(&out).ok()?;
    let _ = fs::remove_file(&out);
    lock_recover(&APP_ICON_CACHE).insert(bundle_path.to_string(), bytes.clone());
    Some(bytes)
}

#[derive(Debug, Deserialize)]
pub(crate) struct QuitAppRequest {
    name: Option<String>,
}

static SCREEN_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Capture the main display to a downscaled JPEG (requires Screen Recording
/// permission for the host process).
fn capture_screen() -> Option<Vec<u8>> {
    let id = SCREEN_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let out = std::env::temp_dir().join(format!("agentport-screen-{id}.jpg"));
    let out_str = out.to_string_lossy().into_owned();
    // -x: silent, -t jpg, -D 1: main display.
    let cap = Command::new("/usr/sbin/screencapture")
        .args(["-x", "-t", "jpg", "-D", "1", &out_str])
        .output()
        .ok()?;
    if !cap.status.success() {
        let _ = fs::remove_file(&out);
        return None;
    }
    // Downscale to fit 1600px (sips edits in place).
    let _ = Command::new("sips").args(["-Z", "1600", &out_str]).output();
    let bytes = fs::read(&out).ok()?;
    let _ = fs::remove_file(&out);
    if bytes.is_empty() {
        return None;
    }
    Some(bytes)
}

/// Find the frontmost normal (layer-0) on-screen window owned by `pid` and
/// return its CGWindowID. CGWindowList is front-to-back ordered, so the first
/// match is the app's frontmost window. Requires Screen Recording permission.
#[cfg(target_os = "macos")]
fn app_main_window_id(pid: u32) -> Option<u32> {
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_graphics::window::{
        copy_window_info, kCGNullWindowID, kCGWindowListExcludeDesktopElements,
        kCGWindowListOptionOnScreenOnly,
    };

    let raw = copy_window_info(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    )?;
    let windows: CFArray<CFDictionary<CFString, CFType>> =
        unsafe { CFArray::wrap_under_get_rule(raw.as_concrete_TypeRef()) };

    let pid_key = CFString::from_static_string("kCGWindowOwnerPID");
    let num_key = CFString::from_static_string("kCGWindowNumber");
    let layer_key = CFString::from_static_string("kCGWindowLayer");

    for w in windows.iter() {
        let owner = w
            .find(&pid_key)
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64());
        if owner != Some(pid as i64) {
            continue;
        }
        let layer = w
            .find(&layer_key)
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64())
            .unwrap_or(0);
        if layer != 0 {
            continue;
        }
        if let Some(num) = w
            .find(&num_key)
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64())
        {
            return Some(num as u32);
        }
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn app_main_window_id(_pid: u32) -> Option<u32> {
    None
}

/// Capture a specific app's main window to a downscaled JPEG (occlusion-proof
/// via `screencapture -l<windowid>`).
/// True if `pid` is the iOS Simulator. Its window doesn't capture usefully via
/// the per-window path, so callers show the whole display instead.
fn pid_is_simulator(pid: u32) -> bool {
    command_stdout("ps", &["-p", &pid.to_string(), "-o", "comm="])
        .map(|path| path.contains("/Simulator.app/"))
        .unwrap_or(false)
}

/// The UDID of the first booted simulator device, if any.
fn first_booted_simulator_udid() -> Option<String> {
    let out = command_stdout("/usr/bin/xcrun", &["simctl", "list", "devices", "booted"])?;
    for line in out.lines() {
        if !line.contains("(Booted)") {
            continue;
        }
        for seg in line.split('(') {
            let candidate = seg.split(')').next().unwrap_or("").trim();
            if candidate.len() == 36 && candidate.matches('-').count() == 4 {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

/// Capture the simulated iOS device's screen (the phone screen, without macOS
/// chrome) as a downscaled JPEG. Returns None when no device is booted.
fn capture_simulator_screen() -> Option<Vec<u8>> {
    let udid = first_booted_simulator_udid()?;
    let id = SCREEN_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let png = std::env::temp_dir().join(format!("agentport-sim-{id}.png"));
    let png_str = png.to_string_lossy().into_owned();
    let cap = Command::new("/usr/bin/xcrun")
        .args(["simctl", "io", &udid, "screenshot", &png_str])
        .output()
        .ok()?;
    if !cap.status.success() {
        let _ = fs::remove_file(&png);
        return None;
    }
    // simctl writes PNG; convert + downscale to JPEG to match the response type.
    let jpg = std::env::temp_dir().join(format!("agentport-sim-{id}.jpg"));
    let jpg_str = jpg.to_string_lossy().into_owned();
    let conv = Command::new("sips")
        .args(["-s", "format", "jpeg", "-Z", "1400", &png_str, "--out", &jpg_str])
        .output();
    let _ = fs::remove_file(&png);
    if !matches!(conv, Ok(ref o) if o.status.success()) {
        let _ = fs::remove_file(&jpg);
        return None;
    }
    let bytes = fs::read(&jpg).ok()?;
    let _ = fs::remove_file(&jpg);
    if bytes.is_empty() {
        return None;
    }
    Some(bytes)
}

fn capture_app_window(pid: u32) -> Option<Vec<u8>> {
    // The iOS Simulator is special-cased: capture the simulated device's screen
    // (the phone screen, via `simctl io <udid> screenshot`) rather than the
    // macOS window chrome. Fall back to the window capture if no device booted.
    if pid_is_simulator(pid) {
        if let Some(bytes) = capture_simulator_screen() {
            return Some(bytes);
        }
    }
    let wid = app_main_window_id(pid)?;
    let id = SCREEN_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let out = std::env::temp_dir().join(format!("agentport-win-{id}.jpg"));
    let out_str = out.to_string_lossy().into_owned();
    let cap = Command::new("/usr/sbin/screencapture")
        .args(["-x", "-o", "-t", "jpg", "-l", &wid.to_string(), &out_str])
        .output()
        .ok()?;
    if !cap.status.success() {
        let _ = fs::remove_file(&out);
        return None;
    }
    let _ = Command::new("sips").args(["-Z", "1400", &out_str]).output();
    let bytes = fs::read(&out).ok()?;
    let _ = fs::remove_file(&out);
    if bytes.is_empty() {
        return None;
    }
    Some(bytes)
}

/// Lock a mutex, recovering the guard even if a previous holder panicked
/// (poisoned). Without this, a single panic while holding a cache mutex
/// poisons it and every later `.lock().expect(...)` panics in turn — resetting
/// every incoming connection until the process restarts.
fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn command_stdout(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn clean_command_output(output: String) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

pub(crate) async fn api_apps(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }

    match tokio::task::spawn_blocking(collect_running_apps).await {
        Ok(apps) => json_response(StatusCode::OK, json!({ "ok": true, "apps": apps })),
        Err(error) => json_response(
            StatusCode::OK,
            json!({ "ok": false, "apps": [], "error": error.to_string() }),
        ),
    }
}

pub(crate) async fn api_apps_installed(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    match tokio::task::spawn_blocking(collect_installed_apps).await {
        Ok(apps) => json_response(StatusCode::OK, json!({ "ok": true, "apps": apps })),
        Err(error) => json_response(
            StatusCode::OK,
            json!({ "ok": false, "apps": [], "error": error.to_string() }),
        ),
    }
}

pub(crate) async fn api_apps_open(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<OpenAppRequest>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let Some(path) = body
        .path
        .as_ref()
        .map(|p| p.trim().to_string())
        .filter(|p| p.ends_with(".app") && Path::new(p).exists())
    else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "valid .app path is required" }),
        );
    };
    let result = tokio::task::spawn_blocking(move || {
        Command::new("/usr/bin/open").arg(&path).output()
    })
    .await;
    match result {
        Ok(Ok(out)) if out.status.success() => {
            json_response(StatusCode::OK, json!({ "ok": true }))
        }
        Ok(Ok(out)) => json_response(
            StatusCode::OK,
            json!({ "ok": false, "error": String::from_utf8_lossy(&out.stderr).trim() }),
        ),
        _ => json_response(StatusCode::OK, json!({ "ok": false, "error": "open failed" })),
    }
}

pub(crate) async fn api_apps_icon(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let Some(path) = query.get("path").filter(|v| !v.is_empty()).cloned() else {
        return json_response(StatusCode::BAD_REQUEST, json!({ "error": "path is required" }));
    };
    match tokio::task::spawn_blocking(move || app_icon_png(&path)).await {
        Ok(Some(bytes)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/png")
            .header(header::CACHE_CONTROL, "max-age=86400")
            .body(Body::from(bytes))
            .expect("response builder"),
        _ => json_response(StatusCode::NOT_FOUND, json!({ "error": "icon not found" })),
    }
}

pub(crate) async fn api_apps_quit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<QuitAppRequest>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let Some(name) = body
        .name
        .as_ref()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
    else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "name is required" }),
        );
    };
    let script = format!("tell application \"{}\" to quit", name.replace('"', ""));
    let result = tokio::task::spawn_blocking(move || {
        Command::new("/usr/bin/osascript")
            .args(["-e", &script])
            .output()
    })
    .await;
    match result {
        Ok(Ok(out)) if out.status.success() => {
            json_response(StatusCode::OK, json!({ "ok": true }))
        }
        Ok(Ok(out)) => json_response(
            StatusCode::OK,
            json!({ "ok": false, "error": String::from_utf8_lossy(&out.stderr).trim() }),
        ),
        _ => json_response(StatusCode::OK, json!({ "ok": false, "error": "quit failed" })),
    }
}

pub(crate) async fn api_screen(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    match tokio::task::spawn_blocking(capture_screen).await {
        Ok(Some(bytes)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(bytes))
            .expect("response builder"),
        _ => json_response(
            StatusCode::OK,
            json!({ "ok": false, "error": "screen capture failed (check Screen Recording permission)" }),
        ),
    }
}

pub(crate) async fn api_app_screenshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let Some(pid) = query.get("pid").and_then(|v| v.parse::<u32>().ok()) else {
        return json_response(StatusCode::BAD_REQUEST, json!({ "error": "pid is required" }));
    };
    match tokio::task::spawn_blocking(move || capture_app_window(pid)).await {
        Ok(Some(bytes)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(bytes))
            .expect("response builder"),
        _ => json_response(
            StatusCode::OK,
            json!({ "ok": false, "error": "no window for app (it may have no visible window)" }),
        ),
    }
}
