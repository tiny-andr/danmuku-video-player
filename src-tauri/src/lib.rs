// Bplayer 桌面版后端：脱离浏览器沙箱，直接以真实路径读盘。
// - pair_danmaku：视频同目录同名 .xml/.json 配对（xml 优先，忽略大小写）
// - scan_folder：剧集列表（视频 + 同名弹幕标记）
// - 视频流走 Tauri 官方 asset 协议（http://asset.localhost，原生支持 Range 跳转）
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            fs_stat,
            pair_danmaku,
            scan_folder,
            read_text,
            pick_video,
            pick_folder
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}