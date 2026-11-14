// Audio bypass through an external mpv process.
//
// WebView2 can never decode Dolby TrueHD / DTS-HD / Atmos, so those files play
// with picture and no sound. Instead of transcoding, we spawn mpv with
// --no-video and drive it over its named-pipe IPC: it decodes the audio
// natively, WebView2 keeps rendering the video, and the two clocks are aligned
// from the frontend.
//
// mpv stays a separate process on purpose: no linking against libmpv (mpv is
// GPLv2+, linking would drag this MIT project into GPL), a crash cannot take
// the player down with it, and the binary can be updated without a rebuild.

use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct MpvProc {
    child: Option<Child>,
    pipe: Option<File>,
    pipe_name: String,
}

impl MpvProc {
    pub fn new() -> Self {
        MpvProc { child: None, pipe: None, pipe_name: String::new() }
    }

    fn send(&mut self, json: &str) -> Result<(), String> {
        let pipe = self.pipe.as_mut().ok_or("mpv not running")?;
        let line = format!("{json}\n");
        pipe.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        pipe.flush().map_err(|e| e.to_string())?;
        Ok(())
    }

    fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.pipe = None;
    }
}

pub type MpvState = Arc<Mutex<MpvProc>>;

/// Locate mpv.exe: MPV_PATH env override, bundled resource dir, then the
/// dev-time copy inside the repo.
fn resolve_mpv(app: &AppHandle) -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("MPV_PATH") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = app.path().resource_dir() {
        candidates.push(dir.join("mpv").join("mpv.exe"));
        candidates.push(dir.join("mpv.exe"));
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources").join("mpv").join("mpv.exe"),
    );
    for c in candidates {
        if c.is_file() {
            return Ok(c);
        }
    }
    Err("mpv.exe not found (bundled resource missing and MPV_PATH unset)".into())
}

fn log_tail(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let lines: Vec<&str> = s.lines().rev().take(4).collect();
            lines.into_iter().rev().collect::<Vec<_>>().join(" | ")
        }
        Err(_) => String::new(),
    }
}

/// Blocking part of startup: spawn mpv, wait for the IPC pipe, start the event
/// pump. Runs on a worker thread so the UI thread never waits on it.
///
/// The state mutex is only held for short reads/writes: holding it across the
/// pipe poll would deadlock the main thread when mpv_load() takes the same lock
/// (that froze the whole window once already).
fn launch(_app: &AppHandle, state: &MpvState, bin: &Path, pipe_name: String) -> Result<String, String> {
    let pipe_path = format!(r"\\.\pipe\{pipe_name}");
    let log_path = std::env::temp_dir().join("danmuku-mpv.log");

    {
        let mut g = state.lock().unwrap();
        if g.child.is_some() && g.pipe.is_some() {
            return Ok(pipe_name);
        }
        g.kill();
    }

    let mut cmd = Command::new(bin);
    cmd.args([
        "--no-video",
        "--idle=yes",
        "--no-terminal",
        "--no-input-default-bindings",
        "--no-config",
        "--audio-display=no",
        "--msg-level=all=warn",
        &format!("--input-ipc-server={pipe_path}"),
    ])
    .arg(format!("--log-file={}", log_path.display()))
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut child = cmd.spawn().map_err(|e| format!("spawn mpv failed: {e}"))?;

    // The pipe only exists once mpv is settled; poll briefly (lock NOT held).
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut file: Option<File> = None;
    while Instant::now() < deadline {
        if let Ok(f) = OpenOptions::new().read(true).write(true).open(&pipe_path) {
            file = Some(f);
            break;
        }
        std::thread::sleep(Duration::from_millis(60));
    }
    let file = match file {
        Some(f) => f,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("mpv IPC pipe did not appear [{}]", log_tail(&log_path)));
        }
    };

    {
        let mut g = state.lock().unwrap();
        g.child = Some(child);
        g.pipe = Some(file);
        g.pipe_name = pipe_name.clone();
        // No observe_property here: a persistent event stream would fill the
        // pipe buffer that nobody drains, and draining it from another thread
        // deadlocks with writes (same FILE_OBJECT). Transport is driven one way
        // (video -> mpv) and mpv needs no queries for that.
    }

    Ok(pipe_name)
}

/// Start mpv (idle) and connect the IPC pipe. Async so the blocking pipe poll
/// happens off the UI thread.
#[tauri::command]
pub async fn mpv_start(app: AppHandle, state: tauri::State<'_, MpvState>) -> Result<String, String> {
    let bin = resolve_mpv(&app)?;
    let pipe_name = format!("danmuku-player-mpv-{}", std::process::id());
    let state_arc = state.inner().clone();
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || launch(&app2, &state_arc, &bin, pipe_name))
        .await
        .map_err(|e| format!("mpv start task failed: {e}"))?
}

#[tauri::command]
pub async fn mpv_load(
    state: tauri::State<'_, MpvState>,
    path: String,
    audio_index: usize,
) -> Result<(), String> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut g = st.lock().unwrap();
        let p = path.replace('\\', "\\\\");
        // loadfile does not accept an options object in this mpv build
        // ("invalid parameter"), so select the track right after loading it.
        let aid = audio_index + 1;
        g.send(&format!(r#"{{"command":["loadfile","{p}","replace"]}}"#))?;
        g.send(&format!(r#"{{"command":["set_property","aid",{aid}]}}"#))?;
        Ok(())
    })
    .await
    .map_err(|e| format!("mpv load task failed: {e}"))?
}

#[tauri::command]
pub async fn mpv_seek(state: tauri::State<'_, MpvState>, seconds: f64) -> Result<(), String> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut g = st.lock().unwrap();
        g.send(&format!(r#"{{"command":["seek",{seconds},"absolute"]}}"#))
    })
    .await
    .map_err(|e| format!("mpv seek task failed: {e}"))?
}

#[tauri::command]
pub async fn mpv_set(
    state: tauri::State<'_, MpvState>,
    name: String,
    value: serde_json::Value,
) -> Result<(), String> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let v = serde_json::to_string(&value).map_err(|e| e.to_string())?;
        let mut g = st.lock().unwrap();
        g.send(&format!(r#"{{"command":["set_property","{name}",{v}]}}"#))
    })
    .await
    .map_err(|e| format!("mpv set task failed: {e}"))?
}

#[tauri::command]
pub fn mpv_stop(state: tauri::State<'_, MpvState>) -> Result<(), String> {
    let mut guard = state.lock().unwrap();
    guard.kill();
    Ok(())
}

#[derive(Serialize)]
pub struct MpvStatus {
    pub running: bool,
    pub pipe: String,
}

#[tauri::command]
pub fn mpv_status(state: tauri::State<'_, MpvState>) -> MpvStatus {
    let guard = state.lock().unwrap();
    MpvStatus {
        running: guard.child.is_some() && guard.pipe.is_some(),
        pipe: guard.pipe_name.clone(),
    }
}

pub fn shutdown(state: &MpvState) {
    state.lock().unwrap().kill();
}
