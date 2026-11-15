// Bplayer 桌面版后端：脱离浏览器沙箱，直接以真实路径读盘。
// - pair_danmaku：视频同目录同名 .xml/.json 配对（xml 优先，忽略大小写）
// - scan_folder：剧集列表（视频 + 同名弹幕标记）
// - 视频流走 Tauri 官方 asset 协议（http://asset.localhost，原生支持 Range 跳转）
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use tauri::Manager;

mod autostart;
mod mkv_probe;
mod pgs_render;
mod transcode;

const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "webm", "mov", "mkv", "avi", "ts", "flv", "wmv", "mpg", "mpeg",
];

fn is_video(name: &str) -> bool {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    VIDEO_EXTS.contains(&ext.as_str())
}

fn base_name(name: &str) -> String {
    match name.rfind('.') {
        Some(i) => name[..i].to_string(),
        None => name.to_string(),
    }
}

#[derive(Serialize)]
pub struct FsStat {
    kind: String, // "file" | "dir" | "missing"
    size: u64,
}

#[tauri::command]
fn fs_stat(path: String) -> FsStat {
    match fs::metadata(&path) {
        Ok(m) => FsStat {
            kind: if m.is_dir() { "dir" } else { "file" }.into(),
            size: m.len(),
        },
        Err(_) => FsStat { kind: "missing".into(), size: 0 },
    }
}

#[derive(Serialize)]
pub struct DanmakuPayload {
    name: String,
    text: String,
}

const SUB_EXTS: &[&str] = &["srt", "ass", "ssa", "vtt"];

#[derive(Serialize)]
pub struct SubFile {
    name: String,
    path: String,
}

/// 扫描视频同目录的外挂字幕：基名相同或带语言/标题后缀变体（Movie.srt / Movie.chs.srt）。
/// 跳过本应用自己的缓存产物与 .tmp 半成品，同名文件排最前。
#[tauri::command]
fn scan_subtitles(video_path: String) -> Vec<SubFile> {
    let vp = Path::new(&video_path);
    let dir = match vp.parent() {
        Some(d) => d,
        None => return vec![],
    };
    let base = base_name(&vp.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).to_lowercase();
    let mut out: Vec<SubFile> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let ext = Path::new(&name)
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if !SUB_EXTS.contains(&ext.as_str()) || name.contains(".tmp") {
                continue;
            }
            let stem_lc = base_name(&name).to_lowercase();
            if !(stem_lc == base || stem_lc.starts_with(&format!("{base}."))) {
                continue;
            }
            if name.contains(" (字幕") || name.contains(" (音轨") || name.contains(transcode::CACHE_TAG) {
                continue; // 本应用缓存产物，菜单里另有内嵌轨条目
            }
            out.push(SubFile { name, path: p.to_string_lossy().into_owned() });
        }
    }
    out.sort_by(|a, b| {
        let a_exact = base_name(&a.name).to_lowercase() == base;
        let b_exact = base_name(&b.name).to_lowercase() == base;
        b_exact.cmp(&a_exact).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

/// 读取文件字节并以 base64 返回（字幕编码探测用；字幕文件都很小）。
#[tauri::command]
fn read_bytes(path: String) -> Result<String, String> {
    let bytes = fs::read(&path).map_err(|e| e.to_string())?;
    Ok(b64_encode(&bytes))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// 在视频同目录找同名弹幕：xml 优先于 json，基名忽略大小写。
#[tauri::command]
fn pair_danmaku(video_path: String) -> Option<DanmakuPayload> {
    let vp = Path::new(&video_path);
    let dir = vp.parent()?;
    let base = base_name(&vp.file_name()?.to_string_lossy()).to_lowercase();
    let mut best: Option<(u8, PathBuf)> = None; // (prio, path)，0=xml 1=json
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let name = p.file_name()?.to_string_lossy().to_string();
        let ext = p.extension()?.to_string_lossy().to_ascii_lowercase();
        let prio = match ext.as_str() {
            "xml" => 0u8,
            "json" => 1u8,
            _ => continue,
        };
        if base_name(&name).to_lowercase() != base {
            continue;
        }
        if best.as_ref().map(|(b, _)| prio < *b).unwrap_or(true) {
            best = Some((prio, p));
        }
    }
    let (_, p) = best?;
    let bytes = fs::read(&p).ok()?;
    Some(DanmakuPayload {
        name: p.file_name()?.to_string_lossy().to_string(),
        text: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

#[derive(Serialize)]
pub struct Episode {
    name: String,
    path: String,
    size: u64,
    danmaku: Option<String>,
}

#[derive(Serialize)]
pub struct FolderScan {
    label: String,
    episodes: Vec<Episode>,
}

/// 扫描目录：全部视频按名排序，附带同名弹幕文件名（供播放列表徽标）。
#[tauri::command]
fn scan_folder(dir: String) -> FolderScan {
    let dp = Path::new(&dir);
    let mut videos: Vec<(String, PathBuf, u64)> = Vec::new();
    let mut dan_map: std::collections::HashMap<String, (u8, String)> = std::collections::HashMap::new();
    if let Ok(rd) = fs::read_dir(dp) {
        for entry in rd.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let ext = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            if is_video(&name) {
                // 跳过本应用的转码缓存产物（原基名 (AAC).mkv），避免播放列表出现重复条目
                if transcode::is_cache_name(&name) {
                    continue;
                }
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                videos.push((name, p, size));
            } else if ext == "xml" || ext == "json" {
                let prio = if ext == "xml" { 0u8 } else { 1u8 };
                let key = base_name(&name).to_lowercase();
                let replace = dan_map.get(&key).map(|(p, _)| prio < *p).unwrap_or(true);
                if replace {
                    dan_map.insert(key, (prio, name));
                }
            }
        }
    }
    videos.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    let episodes = videos
        .into_iter()
        .map(|(name, p, size)| {
            let base = base_name(&name).to_lowercase();
            let danmaku = dan_map.get(&base).map(|(_, n)| n.clone());
            Episode { name, path: p.to_string_lossy().to_string(), size, danmaku }
        })
        .collect();
    FolderScan {
        label: dp.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| dir.clone()),
        episodes,
    }
}

#[tauri::command]
fn read_text(path: String) -> Result<String, String> {
    let bytes = fs::read(&path).map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// 探测 MKV/WebM 轨道（音轨 CodecID / 语言 / 标题），供前端开播前提示内核解不了的编码。
#[tauri::command]
fn probe_mkv_tracks(path: String) -> Result<mkv_probe::MkvProbe, String> {
    mkv_probe::probe(&path)
}

// 音轨转码缓存命令直接注册 transcode 模块里的实现（见 generate_handler）

#[tauri::command]
fn pick_video() -> Option<String> {
    rfd::FileDialog::new()
        .set_title("选择视频文件")
        .add_filter("视频", VIDEO_EXTS)
        .pick_file()
        .map(|p| p.to_string_lossy().to_string())
}

#[tauri::command]
fn pick_folder() -> Option<String> {
    rfd::FileDialog::new()
        .set_title("选择剧集文件夹")
        .pick_folder()
        .map(|p| p.to_string_lossy().to_string())
}

/// 在资源管理器里打开产物所在目录。
/// 不用 `/select,` 选中文件：含中文/空格的路径上 explorer 会静默失败，
/// 而这里的目的就是让用户能找到目录去清理缓存，打开目录足够。
#[tauri::command]
fn reveal_path(path: String) -> Result<(), String> {
    // 前端路径可能带正斜杠，统一成 Windows 分隔符
    let path = path.replace('/', "\\");
    let p = Path::new(&path);
    if path.trim().is_empty() {
        return Err("还没有可定位的产物".into());
    }
    let dir: PathBuf = if p.is_dir() {
        p.to_path_buf()
    } else {
        p.parent().map(|d| d.to_path_buf()).ok_or("路径无效")?
    };
    if !dir.is_dir() {
        return Err(format!("目录不存在：{}", dir.to_string_lossy()));
    }
    std::process::Command::new("explorer.exe")
        .arg(dir.as_os_str())
        .spawn()
        .map_err(|e| format!("打开资源管理器失败: {e}"))?;
    Ok(())
}

/// 列出某部片在本目录下已生成的缓存产物（`(音轨N).mka` / `(字幕N).srt`）。
/// 设置页用它告诉用户缓存到底在哪、有几个，方便手动清理。
#[tauri::command]
fn sidecar_cache_files(video_path: String) -> Vec<String> {
    let vp = Path::new(&video_path);
    let Some(dir) = vp.parent() else { return Vec::new() };
    let stem = vp.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if stem.is_empty() {
        return Vec::new();
    }
    let prefix = format!("{stem} (");
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&prefix) || name.contains(".tmp.") {
                continue;
            }
            let lower = name.to_ascii_lowercase();
            if lower.ends_with(".mka") || lower.ends_with(".srt") {
                out.push(e.path().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            fs_stat,
            pair_danmaku,
            scan_folder,
            read_text,
            read_bytes,
            scan_subtitles,
            probe_mkv_tracks,
            transcode::transcode_target,
            transcode::transcode_audio_start,
            transcode::extract_subtitle_start,
            transcode::transcode_audio_cancel,
            transcode::clean_sidecar_tmp,
            autostart::get_autostart,
            autostart::set_autostart,
            pgs_render::pgs_open,
            pgs_render::pgs_image,
            pgs_render::pgs_close,
            pgs_render::pgs_current,
            pick_video,
            pick_folder,
            reveal_path,
            sidecar_cache_files
        ])
        .manage(std::sync::Mutex::new(None::<transcode::TranscodeJob>))
        .manage::<pgs_render::PgsState>(std::sync::Mutex::new(None))
        .on_window_event(|window, event| {
            // 关窗即杀转码子进程，不让 ffmpeg 变孤儿
            if let tauri::WindowEvent::Destroyed = event {
                if let Some(mut job) = window
                    .app_handle()
                    .state::<transcode::JobState>()
                    .lock()
                    .unwrap()
                    .take()
                {
                    job.kill_now();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}