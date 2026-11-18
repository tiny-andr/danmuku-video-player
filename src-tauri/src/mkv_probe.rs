// MKV/WebM 轨道探测：直接解析 EBML 的 Tracks 段（纯 std，零新增依赖，不依赖 ffprobe）。
// 目的：开播前发现 Chromium 内核解不了的音轨编码（DTS / TrueHD / LPCM / AC-4…），
// 把"视频照播、音频静默消失"变成明确的提示。
//
// EBML 要点：元素 ID 与尺寸都是变长整数，首字节的前导 0 数即总字节数；
// ID 保留标记位，尺寸去掉标记位；尺寸位全 1 表示未知（流式写法，本探测直接放弃）。
// Tracks 正常位于音视频数据（Cluster）之前，所以只读文件头部，大文件也是毫秒级。

use serde::Serialize;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};

#[derive(Serialize)]
pub struct MkvTrack {
    pub kind: String,  // "video" | "audio" | "subtitle" | "other"
    pub codec: String, // CodecID：A_AAC / A_DTS / V_MPEGH/ISO/HEVC …
    pub lang: String,  // LanguageIETF 优先，回退旧 Language 字段
    pub name: String,  // 轨道标题，可能为空
    pub number: u32,   // Matroska TrackNumber（PGS 位图渲染按它定位轨道）
    pub default: bool,
}

#[derive(Serialize)]
pub struct MkvProbe {
    pub tracks: Vec<MkvTrack>,
    pub duration_us: Option<u64>, // 全片时长（微秒），转码进度条用；拿不到为 None
}

const ID_EBML: u32 = 0x1A45_DFA3;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_INFO: u32 = 0x1549_A966;
const ID_TIMESTAMP_SCALE: u32 = 0x2AD7_B1;
const ID_DURATION: u32 = 0x4489;
const ID_TRACKS: u32 = 0x1654_AE6B;
const ID_TRACK_ENTRY: u32 = 0xAE;
const ID_TRACK_NUMBER: u32 = 0xD7;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_FLAG_DEFAULT: u32 = 0x88;
const ID_CODEC_ID: u32 = 0x86;
const ID_LANGUAGE: u32 = 0x22B5_9C;
const ID_LANG_IETF: u32 = 0x22B5_9D;
const ID_NAME: u32 = 0x536E;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_CUES: u32 = 0x1C53_BB6B;

/// 单层 EBML 元素扫描预算（顶层 / Segment 内各一份）。
///
/// 正常 MKV 的顶层只有 EBML 头 + 一个 Segment（偶尔几个 Void）；Segment 里在第一个
/// Cluster/Cues 之前也只有 SeekHead / Info / Tracks / Void 这几个。4096 远大于正常
/// 文件所需，预算用完还没走到数据段，只可能是结构异常（或伪造了 EBML magic 的其它
/// 文件）—— 这时继续逐元素扫到 EOF 会把 shared 的 blocking 线程占住几分钟。
const MAX_SCAN_ELEMENTS: usize = 4096;

/// 入口：先快拒非 EBML 文件，再定位 Segment → 找 Tracks → 解析全部 TrackEntry。
pub fn probe(path: &str) -> Result<MkvProbe, String> {
    let f = File::open(path).map_err(|e| e.to_string())?;
    let mut r = BufReader::new(f);

    // 快拒：真正的 MKV/WebM 第一个元素就是 EBML 头，文件前 4 字节必然是 magic。
    // 没有这一条，一个几 GB 的假 .mkv（扩展名对、内容不是）会被下面逐元素扫到
    // EOF，秒到分钟级占死 blocking 线程池里的一个线程，把之后所有命令一起堵住。
    let mut magic = [0u8; 4];
    if r.read_exact(&mut magic).is_err() || magic != [0x1A, 0x45, 0xDF, 0xA3] {
        return Err("不是 EBML 容器（MKV/WebM）".into());
    }
    r.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;

    let mut budget = MAX_SCAN_ELEMENTS;
    loop {
        if budget == 0 {
            return Err("MKV 顶层元素过多，文件结构异常".into());
        }
        budget -= 1;
        match next_element(&mut r).map_err(|e| e.to_string())? {
            None => return Err("不是 EBML 容器（MKV/WebM）".into()),
            Some((ID_EBML, size, unk)) => skip(&mut r, size, unk)?,
            Some((ID_SEGMENT, _, _)) => return scan_segment(&mut r),
            Some((_, size, unk)) => skip(&mut r, size, unk)?, // 容忍前导 Void 等杂项
        }
    }
}

// ---- 元素流 ----

type El = (u32, u64, bool); // (ID, 尺寸, 是否未知尺寸)

fn next_element(r: &mut BufReader<File>) -> std::io::Result<Option<El>> {
    let id = match read_id(r)? {
        None => return Ok(None),
        Some(v) => v,
    };
    let (size, unknown) = read_size(r)?;
    Ok(Some((id, size, unknown)))
}

fn read_id(r: &mut BufReader<File>) -> std::io::Result<Option<u32>> {
    let first = match read_u8(r)? {
        None => return Ok(None),
        Some(b) => b,
    };
    let len = first.leading_zeros() as usize + 1;
    if len > 4 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "元素 ID 长度非法"));
    }
    let mut id = first as u32;
    for _ in 1..len {
        id = (id << 8) | read_u8(r)?.ok_or_else(eof)? as u32;
    }
    Ok(Some(id))
}

/// 尺寸 vint，返回 (值, 是否未知尺寸)。
fn read_size(r: &mut BufReader<File>) -> std::io::Result<(u64, bool)> {
    let first = read_u8(r)?.ok_or_else(eof)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "尺寸 vint 长度非法"));
    }
    let mut val = first as u64;
    for _ in 1..len {
        val = (val << 8) | read_u8(r)?.ok_or_else(eof)? as u64;
    }
    let marker = 1u64 << (8 * len - len);
    let unknown = val & (marker - 1) == marker - 1;
    Ok((val & !marker, unknown))
}

fn read_u8(r: &mut BufReader<File>) -> std::io::Result<Option<u8>> {
    let mut b = [0u8; 1];
    match r.read(&mut b)? {
        0 => Ok(None),
        _ => Ok(Some(b[0])),
    }
}

fn eof() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "EBML 数据提前结束")
}

fn skip(r: &mut BufReader<File>, size: u64, unknown: bool) -> Result<(), String> {
    if unknown {
        return Err("遇到未知尺寸的元素（流式写法），放弃探测".into());
    }
    if size > i64::MAX as u64 {
        return Err("元素尺寸异常".into());
    }
    r.seek(SeekFrom::Current(size as i64)).map_err(|e| e.to_string())?;
    Ok(())
}

// ---- 段级解析 ----

fn scan_segment(r: &mut BufReader<File>) -> Result<MkvProbe, String> {
    let mut tracks = Vec::new();
    let mut duration_us: Option<u64> = None;
    // 段内扫描预算：见 MAX_SCAN_ELEMENTS。正常片在第一个 Cluster/Cues 之前就
    // break 了，远远用不到这么多。
    let mut budget = MAX_SCAN_ELEMENTS;
    loop {
        if budget == 0 {
            return Err("MKV 段内元素过多，文件结构异常".into());
        }
        budget -= 1;
        match next_element(r).map_err(|e| e.to_string())? {
            None => break,
            Some((ID_TRACKS, size, unk)) => parse_tracks_into(r, size, unk, &mut tracks)?,
            Some((ID_INFO, size, unk)) => {
                if duration_us.is_none() {
                    duration_us = parse_info_duration(r, size, unk)?;
                } else {
                    skip(r, size, unk)?;
                }
            }
            // Tracks/Info 正常都排在音视频数据之前；碰到数据段就收工
            Some((ID_CLUSTER | ID_CUES, _, _)) => break,
            Some((_, size, unk)) => skip(r, size, unk)?,
        }
    }
    if tracks.is_empty() {
        return Err("未找到 Tracks 段".into());
    }
    Ok(MkvProbe { tracks, duration_us })
}

/// 解析 Info 段里的 TimestampScale(0x2AD7B1) + Duration(0x4489)，换算成微秒。
fn parse_info_duration(r: &mut BufReader<File>, size: u64, unknown: bool) -> Result<Option<u64>, String> {
    if unknown {
        return Ok(None);
    }
    let start = r.stream_position().map_err(|e| e.to_string())?;
    let end = start.saturating_add(size);
    let mut scale: u64 = 1_000_000; // 规范缺省 1ms
    let mut value: Option<f64> = None;
    while r.stream_position().map_err(|e| e.to_string())? < end {
        match next_element(r).map_err(|e| e.to_string())? {
            None => break,
            Some((ID_TIMESTAMP_SCALE, size, false)) => scale = read_uint(r, size)?,
            Some((ID_DURATION, size, false)) => value = Some(read_float_or_uint(r, size)?),
            Some((_, size, unk)) => skip(r, size, unk)?,
        }
    }
    // duration_ns = Duration 值 × TimestampScale
    Ok(value.map(|v| ((v * scale as f64) / 1000.0) as u64))
}

/// Matroska 的 Duration 按规范是 float（4/8 字节），旧 muxer 有写成 uint 的，两者都收。
fn read_float_or_uint(r: &mut BufReader<File>, size: u64) -> Result<f64, String> {
    match size {
        4 => Ok(f32::from_be_bytes(read_n(r, 4)?.try_into().unwrap()) as f64),
        8 => Ok(f64::from_be_bytes(read_n(r, 8)?.try_into().unwrap())),
        _ => Ok(read_uint(r, size)? as f64),
    }
}

fn parse_tracks_into(r: &mut BufReader<File>, size: u64, unknown: bool, tracks: &mut Vec<MkvTrack>) -> Result<(), String> {
    if unknown {
        return Err("Tracks 段为未知尺寸（流式写法），放弃探测".into());
    }
    let start = r.stream_position().map_err(|e| e.to_string())?;
    let end = start.saturating_add(size);
    while r.stream_position().map_err(|e| e.to_string())? < end {
        match next_element(r).map_err(|e| e.to_string())? {
            None => break,
            Some((ID_TRACK_ENTRY, size, false)) => {
                // TrackNumber 越界的畸形轨会在 parse_track_entry 里被丢掉，只跳过
                // 这一条，不让整部片（其余正常轨）跟着失败。
                if let Some(track) = parse_track_entry(r, size)? {
                    tracks.push(track);
                }
            }
            Some((_, size, unk)) => skip(r, size, unk)?,
        }
    }
    Ok(())
}

/// 解析一条 TrackEntry。返回 `Ok(None)` 表示这条轨的字段不可信、整条跳过
/// （目前只有 TrackNumber 超出 u32 这一种：截断后会与别的轨撞号）。
fn parse_track_entry(r: &mut BufReader<File>, size: u64) -> Result<Option<MkvTrack>, String> {
    let start = r.stream_position().map_err(|e| e.to_string())?;
    let end = start.saturating_add(size);
    let mut kind = String::from("other");
    let mut codec = String::new();
    let mut lang_legacy = String::new();
    let mut lang_ietf = String::new();
    let mut name = String::new();
    let mut number = 0u32; // TrackNumber：PGS 位图渲染按它定位轨道（libpgs 用同一个号）
    // TrackNumber 规范是 u32，但 read_uint 返回 u64：畸形文件写 2^32+1 时直接
    // `as u32` 会静默截断成 1，和真正的 1 号轨撞号 —— 下游 Cues 索引和 PGS 渲染
    // 都按 number 定位，等于抽到另一条轨（音轨/字幕全错，还查不出来）。
    // 撞号无法修复（没有第二个号可用），所以整条轨丢弃并记日志；但不能整个文件
    // Err，否则一条坏轨会让整部正常片子连音轨列表都拿不到。
    let mut bad_number: Option<u64> = None;
    let mut default = true; // Matroska 规范中 FlagDefault 缺省为 1
    while r.stream_position().map_err(|e| e.to_string())? < end {
        match next_element(r).map_err(|e| e.to_string())? {
            None => break,
            Some((ID_TRACK_TYPE, size, false)) => {
                kind = match read_uint(r, size)? {
                    1 => "video".into(),
                    2 => "audio".into(),
                    17 => "subtitle".into(),
                    _ => "other".into(),
                };
            }
            Some((ID_FLAG_DEFAULT, size, false)) => default = read_uint(r, size)? != 0,
            Some((ID_TRACK_NUMBER, size, false)) => {
                let raw = read_uint(r, size)?;
                match u32::try_from(raw) {
                    Ok(n) => number = n,
                    Err(_) => bad_number = Some(raw),
                }
            }
            Some((ID_CODEC_ID, size, false)) => codec = read_string(r, size)?,
            Some((ID_LANGUAGE, size, false)) => lang_legacy = read_string(r, size)?,
            Some((ID_LANG_IETF, size, false)) => lang_ietf = read_string(r, size)?,
            Some((ID_NAME, size, false)) => name = read_string(r, size)?,
            Some((_, size, unk)) => skip(r, size, unk)?,
        }
    }
    if let Some(raw) = bad_number {
        eprintln!(
            "[mkv_probe] 跳过一条轨道：TrackNumber={raw} 超出 u32 范围，截断后会与其它轨撞号"
        );
        return Ok(None);
    }
    Ok(Some(MkvTrack {
        kind,
        codec,
        lang: if lang_ietf.is_empty() { lang_legacy } else { lang_ietf },
        name,
        number,
        default,
    }))
}

fn read_uint(r: &mut BufReader<File>, size: u64) -> Result<u64, String> {
    if size > 8 {
        return Err("整数字段尺寸异常".into());
    }
    let mut val = 0u64;
    for b in read_n(r, size as usize)? {
        val = (val << 8) | b as u64;
    }
    Ok(val)
}

fn read_string(r: &mut BufReader<File>, size: u64) -> Result<String, String> {
    if size > (1 << 20) {
        return Err("字符串字段尺寸异常".into());
    }
    let s = String::from_utf8_lossy(&read_n(r, size as usize)?).into_owned();
    Ok(s.trim_end_matches('\0').trim().to_string())
}

fn read_n(r: &mut BufReader<File>, n: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 合成样本测试（不依赖外部素材） ----

    fn id_bytes(id: u32) -> Vec<u8> {
        let be = id.to_be_bytes();
        let start = be.iter().position(|&b| b != 0).unwrap_or(3);
        be[start..].to_vec()
    }

    /// 最短形式尺寸 vint（含标记位）。
    fn vint_size(n: u64) -> Vec<u8> {
        let mut len = 1usize;
        while n > (1u64 << (7 * len)) - 1 {
            len += 1;
        }
        let full = n | (1u64 << (7 * len));
        (0..len)
            .rev()
            .map(|i| ((full >> (8 * i)) & 0xFF) as u8)
            .collect()
    }

    fn el(id: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = id_bytes(id);
        out.extend(vint_size(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn sample_mkv() -> Vec<u8> {
        let t1 = [
            el(ID_TRACK_TYPE, &[2]),
            el(ID_CODEC_ID, b"A_DTS"),
            el(ID_LANGUAGE, b"eng"),
            el(ID_FLAG_DEFAULT, &[1]),
        ]
        .concat();
        let t2 = [el(ID_TRACK_TYPE, &[1]), el(ID_CODEC_ID, b"V_MPEGH/ISO/HEVC")].concat();
        let t3 = [
            el(ID_TRACK_TYPE, &[2]),
            el(ID_CODEC_ID, b"A_AAC"),
            el(ID_LANG_IETF, b"zh"),
            el(ID_NAME, "国语音轨".as_bytes()),
            el(ID_FLAG_DEFAULT, &[0]),
        ]
        .concat();
        let tracks = [el(ID_TRACK_ENTRY, &t1), el(ID_TRACK_ENTRY, &t2), el(ID_TRACK_ENTRY, &t3)].concat();
        let info = [
            el(ID_TIMESTAMP_SCALE, &[0x0F, 0x42, 0x40]), // 1_000_000
            el(ID_DURATION, &60000.0f64.to_be_bytes()),  // 60000 ms → 60s
        ]
        .concat();
        let mut segment = Vec::new();
        segment.extend(el(ID_INFO, &info));
        segment.extend(el(ID_TRACKS, &tracks));
        [el(ID_EBML, &[0x42, 0x86, 0x81, 0x01]), el(ID_SEGMENT, &segment)].concat()
    }

    fn write_sample(name: &str, bytes: &[u8]) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn parse_synthetic_mkv() {
        let path = write_sample("test-sample.mkv", &sample_mkv());
        let probe = probe(&path).unwrap();
        assert_eq!(probe.duration_us, Some(60_000_000));
        let audios: Vec<_> = probe.tracks.iter().filter(|t| t.kind == "audio").collect();
        assert_eq!(probe.tracks.len(), 3);
        assert_eq!(audios.len(), 2);
        assert_eq!(audios[0].codec, "A_DTS");
        assert_eq!(audios[0].lang, "eng");
        assert!(audios[0].default);
        assert_eq!(audios[1].codec, "A_AAC");
        assert_eq!(audios[1].lang, "zh");
        assert_eq!(audios[1].name, "国语音轨");
        assert!(!audios[1].default);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_non_mkv() {
        let path = write_sample("test-notmkv.bin", b"RIFFxxxx not an ebml file");
        assert!(probe(&path).is_err());
        std::fs::remove_file(&path).ok();
    }

    /// 扫描预算：文件开头是合法 EBML magic，但后面堆满 Void、永远没有 Segment。
    /// 这类伪造文件必须被预算拒掉，而不是一路扫到 EOF 占死 blocking 线程。
    #[test]
    fn rejects_top_level_scan_budget_exhaustion() {
        let mut bytes = el(ID_EBML, &[0x42, 0x86, 0x81, 0x01]);
        for _ in 0..(MAX_SCAN_ELEMENTS + 1) {
            bytes.extend(el(0xEC, &[])); // Void（ID 0xEC，空 payload）
        }
        let path = write_sample("test-scan-budget.mkv", &bytes);
        // 用 err() 而不是 unwrap_err()：MkvProbe 没有 derive Debug，unwrap_err 要 T: Debug。
        let err = probe(&path).err().expect("应命中扫描预算而不是成功返回");
        assert!(err.contains("结构异常"), "应命中扫描预算而不是走到 EOF：{err}");
        std::fs::remove_file(&path).ok();
    }

    /// TrackNumber 越界：只丢这一条轨，其余轨照常返回（不能整文件 Err）。
    /// 2^32+1 用 `as u32` 会静默截断成 1，跟真正的 1 号轨撞号。
    #[test]
    fn skips_track_with_out_of_range_number() {
        let good = [
            el(ID_TRACK_NUMBER, &[1]),
            el(ID_TRACK_TYPE, &[2]),
            el(ID_CODEC_ID, b"A_AAC"),
        ]
        .concat();
        let bad = [
            el(ID_TRACK_NUMBER, &[0x01, 0x00, 0x00, 0x00, 0x01]), // 2^32 + 1
            el(ID_TRACK_TYPE, &[2]),
            el(ID_CODEC_ID, b"A_DTS"),
        ]
        .concat();
        let tracks = [el(ID_TRACK_ENTRY, &good), el(ID_TRACK_ENTRY, &bad)].concat();
        let segment = el(ID_TRACKS, &tracks);
        let bytes = [el(ID_EBML, &[0x42, 0x86, 0x81, 0x01]), el(ID_SEGMENT, &segment)].concat();

        let path = write_sample("test-bad-tracknumber.mkv", &bytes);
        let probe = probe(&path).unwrap();
        assert_eq!(probe.tracks.len(), 1, "越界 TrackNumber 的轨必须被跳过");
        assert_eq!(probe.tracks[0].number, 1);
        assert_eq!(probe.tracks[0].codec, "A_AAC");
        std::fs::remove_file(&path).ok();
    }

    /// 真机素材冒烟测试：设 MKV_TEST_FILE=<mkv 路径> 后才跑。
    #[test]
    fn probe_real_file() {
        let p = match std::env::var("MKV_TEST_FILE") {
            Ok(v) if !v.is_empty() => v,
            _ => return,
        };
        let probe = probe(&p).unwrap_or_else(|e| panic!("probe 失败: {e}"));
        for t in &probe.tracks {
            println!("[{}] {} lang={} name={} default={}", t.kind, t.codec, t.lang, t.name, t.default);
        }
        assert!(probe.tracks.iter().any(|t| t.kind == "audio"));
    }
}
