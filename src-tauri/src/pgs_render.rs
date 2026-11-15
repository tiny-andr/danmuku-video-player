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

    for ds in &sets {
        if ds.composition_state == CompositionState::EpochStart {
            objects.clear();
            objects_ver.clear();
            palette.clear();
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
            let mut rle: Vec<u8> = Vec::new();
            for seg in group {
                if let Some(ods) = seg.parse_ods() {
                    if let Some(d) = ods_rle_data(&seg.payload, ods.sequence) {
                        rle.extend_from_slice(d);
                    }
                }
            }
            if let Some(pixels) = decode_rle(&rle, w, h) {
                objects_ver.insert(id, first.version);
                objects.insert(id, (w as usize, h as usize, pixels));
            }
        }

        let Some(pcs) = ds.segments.iter().find_map(|s| s.parse_pcs()) else { continue };
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
    let app2 = app.clone();
    let doc = tauri::async_runtime::spawn_blocking(move || build(&video_path, track))
        .await
        .map_err(|e| format!("PGS 任务失败: {e}"))??;
    let index = doc.index.clone();
    *app2.state::<PgsState>().lock().unwrap() = Some(doc);
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
    *state.lock().unwrap() = None;
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
