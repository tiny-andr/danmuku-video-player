// PGS（蓝光位图字幕）本地渲染：用 libpgs 从 MKV 里按 Cues 索引抽取显示集，解出位图
// 合成为 PNG 事件表。不落盘、不 OCR —— 只是把"Chromium 内核不认的图形字幕"还原成图片。
//
// 关键实测（潘神 UHD 35.6 GB，简体轨）：抽取 1669 个显示集只读 13.4 MB（文件的
// 0.038%），冷启动 7.3 s / 缓存热 50 ms；合成 884 张裁边 PNG 约 2.5 s，合计约 30 MB。
// 之所以能只读千分之零点四，是 libpgs 读 Cues 索引直接 seek 到含字幕块的位置，
// 不像 mkvextract/ffmpeg 那样顺序扫全片（那个要读 33 GB、139 秒）。
//
// 事件切分：显示集里出现 PCS 对象且"构图签名"（对象 id/版本/坐标 + 调色板）变化，
// 就算一条新事件；事件在下一次清屏显示集处结束。纯调色板更新（palette_only）
// 不产生新事件，只是重绘已有事件。

use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use libpgs::pgs::payload::ods_rle_data;
use libpgs::pgs::rle::decode_rle;
use libpgs::pgs::{CompositionState, PaletteEntry};
use tauri::{AppHandle, Manager, State};

/// 一条字幕事件：时间区间 + 位图在 PGS 画布上的位置尺寸（已裁掉透明边）。
#[derive(Serialize, Clone)]
pub struct PgsEvent {
    pub start: f64, // ms
    pub end: f64,   // ms
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// 交给前端的索引：画布尺寸 + 事件表（不含图像，图像按需取）。
#[derive(Serialize, Clone)]
pub struct PgsIndex {
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub events: Vec<PgsEvent>,
}

pub struct PgsDoc {
    video: String,
    track: u32,
    index: PgsIndex,
    images: Vec<Vec<u8>>, // 与 events 同序的 PNG 字节
}

pub type PgsState = Mutex<Option<PgsDoc>>;

// ---- 请求代际 ----

/// PGS 请求代际号，只增不减。前端连点两条 PGS 轨时，先发出的那次 build 可能更慢：
/// 如果它一完成就无条件写 state，会把后发者已经写好的文档盖回去，`pgs_image` 随后
/// 按事件序号取到的是另一条轨的图（或图数对不上而静默无图）。前端的 `pgsToken`
/// 只挡 promise 回调，挡不住 Rust 侧状态被旧文档覆盖，所以这里必须自己认号。
static PGS_GEN: AtomicU64 = AtomicU64::new(0);

fn next_pgs_gen() -> u64 {
    PGS_GEN.fetch_add(1, Ordering::SeqCst) + 1
}

// ---- 资源上限 ----

/// 画布边长硬顶。蓝光最大 1920×1080，UHD 蓝光 3840×2160，8192 留足余量。
/// 尺寸是文件里的外部数据，不设限时损坏/恶意文件声明 65535×65535 会让
/// `Canvas::new` 申请约 17 GB（65535²×4），分配失败直接 abort 整个播放器。
const PGS_MAX_DIM: u32 = 8192;

/// 单轨事件数上限。每条事件都常驻一张 PNG，条数失控同样会吃光内存。
const PGS_MAX_EVENTS: usize = 200_000;

/// 位图总量上限。只限条数是不够的 —— 200000 条 × 30 KB 也能到 6 GB。
/// 真片实测约 884 条、30 MB，128 MB 对正常片源绰绰有余。
const PGS_MAX_IMAGE_BYTES: usize = 128 * 1024 * 1024;

/// 单个 epoch 内解码后的对象总量上限。对象尺寸已限 8192²（单对象 64 MB），
/// 但 id 空间有 65536 个，坏文件能堆出天文数字；真片几百 KB 到几 MB。
const PGS_MAX_OBJECT_BYTES: usize = 128 * 1024 * 1024;

fn check_canvas_size(w: u32, h: u32) -> Result<(), String> {
    if w == 0 || h == 0 || w > PGS_MAX_DIM || h > PGS_MAX_DIM {
        return Err(format!("PGS 画布尺寸异常（{w}×{h}），拒绝加载"));
    }
    Ok(())
}

/// ODS 对象尺寸也是文件声明的，`decode_rle` 会按 w*h 预分配像素缓冲，超限同样是
/// 一次 abort。0 表示空对象，解码器自己会返回空切片，照旧放行。
fn object_size_reasonable(w: u16, h: u16) -> bool {
    w <= PGS_MAX_DIM as u16 && h <= PGS_MAX_DIM as u16
}

// ---- 位图合成 ----

fn ycbcr_to_rgb(y: u8, cr: u8, cb: u8) -> (u8, u8, u8) {
    let yf = y as f32;
    let crf = cr as f32 - 128.0;
    let cbf = cb as f32 - 128.0;
    let clamp = |v: f32| v.clamp(0.0, 255.0) as u8;
    (
        clamp(yf + 1.402 * crf),
        clamp(yf - 0.344_136 * cbf - 0.714_136 * crf),
        clamp(yf + 1.772 * cbf),
    )
}

struct Canvas {
    w: usize,
    h: usize,
    rgba: Vec<u8>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Canvas { w, h, rgba: vec![0; w * h * 4] }
    }

    fn draw(&mut self, x: usize, y: usize, bw: usize, bh: usize, idx: &[u8], pal: &HashMap<u8, PaletteEntry>) {
        for row in 0..bh {
            let cy = y + row;
            if cy >= self.h {
                break;
            }
            for col in 0..bw {
                let cx = x + col;
                if cx >= self.w {
                    break;
                }
                let Some(e) = pal.get(&idx[row * bw + col]) else { continue };
                if e.alpha == 0 {
                    continue;
                }
                let (r, g, b) = ycbcr_to_rgb(e.luminance, e.cr, e.cb);
                let o = (cy * self.w + cx) * 4;
                self.rgba[o] = r;
                self.rgba[o + 1] = g;
                self.rgba[o + 2] = b;
                self.rgba[o + 3] = e.alpha;
            }
        }
    }

    /// 透明像素的最小外接矩形（x, y, w, h）；全透明返回 None。
    fn ink_box(&self) -> Option<(u32, u32, u32, u32)> {
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
        for y in 0..self.h {
            let row = y * self.w;
            for x in 0..self.w {
                if self.rgba[(row + x) * 4 + 3] != 0 {
                    if x < x0 {
                        x0 = x;
                    }
                    if y < y0 {
                        y0 = y;
                    }
                    if x > x1 {
                        x1 = x;
                    }
                    if y > y1 {
                        y1 = y;
                    }
                }
            }
        }
        if x0 == usize::MAX {
            return None;
        }
        Some((x0 as u32, y0 as u32, (x1 - x0 + 1) as u32, (y1 - y0 + 1) as u32))
    }

    fn crop_png(&self, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>, String> {
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        for row in 0..h as usize {
            let src = ((y as usize + row) * self.w + x as usize) * 4;
            let dst = row * (w as usize) * 4;
            out[dst..dst + (w as usize) * 4].copy_from_slice(&self.rgba[src..src + (w as usize) * 4]);
        }
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut buf, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().map_err(|e| e.to_string())?;
            writer.write_image_data(&out).map_err(|e| e.to_string())?;
        }
        Ok(buf)
    }
}

/// 构图签名：对象集合（id/版本/位置）+ 调色板版本决定这一帧画面是否变了。
fn signature(pcs: &libpgs::pgs::PcsData, palette_ver: u8) -> Vec<(u16, u8, u16, u16)> {
    let mut v: Vec<(u16, u8, u16, u16)> = pcs
        .objects
        .iter()
        .map(|o| (o.object_id, 0u8, o.x, o.y))
        .collect();
    v.push((u16::MAX, palette_ver, pcs.palette_id as u16, 0));
    v
}

// ---- 抽取 + 合成 ----

fn build(video: &str, track: u32) -> Result<PgsDoc, String> {
    let path = std::path::Path::new(video);
    if !path.is_file() {
        return Err("源视频不存在".into());
    }
    let sets = libpgs::extract_display_sets(path, Some(track)).map_err(|e| format!("抽取 PGS 失败: {e}"))?;
    if sets.is_empty() {
        return Err("这条字幕轨里没有可渲染的位图".into());
    }

    let mut palette: HashMap<u8, PaletteEntry> = HashMap::new();
    let mut palette_ver: u8 = 0;
    let mut objects: HashMap<u16, (usize, usize, Vec<u8>)> = HashMap::new();
    let mut objects_ver: HashMap<u16, u8> = HashMap::new();
    let mut events: Vec<PgsEvent> = Vec::new();
    let mut images: Vec<Vec<u8>> = Vec::new();
    let (mut canvas_w, mut canvas_h) = (0u32, 0u32);
    let mut current: Option<Vec<(u16, u8, u16, u16)>> = None;
    // 累计已吃下的内存，超预算就整轨放弃（见上面两个常量）
    let mut object_bytes: usize = 0;
    let mut image_bytes: usize = 0;

    for ds in &sets {
        if ds.composition_state == CompositionState::EpochStart {
            objects.clear();
            objects_ver.clear();
            palette.clear();
            object_bytes = 0; // 新 epoch 重新开始计数，旧对象已释放
        }
        for seg in &ds.segments {
            if let Some(pds) = seg.parse_pds() {
                palette_ver = pds.version;
                for e in pds.entries {
                    palette.insert(e.id, e);
                }
            }
        }
        // 组装可能被切成多段的 ODS
        let mut ids: Vec<u16> = Vec::new();
        let mut groups: HashMap<u16, Vec<&libpgs::pgs::PgsSegment>> = HashMap::new();
        for seg in &ds.segments {
            if let Some(ods) = seg.parse_ods() {
                if ods.width.is_some() && ods.height.is_some() && !groups.contains_key(&ods.id) {
                    ids.push(ods.id);
                }
                groups.entry(ods.id).or_default().push(seg);
            }
        }
        for id in ids {
            let group = &groups[&id];
            let Some(first) = group[0].parse_ods() else { continue };
            let (Some(w), Some(h)) = (first.width, first.height) else { continue };
            if !object_size_reasonable(w, h) {
                return Err(format!("PGS 对象尺寸异常（{w}×{h}），拒绝加载"));
            }
            let mut rle: Vec<u8> = Vec::new();
            for seg in group {
                if let Some(ods) = seg.parse_ods() {
                    if let Some(d) = ods_rle_data(&seg.payload, ods.sequence) {
                        rle.extend_from_slice(d);
                    }
                }
            }
            if let Some(pixels) = decode_rle(&rle, w, h) {
                object_bytes = object_bytes.saturating_add(pixels.len());
                if object_bytes > PGS_MAX_OBJECT_BYTES {
                    return Err(format!(
                        "PGS 对象总量超过 {} MB 上限，拒绝加载",
                        PGS_MAX_OBJECT_BYTES / (1024 * 1024)
                    ));
                }
                objects_ver.insert(id, first.version);
                objects.insert(id, (w as usize, h as usize, pixels));
            }
        }

        let Some(pcs) = ds.segments.iter().find_map(|s| s.parse_pcs()) else { continue };
        // 先校验再分配：画布是照文件声明申请的，别让坏文件把进程撑爆
        check_canvas_size(pcs.video_width as u32, pcs.video_height as u32)?;
        canvas_w = pcs.video_width as u32;
        canvas_h = pcs.video_height as u32;

        if pcs.objects.is_empty() {
            // 清屏：收尾当前事件
            if let Some(mut sig) = current.take() {
                sig.clear();
                if let Some(last) = events.last_mut() {
                    last.end = ds.pts_ms;
                }
            }
            continue;
        }

        // 把对象版本合进签名，避免同一对象内容换了坐标没变时漏判
        let mut sig = signature(&pcs, palette_ver);
        for entry in sig.iter_mut() {
            if entry.0 != u16::MAX {
                if let Some(v) = objects_ver.get(&entry.0) {
                    entry.1 = *v;
                }
            }
        }
        if current.as_ref() == Some(&sig) {
            continue; // 同一帧，只是刷新
        }

        let mut canvas = Canvas::new(canvas_w as usize, canvas_h as usize);
        let mut any = false;
        for obj in &pcs.objects {
            if let Some((w, h, px)) = objects.get(&obj.object_id) {
                canvas.draw(obj.x as usize, obj.y as usize, *w, *h, px, &palette);
                any = true;
            }
        }
        let Some((bx, by, bw, bh)) = (if any { canvas.ink_box() } else { None }) else {
            continue;
        };

        let png = canvas.crop_png(bx, by, bw, bh)?;
        if let Some(last) = events.last_mut() {
            if last.end <= last.start {
                last.end = ds.pts_ms; // 上一帧结束在这里
            }
        }
        if events.len() >= PGS_MAX_EVENTS {
            return Err(format!("PGS 事件数超过上限（{PGS_MAX_EVENTS}），拒绝继续合成"));
        }
        image_bytes = image_bytes.saturating_add(png.len());
        if image_bytes > PGS_MAX_IMAGE_BYTES {
            return Err(format!(
                "PGS 位图总量超过 {} MB 上限，拒绝继续合成",
                PGS_MAX_IMAGE_BYTES / (1024 * 1024)
            ));
        }
        events.push(PgsEvent { start: ds.pts_ms, end: ds.pts_ms + 2000.0, x: bx, y: by, w: bw, h: bh });
        images.push(png);
        current = Some(sig);
    }

    // 最后一条事件没有明确的结束时间：按常见字幕时长兜底
    if let Some(last) = events.last_mut() {
        if last.end > last.start + 30_000.0 {
            last.end = last.start + 8000.0;
        }
    }
    if events.is_empty() {
        return Err("这条字幕轨里没有可渲染的位图".into());
    }

    Ok(PgsDoc { video: video.to_string(), track, index: PgsIndex { canvas_w, canvas_h, events }, images })
}

// ---- 命令 ----

/// 打开某条 PGS 字幕轨：后台抽取并合成，返回事件索引（图像按需再取）。
#[tauri::command]
pub async fn pgs_open(app: AppHandle, video_path: String, track: u32) -> Result<PgsIndex, String> {
    // 号在 build 之前就取好：之后只要再来一次 open 或 close，号就比这个大了
    let my_gen = next_pgs_gen();
    let app2 = app.clone();
    let doc = tauri::async_runtime::spawn_blocking(move || build(&video_path, track))
        .await
        .map_err(|e| format!("PGS 任务失败: {e}"))??;
    let index = doc.index.clone();
    // 比对和写入必须在同一把锁里：否则「比对通过 → 另一个 open 取号 → 本单写入」
    // 这个交错照样会把新文档盖掉
    {
        let state = app2.state::<PgsState>();
        let mut guard = state.lock().unwrap();
        if PGS_GEN.load(Ordering::SeqCst) != my_gen {
            return Err("PGS 解析已被更新的请求取代".into());
        }
        *guard = Some(doc);
    }
    Ok(index)
}

/// 取某条事件的 PNG（base64）。前端自己做 LRU，只有当前时间附近几条会被请求。
#[tauri::command]
pub fn pgs_image(state: State<'_, PgsState>, index: usize) -> Result<String, String> {
    let guard = state.lock().unwrap();
    let doc = guard.as_ref().ok_or("没有加载 PGS 字幕")?;
    let bytes = doc.images.get(index).ok_or("事件序号越界")?;
    Ok(base64_encode(bytes))
}

/// 释放当前 PGS 文档（切片/关闭字幕时调用，30 MB 内存不常驻）
#[tauri::command]
pub fn pgs_close(state: State<'_, PgsState>) {
    // 先推高代际号，让还在 build 的 open 完成后自己作废；再比对一次是为了防止
    // 「close 取号之后又来了一次新 open」时把新 open 刚写好的文档关掉
    let my_gen = next_pgs_gen();
    let mut guard = state.lock().unwrap();
    if PGS_GEN.load(Ordering::SeqCst) != my_gen {
        return;
    }
    *guard = None;
}

/// 当前加载的是哪条轨（换片判断用）
#[tauri::command]
pub fn pgs_current(state: State<'_, PgsState>) -> Option<(String, u32)> {
    let guard = state.lock().unwrap();
    guard.as_ref().map(|d| (d.video.clone(), d.track))
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_size_accepts_real_and_boundary_sizes() {
        for (w, h) in [(1u32, 1u32), (1920, 1080), (3840, 2160), (8192, 8192)] {
            assert!(check_canvas_size(w, h).is_ok(), "{w}×{h} 应被接受");
        }
    }

    #[test]
    fn canvas_size_rejects_zero_and_absurd_sizes() {
        for (w, h) in [
            (0u32, 0u32),
            (0, 1080),
            (1920, 0),
            (65535, 65535),
            (8193, 100),
            (100, 8193),
        ] {
            assert!(check_canvas_size(w, h).is_err(), "{w}×{h} 应被拒绝");
        }
    }

    #[test]
    fn object_size_guard_allows_empty_but_rejects_oversized() {
        assert!(object_size_reasonable(0, 0));
        assert!(object_size_reasonable(1920, 1080));
        assert!(object_size_reasonable(8192, 8192));
        assert!(!object_size_reasonable(8193, 100));
        assert!(!object_size_reasonable(65535, 65535));
    }

    #[test]
    fn pgs_generation_only_increases() {
        let a = next_pgs_gen();
        let b = next_pgs_gen();
        assert!(b > a, "代际号必须单调递增：{a} -> {b}");
    }
}
