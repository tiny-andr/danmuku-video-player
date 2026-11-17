// Bplayer 桌面版后端：脱离浏览器沙箱，直接以真实路径读盘。
// - pair_danmaku：视频同目录同名 .xml/.json 配对（xml 优先，忽略大小写）
// - scan_folder：剧集列表（视频 + 同名弹幕标记）
// - 视频流走 Tauri 官方 asset 协议（http://asset.localhost，原生支持 Range 跳转）
use serde::Serialize;
use std::cmp::Ordering;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use tauri::Manager;

mod autostart;
mod blocking;
mod mkv_probe;
mod mkv_sub;
mod open_file;
mod pgs_render;
mod subtitle_text;
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

// ---- 阻塞命令的异步外壳 ----
//
// Tauri 2 的同步命令并不会冻住窗口消息循环（实测：SMB 断连时一次 3.8 秒的 fs_stat
// 期间窗口 Responding 一直是 True），但它和之后所有 invoke 共用同一条串行通道 ——
// 实测同时发起一组命令，慢的那条用了 2997ms，快的那条也被拖到 2997ms 才回来。
// 所以异步化的收益是「命令之间不再互相排队」，不是「窗口不卡」。
//
// 统一形态：async 薄壳只负责把重活丢进 spawn_blocking 线程池（见 blocking.rs），
// 逻辑本体是同步的 *_impl。*_impl 里不碰任何 Tauri 类型，纯逻辑可以直接单测；
// 返回值与兜底语义保持原样，前端调用点零改动。

#[derive(Serialize)]
pub struct FsStat {
    kind: String, // "file" | "dir" | "missing"
    size: u64,
}

/// 池子异常（几乎不可能）时按"文件不存在"兜底，保持命令永不 reject 的语义。
#[tauri::command]
async fn fs_stat(path: String) -> FsStat {
    crate::blocking::blocking(move || fs_stat_impl(&path))
        .await
        .unwrap_or_else(|_| FsStat { kind: "missing".into(), size: 0 })
}

/// 探测类型与大小。SMB 掉线时一次 metadata 能卡 30 秒，是这里必须离线程的首要原因。
fn fs_stat_impl(path: &str) -> FsStat {
    match fs::metadata(path) {
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
async fn scan_subtitles(video_path: String) -> Vec<SubFile> {
    crate::blocking::blocking(move || scan_subtitles_impl(&video_path))
        .await
        .unwrap_or_default()
}

fn scan_subtitles_impl(video_path: &str) -> Vec<SubFile> {
    let vp = Path::new(video_path);
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

/// 单个文件整读进内存的上限（64 MB）。
///
/// 弹幕 XML 正常只有几 MB、字幕文件更小；设上限是因为同目录里一个 2 GB 的损坏
/// .xml 会让 pair_danmaku/read_text 直接读 2 GB —— 那时的表现不是报错，是进程
/// 被 OOM 掉直接消失（用户只会看到播放器自己关了）。
pub(crate) const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;

/// 超限判定单独抽成一行，单测才能按边界喂长度而不必真造 64 MB 文件。
fn too_large(len: u64, limit: u64) -> bool {
    len > limit
}

fn cap_err(len: u64, limit: u64) -> String {
    // 给前端 toast 直接显示，所以用 MB，不甩字节数。
    format!(
        "文件过大：{:.1} MB，超过 {:.0} MB 读取上限",
        len as f64 / (1024.0 * 1024.0),
        limit as f64 / (1024.0 * 1024.0)
    )
}

/// 从已打开的流里限量读取。
///
/// 最终以**实际读出的字节数**判定：metadata 的长度会说谎（文件正在被追加、稀疏文件、
/// 远端盘报的大小不可信），只信读出来的东西。take(limit+1) 保证最坏情况下内存也只到
/// limit+1，不会先把整个文件搬进内存再发现超限。
fn read_stream_capped<R: Read>(mut r: R, limit: u64) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    r.by_ref()
        .take(limit + 1)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if too_large(buf.len() as u64, limit) {
        return Err(cap_err(buf.len() as u64, limit));
    }
    Ok(buf)
}

/// 整读文件但不超过 limit 字节。
///
/// 先看 metadata：明显超限的文件连 open/读都不做，省掉一次白分配。
pub(crate) fn read_capped(path: &str, limit: u64) -> Result<Vec<u8>, String> {
    if let Ok(meta) = fs::metadata(path) {
        if too_large(meta.len(), limit) {
            return Err(cap_err(meta.len(), limit));
        }
    }
    let f = fs::File::open(path).map_err(|e| e.to_string())?;
    read_stream_capped(f, limit)
}

/// 读取文件字节并以 base64 返回（字幕编码探测用；字幕文件都很小）。
#[tauri::command]
async fn read_bytes(path: String) -> Result<String, String> {
    // 读盘失败本来就是 Err 给前端显示；这里只额外把"线程池异常"也并进同一个 Err 通道。
    crate::blocking::blocking(move || read_bytes_impl(&path)).await?
}

fn read_bytes_impl(path: &str) -> Result<String, String> {
    let bytes = read_capped(path, MAX_READ_BYTES)?;
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
async fn pair_danmaku(video_path: String) -> Option<DanmakuPayload> {
    crate::blocking::blocking(move || pair_danmaku_impl(&video_path))
        .await
        .unwrap_or(None)
}

fn pair_danmaku_impl(video_path: &str) -> Option<DanmakuPayload> {
    let vp = Path::new(video_path);
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
    // 超限/读不出来的弹幕文件一律当成"这部片没有弹幕"返回 None，而不是抛 Err：
    // 少挂一份坏弹幕用户还能照常看片，开片流程不该被同目录一个损坏文件打断。
    let bytes = read_capped(&p.to_string_lossy(), MAX_READ_BYTES).ok()?;
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
async fn scan_folder(dir: String) -> FolderScan {
    // 池子异常时给一个"空目录"而不是空标题：前端拿 label 直接显示，空串会像丢了数据。
    let fallback = dir.clone();
    crate::blocking::blocking(move || scan_folder_impl(&dir))
        .await
        .unwrap_or_else(|_| FolderScan { label: folder_label(Path::new(&fallback)), episodes: Vec::new() })
}

/// 列表标题用目录名；盘根这类没有 file_name 的路径退回整条路径。
fn folder_label(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.to_string_lossy().into_owned())
}

/// 自然排序里的一段：连续 ASCII 数字，或连续非数字。
enum NaturalSeg<'a> {
    Num(&'a str),
    Text(&'a str),
}

/// 把（已小写的）文件名切成数字段/非数字段。只在 ASCII 数字边界上切，
/// 多字节 UTF-8 字符内部不会被切开（数字字节 0x30-0x39 不会出现在续字节里）。
fn natural_segments(s: &str) -> Vec<NaturalSeg<'_>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let start = i;
        let digit = b[i].is_ascii_digit();
        while i < b.len() && b[i].is_ascii_digit() == digit {
            i += 1;
        }
        out.push(if digit { NaturalSeg::Num(&s[start..i]) } else { NaturalSeg::Text(&s[start..i]) });
    }
    out
}

/// 两段数字按数值比较（不解析成整数，任意长都安全）：先剥前导零，长度长的数大；
/// 等值时前导零少的排前面（"1.mp4" < "01.mp4" < "001.mp4"），保证排序确定。
fn cmp_digit_run(a: &str, b: &str) -> Ordering {
    let sa = a.trim_start_matches('0');
    let sb = b.trim_start_matches('0');
    let (va, vb) = (if sa.is_empty() { "0" } else { sa }, if sb.is_empty() { "0" } else { sb });
    va.len()
        .cmp(&vb.len())
        .then_with(|| va.cmp(vb))
        .then_with(|| a.len().cmp(&b.len()))
}

/// 自然排序比较（人类排序）：连续数字段按数值比，其余段按小写字符串比。
///
/// 为什么需要：`videos.sort_by(|a, b| a.0.to_lowercase().cmp(...))` 是纯字典序，
/// 「第10集」会排到「第2集」前面，用户打开文件夹看到集数顺序是乱的。手机版用
/// `Intl.Collator(numeric: true)`；桌面 Rust 侧没有这个（不想为一个比较函数引
/// unicode 库），所以自己按段拆开比。
///
/// 规则（与手机版分段规则对齐）：
/// - 数字段按数值比较，前导零不参与数值；数值相同再比前导零个数，`1 < 01 < 001`。
/// - 非数字段按小写字符串比较 —— 大小写不敏感，`a.mp4` 排在 `B.mp4` 前。
/// - 同一位置"数字段 vs 非数字段"时数字在前（与 ASCII 码位顺序一致）。
/// - 所有段都相等时按小写全串兜底，`sort_by` 又是稳定排序，结果确定。
pub(crate) fn natural_cmp(a: &str, b: &str) -> Ordering {
    let la = a.to_lowercase();
    let lb = b.to_lowercase();
    let sa = natural_segments(&la);
    let sb = natural_segments(&lb);
    for (x, y) in sa.iter().zip(sb.iter()) {
        let ord = match (x, y) {
            (NaturalSeg::Num(p), NaturalSeg::Num(q)) => cmp_digit_run(p, q),
            (NaturalSeg::Text(p), NaturalSeg::Text(q)) => p.cmp(q),
            (NaturalSeg::Num(_), NaturalSeg::Text(_)) => Ordering::Less,
            (NaturalSeg::Text(_), NaturalSeg::Num(_)) => Ordering::Greater,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    sa.len().cmp(&sb.len()).then_with(|| la.cmp(&lb))
}

fn scan_folder_impl(dir: &str) -> FolderScan {
    let dp = Path::new(dir);
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
    // 自然排序：数字段按数值比，否则「第10集」会排在「第2集」前面。
    videos.sort_by(|a, b| natural_cmp(&a.0, &b.0));
    let episodes = videos
        .into_iter()
        .map(|(name, p, size)| {
            let base = base_name(&name).to_lowercase();
            let danmaku = dan_map.get(&base).map(|(_, n)| n.clone());
            Episode { name, path: p.to_string_lossy().to_string(), size, danmaku }
        })
        .collect();
    FolderScan { label: folder_label(dp), episodes }
}

#[tauri::command]
async fn read_text(path: String) -> Result<String, String> {
    crate::blocking::blocking(move || read_text_impl(&path)).await?
}

fn read_text_impl(path: &str) -> Result<String, String> {
    let bytes = read_capped(path, MAX_READ_BYTES)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// 探测 MKV/WebM 轨道（音轨 CodecID / 语言 / 标题），供前端开播前提示内核解不了的编码。
///
/// 探测函数本来就只吃路径、不碰 Tauri 类型，所以没有再套一层 *_impl。
#[tauri::command]
async fn probe_mkv_tracks(path: String) -> Result<mkv_probe::MkvProbe, String> {
    crate::blocking::blocking(move || mkv_probe::probe(&path)).await?
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
///
/// 和上面那批命令一样要 async：`is_dir()` 落在 SMB/断连盘上会卡住，
/// 而同步命令占的是所有 invoke 共用的那条串行通道，会把后面的请求一起堵住。
#[tauri::command]
async fn reveal_path(path: String) -> Result<(), String> {
    crate::blocking::blocking(move || reveal_path_impl(&path)).await?
}

fn reveal_path_impl(path: &str) -> Result<(), String> {
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
async fn sidecar_cache_files(video_path: String) -> Vec<String> {
    crate::blocking::blocking(move || sidecar_cache_files_impl(&video_path))
        .await
        .unwrap_or_default()
}

fn sidecar_cache_files_impl(video_path: &str) -> Vec<String> {
    let vp = Path::new(video_path);
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

/// 当前版本号，直接取 `Cargo.toml` 的 `package.version` —— 和安装包同源，
/// 发版时把三处版本号改完这里自动跟上，不用再手抄第四处。
#[tauri::command]
fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 单实例 + 命令行参数转发交给官方插件（2.5.x）：
    //   - 它内部同样用命名互斥量（`{identifier}-sim`）在 webview 创建前拦下第二个实例，
    //     拦它的理由没变：两个实例对同一部片会各起一个 ffmpeg 写同一个 `(音轨N).tmp.mka`，
    //     产物互相覆盖；关窗时的 cancel_all 也管不到对方进程。
    //   - 关键差别是**它会用 WM_COPYDATA 把第二个进程的 argv 转发给第一个实例**。
    //     以前手写的闸门只会静默 exit(0)，于是「程序已经开着时双击另一个视频」等于
    //     什么都没发生 —— 那比不开还糟。插件失败时只 warn（互斥量建不出来也照样放行），
    //     行为和原来手写版的 fail-open 一致。
    //   - 必须注册成**第一个 plugin**：它要在其它插件初始化与 webview 创建之前跑完，
    //     第二个实例才不会先闪一个窗口、也不会去抢 WebView2 的用户数据目录。
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // 第二实例带文件参数过来：存 state + 发事件，前端立刻换片。
            if let Some(path) = open_file::video_from_args(argv) {
                open_file::stash(app, path);
            }
            // 用户双击文件后必须有反应：把已有窗口捞到前台，别只是换个片却停在托盘后面。
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.unminimize();
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        .manage(open_file::PendingOpen::default())
        .invoke_handler(tauri::generate_handler![
            app_version,
            open_file::take_open_file,
            fs_stat,
            pair_danmaku,
            scan_folder,
            read_text,
            read_bytes,
            scan_subtitles,
            probe_mkv_tracks,
            transcode::transcode_target,
            transcode::transcode_audio_start,
            transcode::extract_subtitle_text,
            transcode::subtitle_to_srt_text,
            transcode::transcode_audio_cancel,
            transcode::transcode_cancel_all,
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
        .manage(transcode::JobState::default())
        .manage::<pgs_render::PgsState>(std::sync::Mutex::new(None))
        .setup(|app| {
            // 窗口标题带上版本号。不写进 tauri.conf.json 是因为那里发版时要手改，
            // 而版本号的唯一真相是 Cargo.toml —— 从这里取永远不会对不上。
            // 前端改 document.title 不会同步到窗口标题，所以只能在后端设。
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.set_title(&format!(
                    "Danmuku Video Player v{} · 本地视频 + 弹幕播放器",
                    env!("CARGO_PKG_VERSION")
                ));
            }
            // 首次启动：如果是从「打开方式」/文件关联拉起来的，文件路径就在自己的 argv 里。
            // 这时 webview 还没加载完，emit 必然丢，所以只落 state，等前端初始化完
            // 主动 invoke('take_open_file') 取走（第二实例走的是 stash + 事件那条路）。
            if let Some(path) = open_file::video_from_args(std::env::args()) {
                let state = app.state::<open_file::PendingOpen>();
                *state.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // 关窗即杀所有后台子进程（音轨槽 + 字幕槽），不让 ffmpeg 变孤儿
            if let tauri::WindowEvent::Destroyed = event {
                window.app_handle().state::<transcode::JobState>().cancel_all();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试一个独立目录：cargo test 默认多线程并行，共用目录会互相踩。
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bplayer-lib-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, b"x").unwrap();
        p
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn fs_stat_impl_labels_file_dir_and_missing() {
        let dir = scratch("stat");
        let file = touch(&dir, "a.mkv");
        let st = fs_stat_impl(&s(&file));
        assert_eq!((st.kind.as_str(), st.size), ("file", 1));
        let st = fs_stat_impl(&s(&dir));
        assert_eq!((st.kind.as_str(), st.size), ("dir", 0));
        let st = fs_stat_impl(&s(&dir.join("nope.mkv")));
        assert_eq!((st.kind.as_str(), st.size), ("missing", 0));
        let _ = fs::remove_dir_all(&dir);
    }

    /// 回归：外挂字幕只认同基名（含 .语言后缀 变体），跳过 .tmp 与本应用的缓存产物。
    #[test]
    fn scan_subtitles_impl_matches_base_and_skips_our_products() {
        let dir = scratch("subs");
        let video = touch(&dir, "Movie.2009.mkv");
        for n in [
            "Movie.2009.srt",         // 同名，排最前
            "Movie.2009.chs.ass",     // 语言后缀变体
            "movie.2009.CHS.vtt",     // 大小写不同也是同一部片
            "Movie.2009.tmp.srt",     // 半成品
            "Movie.2009 (字幕1).srt",  // 本应用缓存产物
            "Movie.2009 (AAC).srt",   // 转码缓存标记
            "Movie.2010.srt",         // 另一部片
            "Movie.2009.nfo",         // 不是字幕
        ] {
            touch(&dir, n);
        }
        let got: Vec<String> = scan_subtitles_impl(&s(&video)).into_iter().map(|f| f.name).collect();
        assert_eq!(got, vec!["Movie.2009.srt", "Movie.2009.chs.ass", "movie.2009.CHS.vtt"]);

        // 路径必须指向真实文件，前端拿它去读内容
        for f in scan_subtitles_impl(&s(&video)) {
            assert!(Path::new(&f.path).is_file(), "路径不可读：{}", f.path);
        }

        let alone = scratch("subs-none");
        touch(&alone, "Other.mkv");
        assert!(scan_subtitles_impl(&s(&alone.join("Other.mkv"))).is_empty());
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&alone);
    }

    /// 回归：播放列表只列视频、跳过 (AAC) 缓存、按名排序、配同名弹幕（xml 优先）。
    #[test]
    fn scan_folder_impl_skips_cache_sorts_and_pairs_danmaku() {
        let dir = scratch("folder");
        for n in [
            "B.MKV",           // 扩展名大写也算视频
            "A.mp4",
            "A (AAC).mkv",     // 本应用转码产物，不能重复出现在列表里
            "A (AAC).tmp.mkv", // 转码半成品
            "A (音轨1).mka",    // 音频产物，不是视频
            "notes.txt",       // 无关文件
            "a.xml",           // A 的弹幕：xml 优先于 a.json
            "a.json",
            "B.JSON",          // 基名忽略大小写
            "C.mkv",           // 没有弹幕
        ] {
            touch(&dir, n);
        }
        let scan = scan_folder_impl(&s(&dir));
        let names: Vec<&str> = scan.episodes.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["A.mp4", "B.MKV", "C.mkv"], "应按名排序且不含缓存产物");

        assert_eq!(scan.episodes[0].danmaku.as_deref(), Some("a.xml"));
        assert_eq!(scan.episodes[1].danmaku.as_deref(), Some("B.JSON"));
        assert_eq!(scan.episodes[2].danmaku, None);
        assert_eq!(scan.episodes[0].size, 1, "size 来自 entry.metadata()");
        assert_eq!(scan.label, dir.file_name().unwrap().to_string_lossy());
        let _ = fs::remove_dir_all(&dir);
    }

    /// 缺陷 A5 回归：播放列表必须按自然排序（人类排序）。
    /// 修复前 `scan_folder_impl` 是纯小写字典序，于是「第10集」排在「第1集」前面，
    /// 用户点开文件夹看到的集数顺序是乱的。
    #[test]
    fn scan_folder_impl_sorts_episodes_naturally() {
        let dir = scratch("folder-natural");
        for n in ["第10集.mp4", "第2集.mp4", "第1集.mp4", "第11集.mp4"] {
            touch(&dir, n);
        }
        fs::write(dir.join("第2集.xml"), b"<i/>").unwrap();
        let scan = scan_folder_impl(&s(&dir));
        let names: Vec<&str> = scan.episodes.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["第1集.mp4", "第2集.mp4", "第10集.mp4", "第11集.mp4"],
            "集数应按数值排序"
        );
        // 排序后弹幕配对不能跟着错位
        assert_eq!(scan.episodes[1].danmaku.as_deref(), Some("第2集.xml"));
        assert_eq!(scan.episodes[0].danmaku, None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A5 单测辅助：按 natural_cmp 排序后取回名字。
    fn natural_sorted(items: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        v.sort_by(|a, b| natural_cmp(a, b));
        v
    }

    /// A5 单测：集数按数值排，不能被纯字典序带偏。
    #[test]
    fn natural_cmp_orders_episode_numbers() {
        assert_eq!(
            natural_sorted(&["第10集.mp4", "第2集.mp4", "第1集.mp4"]),
            vec!["第1集.mp4", "第2集.mp4", "第10集.mp4"]
        );
        // 纯字典序的反例：第10 会压在第1 前面，这里顺带确认期望值不是旧行为
        let mut lex: Vec<&str> = vec!["第10集.mp4", "第2集.mp4", "第1集.mp4"];
        lex.sort_by_key(|s| s.to_lowercase());
        assert_eq!(lex[0], "第10集.mp4", "旧字典序反例变了，说明算例失效");
    }

    /// A5 单测：大小写不敏感，a 排在 B 前（旧实现 to_lowercase 后正好也这样，
    /// 但这条防的是将来谁把 non-digit 段改成大小写敏感比较）。
    #[test]
    fn natural_cmp_is_case_insensitive() {
        assert_eq!(natural_sorted(&["B.mp4", "a.mp4"]), vec!["a.mp4", "B.mp4"]);
        assert_eq!(natural_cmp("A.mp4", "a.MP4"), Ordering::Equal);
    }

    /// A5 单测：数字段与非数字段交替时，逐段比较。
    /// 期望：ep2b < ep02a（数值同为 2，前导零少的在前，再比 a/b）< ep10。
    #[test]
    fn natural_cmp_handles_mixed_digit_and_text_runs() {
        assert_eq!(
            natural_sorted(&["ep02a.mp4", "ep2b.mp4", "ep10.mp4"]),
            vec!["ep2b.mp4", "ep02a.mp4", "ep10.mp4"]
        );
    }

    /// A5 单测：前导零数值相同，排序必须确定、可重复且不 panic。
    #[test]
    fn natural_cmp_handles_leading_zeros_deterministically() {
        let once = natural_sorted(&["01.mp4", "1.mp4", "001.mp4"]);
        assert_eq!(once, vec!["1.mp4", "01.mp4", "001.mp4"]);
        // 任意输入顺序都应得到同一结果（等值部分有显式 tie-break）
        for order in [
            ["001.mp4", "01.mp4", "1.mp4"],
            ["01.mp4", "001.mp4", "1.mp4"],
            ["1.mp4", "001.mp4", "01.mp4"],
        ] {
            assert_eq!(natural_sorted(&order), once, "输入顺序 {order:?} 结果不稳定");
        }
        // 超长数字段不能靠整数解析（这里 30 位，u64 会溢出）
        assert_eq!(natural_cmp("a1.mp4", "a0000000000000000000000000001.mp4"), Ordering::Less);
    }

    /// A5 单测：纯中文无数字文件名仍然稳定有序，不能乱。
    #[test]
    fn natural_cmp_keeps_plain_chinese_names_stable() {
        assert_eq!(
            natural_sorted(&["附录.mp4", "反派 (下).mp4", "反派 (上).mp4"]),
            vec!["反派 (上).mp4", "反派 (下).mp4", "附录.mp4"]
        );
        // 完全相等 → 稳定排序保持输入顺序（不 panic、不产生随机序）
        assert_eq!(natural_sorted(&["同名.mp4", "同名.mp4"]), vec!["同名.mp4", "同名.mp4"]);
    }

    /// 回归：xml 优先于 json，基名忽略大小写，配不上返回 None（前端据此不显示弹幕）。
    #[test]
    fn pair_danmaku_impl_prefers_xml_and_is_case_insensitive() {
        let dir = scratch("danmaku");
        let video = touch(&dir, "A.mkv");
        fs::write(dir.join("a.json"), br#"{"n":1}"#).unwrap();
        fs::write(dir.join("A.xml"), b"<i/>").unwrap();
        let got = pair_danmaku_impl(&s(&video)).expect("应该配上 A.xml");
        assert_eq!((got.name.as_str(), got.text.as_str()), ("A.xml", "<i/>"));
        assert!(pair_danmaku_impl(&s(&dir.join("B.mkv"))).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_text_and_bytes_impl_read_file_and_report_errors() {
        let dir = scratch("read");
        let p = touch(&dir, "a.txt");
        fs::write(&p, b"abcde").unwrap();
        assert_eq!(read_text_impl(&s(&p)).unwrap(), "abcde");
        // base64 常量核对，防止哪天把编码器改坏
        assert_eq!(read_bytes_impl(&s(&p)).unwrap(), "YWJjZGU=");
        assert!(read_text_impl(&s(&dir.join("nope.txt"))).is_err());
        assert!(read_bytes_impl(&s(&dir.join("nope.txt"))).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// 上限边界：恰好等于上限要放行（防"正好 64 MB 的弹幕被判超大"），多一个字节才拒。
    /// 用注入的 limit 测，不真造 64 MB 文件；另外单独测"读流时按实际字节数判定"这一层，
    /// 因为 metadata 先验看的是文件声明大小，够不到这条路径。
    #[test]
    fn read_limit_boundary() {
        assert!(!too_large(MAX_READ_BYTES - 1, MAX_READ_BYTES));
        assert!(!too_large(MAX_READ_BYTES, MAX_READ_BYTES));
        assert!(too_large(MAX_READ_BYTES + 1, MAX_READ_BYTES));

        // 读流判定：limit+1 字节就必须失败，且错误文案能被前端 toast 直接显示
        let err = read_stream_capped(std::io::Cursor::new(vec![7u8; 5]), 4).unwrap_err();
        assert!(err.contains("上限"), "错误文案应说明超限：{err}");
        assert_eq!(read_stream_capped(std::io::Cursor::new(vec![7u8; 4]), 4).unwrap().len(), 4);

        // 文件层：metadata 先验与整读都给同一个界限
        let dir = scratch("cap");
        let p = dir.join("a.txt");
        fs::write(&p, b"abcd").unwrap();
        assert_eq!(read_capped(&s(&p), 4).unwrap(), b"abcd");
        assert!(read_capped(&s(&p), 3).is_err(), "4 字节文件在 3 字节上限下必须拒");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 真造一个"声明 64 MB + 1 字节"的稀疏文件（set_len 不占磁盘、不占内存），
    /// 覆盖用户实际踩到的路径：同目录一个超大损坏 .xml。
    #[test]
    fn oversized_file_is_rejected_and_does_not_break_danmaku_pairing() {
        let dir = scratch("oversize");
        let big = dir.join("A.xml");
        fs::File::create(&big).unwrap().set_len(MAX_READ_BYTES + 1).unwrap();
        let video = touch(&dir, "A.mkv");

        let text = read_text_impl(&s(&big)).unwrap_err();
        assert!(text.contains("上限"), "read_text 超限要给前端可读的 Err：{text}");
        let bytes = read_bytes_impl(&s(&big)).unwrap_err();
        assert!(bytes.contains("上限"), "read_bytes 超限要给前端可读的 Err：{bytes}");

        // 弹幕超限不是错误：当作这部片没有弹幕，开片流程继续
        assert!(pair_danmaku_impl(&s(&video)).is_none(), "超大弹幕必须静默当作没有");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 回归：只列本片自己的产物，别的片与半成品不列，结果有序。
    #[test]
    fn sidecar_cache_files_impl_only_lists_own_products() {
        let dir = scratch("sidecar");
        let video = touch(&dir, "反派 (上).mkv");
        for n in [
            "反派 (上) (音轨1).mka",
            "反派 (上) (音轨2).mka",
            "反派 (上) (字幕1).srt",
            "反派 (上) (音轨1).tmp.mka", // 半成品
            "反派 (上) (备注).mkv",      // 名字套娃但不是产物
            "反派 (下) (音轨1).mka",     // 别的片
        ] {
            touch(&dir, n);
        }
        let got = sidecar_cache_files_impl(&s(&video));
        let names: Vec<String> = got
            .iter()
            .map(|p| Path::new(p).file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["反派 (上) (字幕1).srt", "反派 (上) (音轨1).mka", "反派 (上) (音轨2).mka"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 薄壳也要真的能跑：每个 async 命令都是"进线程池 + 保持不变语义"，
    /// 只测 impl 会漏掉薄壳接错函数的低级错误。
    #[test]
    fn async_shells_keep_sync_semantics() {
        let dir = scratch("shell");
        let file = touch(&dir, "a.mkv");
        fs::write(&file, b"hi").unwrap();

        let st = tauri::async_runtime::block_on(fs_stat(s(&file)));
        assert_eq!((st.kind.as_str(), st.size), ("file", 2));

        let st = tauri::async_runtime::block_on(fs_stat(s(&dir.join("nope.mkv"))));
        assert_eq!(st.kind, "missing");

        let text = tauri::async_runtime::block_on(read_text(s(&file))).unwrap();
        assert_eq!(text, "hi");
        assert!(tauri::async_runtime::block_on(read_text(s(&dir.join("nope.txt")))).is_err());

        let subs = tauri::async_runtime::block_on(scan_subtitles(s(&file)));
        assert!(subs.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}