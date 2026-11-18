# AGENTS.md

> **改完代码、验证通过后立刻维护本文件（AGENTS.md）——不要等提交。** 新增模块写进 §3「模块实现」，新踩的坑写进 §5「硬性约束」，行为有变就同步改已有描述。提交前再复核一遍，确保描述与最终代码一致。

给在这个仓库里改代码的 AI / 人类协作者。只写容易踩错、又不好从代码里一眼看出来的东西。

## 1. 这是什么

Tauri 2 桌面播放器：打开本机视频，自动配同目录的弹幕 XML 与字幕一起播。

- 前端**没有构建步骤**：`src/index.html` 是唯一入口，CSS 与 JS 全部内联，`src/vendor/` 下的 Artplayer 与两个插件是本地化副本，不走 CDN。
- 后端是 Rust，9 个模块，多数命令是"调外部 ffmpeg / 读二进制 / 开系统对话框"的薄封装。

## 2. 代码地图

| 文件 | 职责 |
|---|---|
| `src/index.html` (~3700 行) | 整个前端。播放器装配、弹幕、字幕菜单 + 字幕样式/位置/延时、音轨菜单、画面适配、停止播放、断点续播、设置页、拖放、进度胶囊 |
| `src/vendor/artplayer.js` | 播放器内核，本地副本，**不要改** |
| `src/vendor/artplayer-plugin-danmuku.js` | 弹幕插件，本地副本，只在必要时打补丁（`.orig.js` 保留原版） |
| `src-tauri/src/lib.rs` | 命令注册、选文件/选文件夹、目录扫描、`reveal_path`、`sidecar_cache_files` |
| `src-tauri/src/mkv_probe.rs` | 解析 MKV 轨道表。产出 `number`（Matroska TrackNumber，**不是** 0 基索引） |
| `src-tauri/src/mkv_sub.rs` | 用 Cues 索引稀疏抽取文本字幕（核心性能点） |
| `src-tauri/src/pgs_render.rs` | libpgs 解码 PGS 位图 → base64 PNG |
| `src-tauri/src/subtitle_text.rs` | 外挂字幕（srt/vtt/ass/ssa）纯 Rust → SRT，**不依赖 ffmpeg** |
| `src-tauri/src/blocking.rs` | `blocking()`：把阻塞 IO 丢进 spawn_blocking 线程池，命令薄壳统一用它 |
| `src-tauri/src/open_file.rs` | 从命令行参数/文件关联打开视频：argv 解析、待打开文件 state、`take_open_file` 命令、`bplayer://open-file` 事件 |
| `src-tauri/src/transcode.rs` | 音轨转码、字幕抽取兜底、audio/subtitle 双任务槽、Job Object |
| `src-tauri/src/audio_stream.rs` | 流式音轨：ffmpeg 从 seek 点编 fMP4 写管道，按 fragment 切块交给前端 MSE（pull 模式） |
| `src-tauri/src/autostart.rs` | 注册表 `Run` 项自启动 |

## 3. 模块实现

逐个模块记录：它负责什么、对外暴露什么、内部怎么流转、失败时是什么语义。**「为什么这么写」在 §5 硬性约束里**，这里只写实现事实。

### 3.1 `blocking.rs` — 命令的线程模型

Tauri 2 的所有 `invoke` **共用一条串行通道**。同步命令不会冻住窗口消息循环（窗口 Responding 一直为 true），但一条 3 秒的 SMB `fs_stat` 会把之后所有命令一起堵到 3 秒才返回。这个模块就是那层把阻塞段挪出通道的薄壳。

```rust
pub(crate) async fn blocking<T, F>(f: F) -> Result<T, String>
where F: FnOnce() -> T + Send + 'static, T: Send + 'static
```

- 内部就是 `tauri::async_runtime::spawn_blocking(f).await`，失败（panic / 运行时关闭）映射成 `"后台任务异常退出: {e}"`。
- 返回 `Result` 而不是 `T`：调用方通常 `unwrap_or` / `unwrap_or_else` 兜一个安全值，保持命令原本「永不 reject」的语义。
- **标准形态**：命令声明成 `async fn`，函数体只有一行 `blocking(move || xxx_impl(...)).await?`；`xxx_impl` 是同步函数、不碰任何 Tauri 类型、返回值直接给前端 —— 这样纯逻辑可以单测，前端调用点零改动。
- 例外（不需要套壳）：`pick_video` / `pick_folder`（rfd 模态必须主线程）、`pgs_image` / `pgs_close` / `pgs_current`（纯内存）、`autostart` 系列（注册表毫秒级）、`transcode_target`（纯路径计算）。

### 3.2 `lib.rs` — 命令注册与文件系统

模块声明的 9 个 `mod`、`VIDEO_EXTS`（11 个扩展名：mp4/m4v/webm/mov/mkv/avi/ts/flv/wmv/mpg/mpeg）、`run()` 里的插件与 `setup` 都在这里。文件系统类命令：

| 命令 | 返回 | 说明 |
|---|---|---|
| `fs_stat(path)` | `FsStat { kind, size }` | `kind` 三态 `file` / `dir` / `missing`，**不存在不报错** |
| `scan_folder(dir)` | `FolderScan` | 剧集列表（视频 + 是否配到同名弹幕） |
| `scan_subtitles(video_path)` | `Vec<SubFile>` | 同目录外挂字幕候选 |
| `pair_danmaku(video_path)` | `Option<DanmakuPayload>` | 同名 `.xml` / `.json` 配对，**xml 优先、忽略大小写** |
| `read_text(path)` / `read_bytes(path)` | `Result<String, String>` | 整读，走 `read_capped`（64 MB 上限） |
| `probe_mkv_tracks(path)` | `Result<MkvProbe, String>` | 转 `mkv_probe::probe` |
| `sidecar_cache_files(video_path)` | `Vec<String>` | 列同目录已有的 `(音轨N).mka` / `(字幕N).srt` |
| `reveal_path(path)` | `Result<(), String>` | 资源管理器定位文件 |
| `app_version()` | `String` | 读 `CARGO_PKG_VERSION` |

- `VIDEO_EXTS` / `is_video()` / `base_name()` 是**唯一一份**扩展名与基名规则，目录扫描、打开对话框、文件关联三处共用。
- `read_capped(path, MAX_READ_BYTES)` 是**所有整读路径的强制入口**（弹幕 / 文本 / 字幕 / ffmpeg stdout），超限即 Err。弹幕路径把 Err 当「没有弹幕」静默处理，文本/字幕路径把 Err 交给 toast。
- `run()` 顺序：**`.plugin(tauri_plugin_single_instance)` 必须第一个** → 其余插件 → `setup`（解析 bundled ffmpeg 路径存进 `transcode::BUNDLED_FFMPEG`、把 argv 里的视频存进 `PendingOpen`）→ `.invoke_handler(generate_handler![...])`。前端命令清单以 `generate_handler!` 为准。

### 3.3 `mkv_probe.rs` — MKV 轨道表

纯 std 解析 EBML，**不用 ffprobe**（安装包不带它）。目的是开播前就知道哪条音轨内核解不了，把「视频照播、音频静默消失」变成明确提示。

```
probe(path) -> Result<MkvProbe, String>
  loop next_element():
    ID_EBML   -> skip
    ID_SEGMENT-> scan_segment()
    其他       -> skip（容忍前导 Void）
  非 EBML  -> Err("不是 EBML 容器（MKV/WebM）")
```

- `MkvProbe { tracks: Vec<MkvTrack>, duration_us: Option<u64> }`；`MkvTrack { kind, codec, lang, name, number, default }`。
- `kind` 四态：`video` / `audio` / `subtitle` / `other`（TrackType 1/2/17，其余归 other）。
- `codec` 是 CodecID 原样字符串（`A_AAC` / `A_DTS` / `V_MPEGH/ISO/HEVC`）—— 前端的可解码判定直接比对它。
- **`number` 是 Matroska TrackNumber，不是 0 基索引**。Cues 索引和 PGS 渲染都按它定位；前端拿到的「第几条轨」是另一个概念，两者不能混用。
- `duration_us` 来自 `Info/Duration × TimestampScale`，转码进度条用；读不到就是 `None`，调用方必须容忍。
- 只读文件头：Tracks 在 Cluster 之前，大文件也是毫秒级。
- **入口先校验 EBML magic**（`1A 45 DF A3`）：以前一个几 GB 的假 `.mkv` 会逐元素扫到 EOF，秒到分钟级占死一个共享 blocking 线程。顶层与 `scan_segment` 内层各有一道 `MAX_SCAN_ELEMENTS = 4096` 预算。
- `TrackNumber` 用 `u32::try_from` 转换，超范围**只丢该轨**（`eprintln!` 记录），不整文件 Err —— 截断成 1 会和真正的 1 号轨撞号，而 Cues / PGS 都按 number 定位，撞号必然抽错轨。

### 3.4 `mkv_sub.rs` — 内嵌文本字幕稀疏抽取

用 Cues 索引按字节位置跳读，只碰字幕块，不扫全片。实测 Free Guy（27.95 GB / 1902 条 subrip）：ffmpeg 顺序抽 195.6 s → 稀疏抽 0.19 s，产物逐字节相同。

```rust
pub fn sparse_extract_srt(path: &Path, track_number: u32, is_ass: bool) -> Result<String, String>
pub fn strip_markup(srt: &str) -> String      // 被 subtitle_text 复用
```

流程：

1. `locate_cues()` — 读前 2 MiB → 找 `Segment` → 走 `SeekHead` 找到 `Cues` 绝对位置，同时取 `TimestampScale`。**读不到 TimestampScale 就 Err**（按 1e6 猜会让整条时间轴按比例错位）。
2. `read_cues()` — 一次读整个 Cues 元素，解析 `CuePoint` 的 `CueTime` / `CueTrack` / `CueClusterPosition` / `CueRelativePosition`，按目标 `track_number` 过滤。
3. 逐 cue：seek 到簇起点 → 读簇头（96 B）→ 取 `ClusterTimestamp` → `block_off = 簇payload起点 + CueRelativePosition` → 读块 → `parse_block()`。
4. `parse_block()` 返回 `(track, rel_ts, duration, payload)`；`start_ms = cluster_ts + rel_ts`。
5. 文本化：SRT 原样保留行；ASS 走 `ass_line_to_text()`（`Dialogue:` 前缀跳 9 段，否则跳 8 段，再 `ass_clean_text()` 去 `{\...}` 覆盖标签、处理 `\N` / `\n` / `\h`）。
6. 排序、按 `duration`（BlockDuration）或下一条 `start_ms` 算结束时间、拼 SRT。

**失败语义是整轨 fail-closed**：任何一条 cue 解析不出（缺 `CueRelativePosition`、block 带 lacing、cluster 头异常、时间戳溢出、ASS 行格式不对）→ 返回 `Err("稀疏解析失败 n/m 条，整轨拒绝")`，调用方回退 ffmpeg 顺序抽。

**`read_cues` 也在这条防线上**：目标轨的 `CueTrackPositions` 缺 `CueClusterPosition`、或缺 `CueTrack`（Matroska 必需元素）= 文件损坏，直接 `Err`。**不要退回「`if let Some(c) = cluster`，没有就静默跳过」** —— 那样丢掉的 cue 既不计入 `dropped` 分子也不计入 `cues.len()` 分母，最终产出一份缺行却看起来正常的 SRT，而这条链路没有任何自动修复途径（2026-11-18 评审抓到的 P0 破口，`synthetic_mkv_missing_cluster_position_is_fail_closed` 钉住）。`CueTrack` 存在但**不是**目标轨的必须继续正常跳过 —— 那是别的轨，不是缺陷。

单测里有一条**不依赖任何片源**的端到端：内存里拼真 EBML（EBML 头 + Segment{Info + SeekHead + Cues + Cluster}）写临时文件，真跑全链路并逐字节比对 SRT / ASS 输出。

### 3.5 `subtitle_text.rs` — 外挂字幕解析

`srt` / `vtt` / `ass` / `ssa` 全是纯文本容器，「转换」只是重排时间轴、丢样式，不需要解码器。自己实现，ffmpeg 只留给认不出来的格式兜底。

```rust
pub fn to_srt(path: &Path) -> Result<String, String>
pub fn decode_text(bytes: &[u8]) -> String
```

- `to_srt()`：`read_capped`（64 MB）→ `decode_text()` → 按扩展名分派 `normalize_srt` / `vtt_to_srt` / `ass_to_srt` → `strip_markup()` → 空结果 Err。
- 复用 `mkv_sub` 的 `strip_markup` / `ass_clean_text` / `fmt_ts` —— 内嵌和外挂的 ASS 清洗规则是同一份。
- `decode_text()` 是**编码探测链**，顺序不能改：`BOM 优先` → `无 BOM 的 UTF-16 启发式` → `严格 UTF-8` → `GBK（MultiByteToWideChar 936）` → `有损 UTF-8`。
- 为什么 BOM 必须排第一：UTF-16LE 的中文（如「你」= `60 4F`）整段字节流**恰好是合法 UTF-8**，先试严格 UTF-8 会"成功"并返回 `` `O}Y `` 这类垃圾，用户看到乱码而连 GBK 回退都不会触发。
- 用 `windows-sys` 的系统 API 解码 GBK，不引第三方编码库（`windows-sys` 已是依赖）。

### 3.6 `pgs_render.rs` — PGS 位图字幕

蓝光位图字幕 Chromium 不认，这里把它还原成图片叠在画面上。不落盘、不 OCR。

```
pgs_open(video_path, track)  -> PgsIndex        (async + blocking，几秒)
pgs_image(index)             -> Result<String>  (base64 PNG，纯内存)
pgs_close()                  -> ()
pgs_current()                -> Option<(String, u32)>   当前文档的 (视频, 轨道号)
```

- 数据流：`libpgs` 按 Cues 抽取显示集 → `decode_rle()` 解位图 → 合成到画布 → 裁掉透明边 → `png` 编码 → `Vec<Vec<u8>>` 全部存进 `PgsDoc`，`PgsIndex`（画布尺寸 + 事件表）交给前端，图像按需 `pgs_image(index)` 取。
- 实测（潘神 UHD 35.6 GB / 1669 个显示集）：只读 **13.4 MB**（文件的 0.038%），冷启动 7.3 s、缓存热 50 ms；合成 884 张裁边 PNG 约 2.5 s，合计约 30 MB 内存。
- 实测 2（`Pan's Labyrinth` UHD 33.14 GB，5 条 PGS 轨）：Eng 轨 887 事件 / Chs 轨 884 事件，`pgs_open` 冷启动 18.5 s / 14.3 s；两条轨 **零 INFINITY、零负间隔、零重叠**，并存在精确衔接（上一帧 `end` == 下一帧 `start`），时长 p50≈2.1–2.5 s、最长 6.1–8.2 s。这是 `end` 哨兵化收尾的真机证据 —— 旧的固定 `+2000` 会同时造出"字幕提前消失的空隙"和"与下一帧重叠"。
- **事件切分**：显示集里出现 PCS 对象且「构图签名」（对象 id/版本/坐标 + 调色板）变化 = 新事件；下一次清屏显示集结束它。纯调色板更新（`palette_only`）**不产生新事件**，只重绘已有事件。
- **`end` 用哨兵 `f64::INFINITY`，三条路径各自收尾**（2026-11-18 评审修掉的真缺陷）：
  - 路径1 **清屏**：清屏显示集的 `pts` 就是上一条的 `end`；
  - 路径2 **新帧替换旧帧**（签名变了但没清屏，卡拉OK逐字/多行替换）：push 新事件前，若上一条 `end` 仍是 `INFINITY` 才用新帧 `pts` 收尾；
  - 路径3 **循环结束**：仍残留 `INFINITY` 的按 `start + 8000` 兜底，保证事件表里**没有非有限值**（serde 会把 ±INF 序列化成 `null`，前端 `t < null` 恒假 → 那条字幕永远不显示）。
  **不要退回「push 时 end = start + 2000」**：默认值让"已收尾/未收尾"无法区分，才导致原来的收尾判断恒假（死代码）、以及片尾合法长事件被 `> start+30000` 截成 8 秒（字幕提前消失半分钟）。
- **两处预算的边界要说清**：`PGS_MAX_OBJECT_BYTES` / `PGS_MAX_IMAGE_BYTES` 管的是合成阶段（RLE 解码后的像素 + PNG 编码产物）；`PGS_MAX_SOURCE_BYTES = 256 MiB` 管的是 `extract_display_sets` 返回后、进合成循环前的源数据总量。**但抽取本身没有预算**：`libpgs::extract_display_sets` 是一次性 API，调用即全量驻留，`PGS_MAX_SOURCE_BYTES` 只能避免之后继续放大（对象解码 + PNG），挡不住抽取阶段的峰值。根治要换流式抽取（`libpgs` 目前没有）。
- `PgsEvent { start, end, x, y, w, h }` 的时间单位是**毫秒**，坐标是**裁边后**位图在 PGS 画布上的位置；前端按 `canvas_w/h` 和可见画面框等比摆放。
- **单例 state**：`PgsState = Mutex<Option<PgsDoc>>`，同一时刻只存在一份文档。`pgs_open` 里用 `PGS_GEN: AtomicU64` 认号 —— 前端连点两条 PGS 轨时，先发出的那次 build 可能更慢，一完成就无条件写 state 会把后发者已经写好的文档盖回去，`pgs_image` 随后按序号取到另一条轨的图。`pgs_current` 就是给前端判断"现在挂的是不是我要的那条"用的。
- 锁中毒不能跟着 panic（持锁方只做整体替换 Option，不存在半个状态），统一 `unwrap_or_else(|e| e.into_inner())`。

### 3.7 `transcode.rs` — 音轨转码 + 字幕兜底 + 双任务槽

后台媒体处理，一个模块管两条互不干扰的流水线：音轨 → `(音轨N).mka`（落视频同目录），内嵌文本字幕 → SRT（只进内存，不落盘）。

**子进程包装**（全模块统一）：

```rust
pub(crate) fn hidden_command(program) -> Command   // Windows CREATE_NO_WINDOW，不弹黑框
pub fn assign(child: &Child) -> bool               // 绑进带 KILL_ON_JOB_CLOSE 的 Job Object
```

Job Object 是 `OnceLock` 单例：父进程无论怎么死（点关闭 / 崩溃 / 任务管理器强杀），内核都会连带杀掉 job 里的 ffmpeg。光靠 window 事件回调不够 —— 强杀时回调根本不执行，ffmpeg 会变孤儿继续占 IO。

**ffmpeg 定位** `find_ffmpeg()`：`BUNDLED_FFMPEG`（`OnceLock`，启动时 setup 从 `BaseDirectory::Resource` 解析）→ exe 同目录 → PATH。它是普通函数、拿不到 AppHandle，所以路径必须提前注入。

**任务模型**：

- `JobSlots { audio: Mutex<Option<TranscodeJob>>, subtitle: ... }` —— 两个独立槽，互不杀。
- `JOB_GEN: AtomicU64`，每个任务 `fetch_add(1)` 拿号；worker 完成时 `take_if_gen(|j| j.gen == gen)` 认领自己那单。号只增不减。
- 产物先写 `{stem} (音轨N).tmp.mka`，成功后原子重命名 —— 半成品永远不会被当成缓存命中。
- 进度/结果靠事件：`transcode://progress`（`{ kind, video, index, percent, written }`）、`done`、`error`、`cancelled`。
- kill 一律在锁外做（等子进程退出要几百毫秒，拿锁会把另一个槽的命令一起卡住）。
- **`done` 必须自带轨号**：`JobSpec.index` → `JobDone.index`。音轨填 0 基 `audio_index`，字幕填 0 基轨序号。前端**不能拿记账位当前值当事件轨号**（同片换轨时记账位已被覆盖，旧任务收尾会把 A 轨的 mka 用 B 轨的号挂上）。`done_payload_carries_zero_based_audio_index` 钉住这个契约。

**命令**：

| 命令 | 说明 |
|---|---|
| `transcode_audio_start(video, index, ...)` | 起音轨任务（async + blocking） |
| `transcode_audio_cancel()` | 只杀音轨槽 |
| `transcode_cancel_all()` | 两个槽都杀，返回杀掉的个数 |
| `transcode_target(video, index)` | 纯计算：mka 目标路径（不 IO，不用套壳） |
| `subtitle_to_srt_text(path)` | 外挂字幕 → SRT 文本 |
| `extract_subtitle_text(video, subs, ...)` | 内嵌字幕：先试 `mkv_sub` 稀疏抽，失败回退 ffmpeg 顺序抽 |
| `clean_sidecar_tmp(video)` | 只删 `is_our_sidecar_tmp` 认定的半成品 |

**命名与清理**：`mka_cache_path()` / `srt_cache_path()` 是唯一一份产物命名规则；`is_our_sidecar_tmp()` 只认 `{stem} (音轨N).tmp.mka` / `{stem} (字幕N).tmp.srt` 两种精确形态。`CACHE_TAG = " (AAC)"` 用于识别旧版整片缓存。

**分流规则**（谁走 copy、谁走转码）不在这个模块里定，由前端 `ensureAudioTrack` 按 `isDeadCodec` 决定，模块只执行 `copy: bool` 参数。见 §3.15。

### 3.8 `audio_stream.rs` — 流式音轨（C 级）

真解不了的音轨（DTS / TrueHD / DTS-HD）整条转完要 2~3 分钟，这里改成从 seek 点开始边转边喂 MSE，**init + 首块实测 60~70 ms 就绪**。

```
audio_stream_start(video, audioIndex, startSecs) -> StreamInfo { id, mime, start_secs }
audio_stream_read(id, maxBytes)                  -> Vec<u8>    // 长度 0 = 流结束
audio_stream_stop(id)                            -> bool
audio_stream_state(id)                           -> String     // 诊断用
```

- 一个 `StreamState(Mutex<Option<StreamHandle>>)` 单例，**同时只允许一路**；`shutdown(app)` 在窗口销毁时清掉。
- 内部：子进程 `ffmpeg -ss <start> -i <video> -map 0:a:N -ac 2 ... -f mp4 -movflags +frag_keyframe+empty_moov+default_base_moof -flush_packets 1 -` 写 stdout；读线程按 ISO-BMFF box 边界增量切块塞进 `VecDeque`，前端 `read` 就是从这个队列取。
- **`FragmentSplitter`**：第一块是 init（`ftyp` + `moov`），之后每块 `moof` + `mdat`（一个 1 秒 fragment）。`init_sent` 之后**只放行 `moof` / `mdat`**，`mfra` 一律丢弃。
- **pull 模型的实现**：`read` 命令用 `Condvar` 等数据；前端不来取 → 队列满 → 读线程不读 stdout → ffmpeg 写满管道自己阻塞。缓冲上限天然卡死。
- 常量：`MAX_BUFFERED_BYTES` 8 MB（约 5.5 分钟 192k）、`MAX_BOX_BYTES` 64 MB（防损坏数据吃穿内存）、`MAX_READ_BYTES` 4 MB（单次 IPC 上限）、`WAIT_TIMEOUT` 30 s（NAS 断连时 ffmpeg 可能长时间不吐数据）。
- `MIME_AAC = audio/mp4; codecs="mp4a.40.2"` —— 前端 `addSourceBuffer` 直接用 `info.mime`。
- **`read` 必须能立刻嗅到"已被取消"**：`stop` 会置 `cancelled` + `notify_all`，但等待循环若只判 `error/chunks/eof`，哨兵唤醒后会重新睡回去，等满 `WAIT_TIMEOUT` 30 s 才返回假错误。命中 `cancelled` 时返回**空 `Vec`**（长度 0 = 流结束），不要返 Err。
- **并发 `start` 要先杀旧 handle**：单例槽直接 `Some(handle)` 覆盖时，旧 `StreamHandle` 被 drop，但 worker 手里的 `Arc` 还在 ⇒ 子进程不会被杀；没人消费队列后它攒满 8 MB 卡在 `wait_for_room`、ffmpeg 卡在写满的管道，孤儿进程一直占 IO。做法是 `slot.replace(handle)` 取旧值、**在锁外** `kill_handle(old)`（对齐 `transcode::JobSlots::install`）。
- 前端配套的三个 MSE 坑（`duration = Infinity`、`timestampOffset` 必须先设、`QuotaExceededError`）见 §5。

### 3.9 `open_file.rs` — 命令行 / 文件关联打开

Windows 没有 macOS 的 `RunEvent::Opened`，用户右键「打开方式」或双击关联文件时，系统只把路径塞进 argv。

```rust
pub const OPEN_FILE_EVENT: &str = "bplayer://open-file";
pub fn video_from_args(args) -> Option<String>        // argv 解析（纯函数，可单测）
pub fn stash(app, path)                               // 存 state + emit 事件
take_open_file(state) -> Option<String>               // 取一次即清空
```

- `video_from_args`：跳过 `argv[0]` → 认 `--open <path>` 显式写法 → `-` / `--` 开头当开关跳过（Tauri / WebView2 会塞自己的参数）→ 第一个「扩展名在 `VIDEO_EXTS` 里**且 `path.is_file()`**」的参数胜出，后面忽略。要求文件真实存在，是因为用户可能把字幕/弹幕/任意文件关联过来。
- **两条入口分开处理**，因为前端就绪状态不同：
  - 首次启动：参数在自己 argv 里，但 webview 还没加载完 → `setup` 存进 `PendingOpen`，前端初始化完 `invoke('take_open_file')` 取。
  - 已有实例：`single-instance` 插件把第二个进程的 argv 用 `WM_COPYDATA` 转发过来 → 走 `stash` 存 state **再** emit 事件，界面立刻换片。emit 失败（前端还没监听）不算错误，state 里还有值，两条路互为兜底。

### 3.10 `autostart.rs` — 开机自启

```rust
get_autostart() -> bool
set_autostart(enable) -> Result<bool, String>
```

- 写 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，值名固定英文 `Danmuku Video Player`（中文键名在部分系统上编码异常），路径统一加引号。
- 用注册表而不是任务计划：删掉这个值就彻底停用，不需要额外组件。`exists()` 用 `RegGetValueW` 判断，`set(false)` 走 `RegDeleteKeyValueW`。
- 整模块 `#![cfg(windows)]`。

### 3.11 前端总览：单文件 IIFE 的结构

`src/index.html` 3705 行，**CSS 与 JS 全部内联、无构建步骤**、ES5 风格大 IIFE。内部按功能分 12 个区（行号为大致边界，改动时按注释找）：

| 行段 | 分区 |
|---|---|
| ~600–1500 | 状态与工具（`appSettings`、`$()`、`toast()`、`invoke` 包装）、播放器装配 `createPlayer` |
| 1492 | 画面适配 `applyFitMode` / `videoPictureBox` |
| 1511 | 字幕样式 / 位置 / 延时的纯函数部分（不碰 DOM，可离线单测） |
| 1644 | 停止播放（回到空态） |
| 1945 | 音轨编码探测 + 外挂音轨（audio-track 插件） |
| 2274 | 流式音轨（MSE） |
| 2607 | 音轨菜单 |
| 2666 | 字幕：外挂扫描 + 内嵌抽取 |
| 2965 | PGS 位图字幕 |
| 3343 | 停止播放（第二轮，含状态清理） |
| 3568 | 设置页 |
| 3673 | 字幕样式 / 延时面板 |
| 3802 | 从命令行 / 文件关联打开 |

主要运行时状态：

```js
var appSettings  = { autoAudio: true, autoSubtitle: true, fit: 'contain' };  // 齿轮页
var art          = null;      // Artplayer 实例
var danmakuList  = [];        // 播放器格式的弹幕数组
var subSourceText = '';       // 当前字幕的原始 SRT（改延时要在它上面重做位移）
var probeTracks  = null;      // 最近一次探测的音频轨列表（可能为 null，读取前必须判空）
var audioJob     = { video: '', ordinal: -1 };            // 音轨任务记账位（ordinal 1-based）
var subJob       = { video: '', index: -1, item: null };  // 内嵌字幕任务记账位
```

**作用域陷阱**：`createPlayer` 在 2 空格缩进，字幕/音频/PGS 那一大坨在 4 空格缩进的块里。块内 `function` 声明不外泄，跨层调用会 `ReferenceError: pgsPump is not defined`。要跨层用就挪到外层或挂到对象上。

### 3.12 播放器装配与文件打开

- 视频走 Tauri 官方 **asset 协议**（`http://asset.localhost/<编码后路径>`），原生支持 Range，不经过自定义命令。`art.video.crossOrigin = 'anonymous'` 必须在**第一次设 `src` 之前**（WebAudio 要抓音轨做隔离）。
- 打开单个文件：`pick_video` → `fs_stat`（判 `file` / `dir` / `missing`）→ `probe_mkv_tracks`（MKV 才有轨道表，其他容器返回 Err）→ `pair_danmaku` → `scan_subtitles` → 装配播放器 → 自动挂弹幕/字幕。
- 打开文件夹：`scan_folder` 产出剧集列表（`Episode { path, name, hasDanmaku }`），按自然序排列，双击切集走的是**同源换集**路径（不重建播放器）。
- 拖放：拖入视频 = 打开；拖入字幕 = 登记进字幕菜单并立即挂载（`addDroppedSubtitle`）；拖入弹幕 XML = 登记并应用。
- 换片必须先 `transcode_cancel_all()` + 清 `audioJob` / `subJob`，否则上一片的 worker 会把产物认领到新片上。

### 3.13 弹幕

- 源格式：B 站 XML（`<d p="时间,模式,字号,颜色,时间戳,池,用户,行号">文本</d>`）与 JSON（新版接口）。解析后统一成 `danmakuList`，交给 `artplayer-plugin-danmuku` 消费。
- 配对规则在 Rust `pair_danmaku`：视频同目录同名 `.xml` / `.json`，**xml 优先、忽略大小写**；读超 64 MB 视为「没有弹幕」静默处理。
- 插件通过 `mount()` 挂进播放器；密度与屏蔽词是两套独立配置（`DENSITY_VER` / `CONFIG_VER`），改动即存。
- `danmakuManual` 令牌：只有**非空的**手动选择才置位，否则自动配对永久失效。

### 3.14 字幕（文本 / 位图 / 样式 / 延时）

**候选来源三条**：同目录外挂（`scan_subtitles`，含语言后缀变体）、内嵌文本（`extract_subtitle_text`）、内嵌位图（`pgs_open`）。

**文本字幕路径**：

1. 外挂：`subtitle_to_srt_text(path)` → Rust 纯解析（不依赖 ffmpeg）。
2. 内嵌：`extract_subtitle_text(video, subs, durationUs)` → Rust 先试 `mkv_sub` 稀疏抽（毫秒级），失败回退 ffmpeg 顺序抽（分钟级）。
3. 拿到 SRT 文本后 `mountSubtitleText()`：原文留档到 `subSourceText` → 建 blob URL → `art.subtitle.switch()`。文本**只进内存**，不落盘。

**位图字幕路径**：`pgs_open(video, track)` 拿 `PgsIndex` → 建一个绝对定位的图片层 → 播放时按当前时间二分查事件 → `pgs_image(index)` 取 base64 PNG。图片数量多，取回后要缓存。

**自动挂载优先级**（`autoSubtitle` 开时）：外挂（精确同名 / 语言后缀变体 / 拖入）> 内嵌 `default` 轨 > 唯一一条内嵌文本轨 > 位图兜底 > 静默。一条候选都没有时**不弹任何提示**。

**样式**：字号 32/38/44（默认 32）/ 颜色 / 描边 / 位置走 CSS 变量（`--art-subtitle-font-size`、`--art-subtitle-bottom` 等），必须用 `setProperty`（Artplayer 的 `style()` 对自定义属性静默失效）。字号**只对文本字幕有效**：srt/vtt/ass/ssa 统一先转 SRT 再渲染，ASS 自带的字号样式转换时就被剥掉；PGS 是位图，只随可见画面等比缩放，字号/颜色/描边对它无效。位置按 `videoPictureBox()` 反算的**可见画面**摆放，不是窗口。

**延时**：`shiftSrtTimes(text, deltaMs)` 在**文本层**整体位移时间戳后重建 blob 再 `switch()`（重建时**不传 `name`**，否则 Artplayer 会弹「切换字幕」notice）。原文必须留档，否则每拖一次就在已位移的文本上再位移一次。

### 3.15 音轨（探测 → 决策 → 三条通道）

```
probeTracks (probe_mkv_tracks 产出，可能 null)
  → classifyAudio()  →  { count, dead, deadNames, shakyNames, audioIndex, defDead, pin }
  → audioPlan()      →  决定「要不要接管音频管线」
  → needsAudioEntry()→  决定「音轨按钮显不显示」
```

- `isDeadCodec(codec)`：`A_DTS*` / `A_TRUEHD` / `A_AC4` / `A_DRA` 是内核真解不了的；`A_AC3` / `A_EAC3` / `A_PCM*` 归 `shakyNames`（内核应该能解）。
- `classifyAudio()` 算出的 `pin` = 「内核会挑的那条 ≠ 用户要的默认轨」。`pin` 为真必须强制隔离并静音原轨（Chromium 会挑第一条能解的，不是播默认轨）。
- `needsAudioEntry()` = `count > 1 || (count > 0 && (dead > 0 || pin))` —— **「有多条可选」本身就是入口的理由**。
- `handleAudioCodecs()` 的提前返回**必须写在 `pin` 之后**（双 AAC 文件没有任何死编码轨，但内核照样播错）。

**三条通道**（由 `ensureAudioTrack` 按编码分流）：

| 通道 | 条件 | 实现 |
|---|---|---|
| **A 原生** | 默认轨编码内核能解且 `pin` 为假 | 什么都不做，内核直接播 |
| **B copy** | `isDeadCodec` 为假（AC-3 / E-AC-3 / PCM） | `transcode_audio_start` 带 `copy: true`，`-c:a copy` 抽 mka 挂外挂音轨（5.1 原样保留） |
| **C 流式转码** | `isDeadCodec` 为真（DTS / TrueHD…） | `audio_stream_start` + MSE 边转边播；失败回落整段转码 |

**双记账位**：`audioJob` / `subJob` 各自记 video + 序号，`transcode://progress|done|error|cancelled` 一律按 `p.kind` 分流、只比对该组自己的 video。胶囊由 `syncPill()` 统管，两组都空才隐藏。

### 3.16 画面适配与层定位

- `appSettings.fit` 三态：`contain`（默认，保比例留黑边）/ `cover`（裁满）/ `fill`（拉伸），落成 `video.style.objectFit`。
- `videoPictureBox(hostW, hostH, videoWidth, videoHeight, fit)` 返回**实际可见画面**在容器里的 `{ x, y, w, h }`。字幕层和 PGS 位图层的坐标**必须**用它反算 —— 用容器尺寸会在有黑边时整体偏出去。
- `object-fit` 变化**不触发 `ResizeObserver`**（元素尺寸没变），所以 `applyFitMode()` 主动广播 `bplayer:fit` 事件，两个层都监听它重排。
- 窗口尺寸、画面尺寸（`loadedmetadata`）、fit 切换、进入/退出全屏都是重排触发点。

### 3.17 断点续播 / 设置 / 进度胶囊 / 命令行打开

- **断点续播**：`bplayer_resume` 存 `{ path, position, duration, updatedAt }` 数组。播放中每 5 秒节流写一次，`ended` 时删除该条，读取时丢弃 30 天前的记录。点开有记录的视频先问「继续 / 从头」。
- **设置页**：`bplayer_settings`（`autoAudio` / `autoSubtitle` / `fit` / 常亮 / 自启动），改动即存，带 `SETTINGS_VER` 并在读取时对字段做白名单回落。
- **字幕面板**：`bplayer_subtitle_style`（字号 / 颜色 / 描边 / 位置 / 延时），**只有点「保存」才写**，预览即时生效但不落盘、不跨片。
- **进度胶囊**：底部浮层，显示 `音轨转换 xx%` / `字幕抽取中`；`syncPill()` 按 `audioJob` / `subJob` 两组状态渲染，点它 = `transcode_cancel_all()`。
- **命令行 / 文件关联**：启动后 `invoke('take_open_file')` 取首次参数；运行中监听 `bplayer://open-file` 事件换片。

## 4. 构建与验证

```powershell
npm run build -- --no-bundle        # 只编 exe，不出安装包（迭代时用这个）
npm run build                       # 出 NSIS 安装包，落在 src-tauri/target/release/bundle/nsis/
```

**验证界面改动必须把程序真跑起来看截图**，别只断言 DOM 状态。

调试用 CDP：

```powershell
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9333'
Start-Process 'src-tauri\target\release\danmuku-video-player.exe'
# 然后 http://127.0.0.1:9333/json 拿 webSocketDebuggerUrl，用 Runtime.evaluate 驱动
```

注意：`document.getElementById('someRange').value` 在面板**没打开过**时读到的是 HTML 初始值，不是运行时配置。验证默认值必须先把面板打开。

```powershell
cargo test --lib                     # Rust 单测 83 条，含合成的 MKV / SUP 端到端、argv 解析
npm run test:audio                   # 前端选轨决策单测（从 index.html 抠真函数跑，不依赖程序）
```

`cargo test --lib` 里有两条**不需要任何片源**的端到端测试，改 `mkv_sub` / `pgs_render` 时必须靠它们兜底（以前这两条链路只能在真机上用环境变量喂片才跑得到）：
- `mkv_sub::tests::synthetic_mkv_sparse_extract_srt_and_ass`：在内存里拼一个真 EBML 字节流（EBML 头 + Segment{Info(TimestampScale) + SeekHead + Cues + Cluster}）写到临时文件，真跑 `sparse_extract_srt()` 全链路，逐字节比对 SRT 与 ASS 输出，并验「不存在的轨必须 Err」。同文件还有 `synthetic_mkv_without_relative_position_is_fail_closed` 钉 fail-closed。
- `pgs_render::tests::synthetic_sup_decodes_to_png_event`：用 `libpgs::DisplaySetBuilder` 造一条最小 PGS 轨写成 `.sup`，真跑 `build()` 的「抽取 → RLE 解码 → 画布合成 → 裁边 → PNG 编码」，断言 PNG 魔数、宽高、`color_type`、像素 RGBA 值和 base64 无 `data:` 前缀。

前端那批「从 `index.html` 抠真函数跑」的脚本在 `H:\tmp\bplayer-audit\`（**不入库**，属临时资产，系统清理可能删掉）：`syntax-check.mjs`、`sub-ownership-test.mjs`、`jl-ownership-test.mjs`、`race-ownership-test.mjs`、`toast-stack-test.mjs`、`contract-check.mjs`、`a2..a10-*.mjs`、`b1..b4-*.mjs`。改前端后至少跑 `syntax-check.mjs` 加与改动区域相关的那几个。

`cargo test --release sparse_probe -- --nocapture` 是 `mkv_sub` 的真机单测入口，靠环境变量喂片源：

```powershell
$env:SPARSE_TEST_SRC='<mkv>'; $env:SPARSE_TEST_TRACK='9'; $env:SPARSE_TEST_ASS='0'
```

## 5. 硬性约束（都是踩过的坑）

**作用域**：`index.html` 是一个大 IIFE，但内部有二级块作用域——`createPlayer` 在 2 空格缩进，字幕/音频/PGS 那一大坨在 4 空格缩进的块里。**块内的函数声明不会外泄**，从外层调用会 `ReferenceError: pgsPump is not defined`。要跨层用，就显式挪到外层或挂到对象上。

**WebAudio 隔离**：`art.video.crossOrigin = 'anonymous'` 必须在**第一次设 `src` 之前**。页面源是 `tauri.localhost`，素材源是 `asset.localhost`，跨源被污染的媒体抓出来是纯静音，而且一个元素只能抓一次。

**音频规则（用户明确要求过两次）**：不管能不能转，**只能读默认音轨**；默认轨解不了就静音。Chromium 的真实行为是"挑第一条能解的"，不是播默认轨——所以 `classifyAudio()` 算出的 `pin`（`firstPlayable !== audioIndex`）为真时必须强制隔离并静音原轨，否则会放出一条没人要的评论轨。

**`pin` 优先于"编码都能解"**：双 AAC 文件（默认标在第二条）没有任何死编码轨，但内核照样会播错。所以 `handleAudioCodecs()` 的提前返回必须写在 `pin` 之后。入口判据见下面「音轨入口与切轨」，改动这块逻辑跑 `npm run test:audio` 回归。

**ffmpeg 随安装包分发（1.3.0 起）**：`src-tauri/resources/ffmpeg.exe` 是裁剪到 3.31 MiB 的 LGPL 构建（只留音频链：13 个音频解码器 + AAC 编码 + 5 个解封装 + 4 个封装；无视频编解码、无网络、无字幕解码、无 zlib），`tauri.conf.json` 的 `bundle.resources` 把它映射到安装目录根。`find_ffmpeg()` 定位顺序是 **随包分发 → exe 同目录 → PATH**：启动时 `setup` 用 `BaseDirectory::Resource` 解析一次存进 `transcode::BUNDLED_FFMPEG`（`OnceLock`），因为 `find_ffmpeg` 会被没有 AppHandle 的辅助函数调用。不打包的后果是干净机器 PATH 里没有 ffmpeg，音轨转码整个不可用。**外挂字幕仍然不走 ffmpeg**：`subtitle_text.rs` 自己解析 srt/vtt/ass/ssa（编码探测走 `MultiByteToWideChar` 的 936 代码页），ffmpeg 只留给它认不出来的格式兜底 —— 别把字幕路径改成依赖 ffmpeg。

**删文件前先精确匹配**：`clean_sidecar_tmp()` 只认 `` {stem} (音轨N).tmp.mka `` / `` {stem} (字幕N).tmp.srt ``（`is_our_sidecar_tmp`）。**不要退回「基名前缀 + 含 `.tmp.`」**——实测那样会把用户目录里 `反派 (上).tmp.txt`、`某片 (备注).tmp.mka` 这类普通文件一起删掉，不可逆。

**字幕只进内存**：内嵌文本字幕抽出来直接交给前端做 blob URL，**不写 `(字幕N).srt`**。只有音轨转码会落盘 `(音轨N).mka`。别再往媒体目录里写派生文件。

**稀疏抽取 fail-closed**：`mkv_sub.rs` 只要有一条 cue 解析不出（缺 `CueRelativePosition`、block 带 lacing、`TimestampScale != 1e6`、Cues 被截断、ASS 行格式不对），就整轨返回 `Err` 让调用方回退 ffmpeg。**不要改回"失败就 continue"**——那样会产出一份缺行却看起来正常的 SRT，用户只会觉得"这段怎么没字幕"，而且无从排查。

**ffmpeg 调用**：必须走 `hidden_command()`（`CREATE_NO_WINDOW`），否则会弹黑框；必须 `job::assign()` 进 Job Object，否则宿主被强杀后留下孤儿进程；同时把 `.tmp.*` 半成品清掉。

**流式音轨（C 级）是"单进程 + MSE"，不要退回"整条转完再挂文件"**：真解不了的音轨（DTS / TrueHD / DTS-HD）转 AAC 只有 38~76 倍实时，2 小时片子要 2~3 分钟才出成品，用户点开时不能干等。`audio_stream.rs` 从 seek 点起一路 ffmpeg 编 fMP4 写到 stdout，`FragmentSplitter` 按 `moof`/`mdat` 切块，前端拉一块 append 一块 —— init + 首块实测 60~70 ms 就绪。

- **每段单独起一个 ffmpeg 会在接缝掉约 21 ms 声音**（编码器 flush 尾部填充 + 新编码器 pre-roll，`empty_moov` 没有 edit list 可裁）。必须单进程连续编码，seek 时杀掉重起，代价只有 ~70 ms。
- **后端用 pull 而不是 push**：前端 `invoke('audio_stream_read')` 主动拉，`audio_stream_start/stop/state` 管生命周期。转码比播放快 40~80 倍，push（事件 / Channel）没有背压，2 小时片子两分钟就把几百 MB 推进前端内存；pull 下"前端不来取 = 读线程不读 stdout = ffmpeg 写满管道自己阻塞"，缓冲上限天然卡死，顺带省 CPU。
- **`mfra` 绝不能交给 MSE**：ffmpeg 会在流末尾追加 `mfra`（fMP4 文件索引），播放用不到，还可能让 source buffer 判成非法 box。`FragmentSplitter` 在 `init_sent` 之后只放行 `moof` / `mdat`。
- **ffmpeg 参数**：`-ss` 放 `-i` 之前（输入端快速 seek，输出时间戳由 ffmpeg 归零）；`-ac 2` 必加（WebView2 解码上限 6 声道，7.1 是静音）；`-map_chapters -1` 必加（否则 mov muxer 把 MKV 章节写成一条 QuickTime text 轨）；`-frag_duration` 单位是**微秒**；管道必须 `-flush_packets 1`。
- **前端三个必踩的坑**：`empty_moov` 下 Chromium 把 `duration` 报成 `Infinity`（**不是 NaN**），必须手动 `ms.duration = 总时长`；`sb.timestampOffset` 必须在第一次 `appendBuffer` **之前**设（之后改抛 `InvalidStateError`），它负责把"流内时间 0"摆到视频的起始秒；`QuotaExceededError` 要 `remove()` 播放头之前的区间再重试。
- **seek 要监听 `video:seeking`，不是 `art.on('seek')`**：直接改 `video.currentTime` 只产生原生事件，Artplayer 转发成 `video:seeking`；`art.on('seek')` 只在它自己的 seek 流程里发。两条都挂上，靠 pending 标志去重。落点不在 `sb.buffered` 里就杀掉后端重起并从新位置重建，否则浏览器只会把 `currentTime` 夹到缓冲末端、`seeked` 永远不来。
- **只替代「内核真解不了」那一路**：`ensureAudioTrack` 里只有 `isDeadCodec` 为真才进流式；能 copy 的（AC-3 / E-AC-3 / PCM）继续走原来的 `-c copy` 抽 mka，否则会把 5.1 压成 2.0。
- **必须回收旧缓冲，否则长片必炸 MSE 配额**：`buffered` 只进不出，转码 60~76 倍实时，几秒钟就能把几百秒塞进去。三层约束缺一不可：① `pullStreamChunk` 的递归也要过 `streamAheadFull`（只判本地队列长度会把几百秒拉进来）；② `appendStreamChunk` 超前瞻窗口就不消费；③ 每秒 tick 驱动 append、每 5 秒 `trimStreamBuffer()` 把播放头 60 秒以外的 `remove` 掉。第 ③ 步必须走定时器——数据一次性到齐后不会再有 `updateend`，光靠 append 回调永远回收不了。实测 8 分钟片不修时 4 秒内 buffered 冲到 457 秒（约 11 MB）并 `QuotaExceededError`；修完恒定在 181~189 秒。
- **同一个 MediaSource 的 `sourceopen` 会重复触发**（实测 `sb.remove()` 之后会再来一次）。回调里必须先判 `if (s.sb) return`，否则第二次 `addSourceBuffer` 抛 `NotSupportedError`，被当成「内核不支持 MSE」把整条流杀掉。
- **`SourceBuffer.remove()` 在 `MediaSource.readyState` 变成 `ended` 后会抛 `InvalidStateError`**：trim 前必须检查 `ms.readyState === 'open'`；短片转码几秒就跑完、`ended` 之后确实不需要再回收。

**CSP 有两个不能动的例外**：`tauri.conf.json` 配了 CSP，但
- `style-src` 必须带 `'unsafe-inline'`，**并且**必须把 `style-src` 写进 `dangerousDisableAssetCspModification`。Tauri 编译期会给 style-src 注入 nonce，而 CSP 规范规定「sources 里有 nonce/hash 时 `unsafe-inline` 被忽略」，于是 Artplayer 运行时动态创建的 `<style>` 全被挡（实测 24 条 violation、界面样式失效）。
- `connect-src` 必须允许 `blob:`。Artplayer 挂字幕是 `fetch(blobUrl)` 读文本的，少这一条字幕会**静默**挂不上（菜单显示已挂载、画面没字）。
其余指令保持收紧（`script-src 'self'` 靠 Tauri 的 hash、`object-src 'none'`、`base-uri`、`frame-ancestors`）。改完用 `H:\tmp\bplayer-audit\verify-csp.mjs` 跑一遍，要求 violation = 0。

**读文件一律走 `read_capped`**：`MAX_READ_BYTES = 64 MB`，`read_text` / `read_bytes` / `pair_danmaku` / `subtitle_text::to_srt` / ffmpeg 字幕 stdout 全走它。新增任何"整读进内存"的路径也必须走——一个几 GB 的损坏 .xml/.ass 能把进程 OOM 掉（不是报错，是进程直接消失）。弹幕路径超限按「没有弹幕」静默处理，文本/字幕路径返回 Err 给 toast。

**单实例 + 命令行打开**：用官方 `tauri-plugin-single-instance`（2.5.x），**必须注册成第一个 plugin**（`run()` 里 `.plugin(...)` 写在最前），第二个实例才会在 webview 创建前被拦下，不会先闪窗口、也不会抢 WebView2 用户数据目录。**不要退回手写命名互斥量**：手写版只能 `exit(0)`，用户双击另一个视频等于什么都没发生；插件会用 `WM_COPYDATA` 把第二个进程的 argv 转发给第一个实例。参数解析在 `open_file.rs::video_from_args`（只认 `VIDEO_EXTS` 里且**文件真实存在**的参数，跳过 `-`/`--` 开关）。两个入口：首次启动后端 `setup` 存 state、前端 `invoke('take_open_file')` 取（那时 webview 还没加载完，emit 必丢）；第二实例走 `stash` + emit `bplayer://open-file` 事件。Windows 没有 macOS 的 `RunEvent::Opened`，只能读 argv。`bundle.fileAssociations` 注册了 11 个视频扩展名。两个实例同时对同一片转码会让两份 ffmpeg 写同一个 `.tmp.mka`，这是单实例唯一的存在理由。

**命令必须 async + 走 `blocking()`**：Tauri 2 的同步命令**不冻窗口消息循环**（实测 3 秒的 SMB `fs_stat` 期间窗口 Responding 一直 True），但**所有 invoke 共用一条串行通道** —— 同一时刻慢命令 2997 ms，并发发起的本地快命令也是 2997 ms；改成 async + `crate::blocking::blocking(...)` 后快命令 2 ms。所以凡是做文件 IO / 等子进程 / 目录遍历的命令都要这么写，否则一条 SMB 断连能把挂字幕、探音轨、刷进度全堵在后面几十秒。例外不用改：`pick_video`/`pick_folder`（rfd 模态对话框要在主线程）、`pgs_image`/`pgs_close`/`pgs_current`（纯内存）、`autostart`（注册表毫秒级）、`transcode_target`（纯计算）。

**audio / subtitle 是两个独立 Job 槽**：`JobSlots { audio, subtitle }`。**不要合并回单槽** —— 实测 5 分钟片转音轨途中点内嵌字幕，共用槽会把音轨任务静默杀掉、mka 永远不落盘。取消分两种：换片/关窗用 `transcode_cancel_all`（两个都杀），单杀音轨用 `transcode_audio_cancel`。kill 一律在锁外做（等子进程退出要几百毫秒，拿锁会把另一个槽的命令一起卡住）。

**任务代际号只增不减**：`JOB_GEN: AtomicU64` + `fetch_add(1)`。**不要改回"槽里 gen+1、槽为空则从 1 开始"** —— 取消会把槽清空，下一个任务又从 1 编号，而上一单的 worker 正是靠 `take_if_gen(|j| j.gen == gen)` 认领自己那单；号一重合，旧 worker 就会摘走新任务、拿上一单的产物清单去改名。

**被取消/让位必须发 `transcode://cancelled`**：这条路径以前是静默清理，前端等不到 `done`，字幕的"加载中"状态会永远挂着。payload `{ kind, video, index }`（音轨的 index 序列化成 null）。

**前端按 kind 双记账位**：`audioJob { video, ordinal }` / `subJob { video, index, item }`，progress / done / error 一律按 `p.kind` 分流，只比对该组自己的 video。**不要退回单组 `transcodeVideo` 当全局闸门** —— 音轨 done 会把它清空，随后到达的字幕 done 就被当成过期事件丢掉（内嵌字幕永远不挂载）。pill 用 `syncPill()`，两组都空才隐藏。

**`transcode://cancelled` 只做提示，不清记账位**：片内换轨时旧任务的 cancelled 先到、新任务 done 后到，在事件里清账会让新 done 认领不上。状态清理归发起方（换片 / 点 pill 的 `transcode_cancel_all` 回调），不依赖事件。

**floating 面板互斥**：音轨、字幕、弹幕密度、弹幕屏蔽四个面板都用 `.block-panel`，固定在同一个 `right:16px; bottom:64px`。新加这类面板必须接进 `closeOtherPanels()`。

**画面定位一律按「实际可见画面」**：字幕层和 PGS 位图层的坐标必须用 `videoPictureBox(hostW, hostH, videoWidth, videoHeight, appSettings.fit)` 反算，**不能用容器尺寸**。默认 `fit = 'contain'`，有黑边时容器比真画面大一圈，按容器算会让两者整体偏出去（4:3 片在 16:9 窗口里偏约 190px，肉眼只看到"字幕怎么不在画面里"）。`object-fit` 变化**不触发 `ResizeObserver`**（元素尺寸没变），所以 `applyFitMode()` 广播 `bplayer:fit` 事件，位图层和字幕层都必须监听它重排。

**`.art-subtitle` 的 `bottom` 带 `!important`，且自定义属性必须 `setProperty`**：vendor 里 `.art-subtitle{bottom:var(--art-subtitle-bottom)!important}` —— inline `style.bottom` 盖不住，只能改 CSS 变量。而 Artplayer 的 `art.subtitle.style()` / `setStyles` 内部是 `el.style[key] = value`，对 `--art-subtitle-bottom` 这类自定义属性**静默失效**（实测赋值后 `getComputedStyle` 不变），必须 `el.style.setProperty(...)`（见 `setSubtitleVar()`）。字号同理走 `--art-subtitle-font-size`。另外控制栏显示时 Artplayer 会自己把字幕再抬 `--art-control-height`（46px），这是"别让控制栏压住字幕"的既有行为，不要压掉。

**字幕延时在 SRT 文本层做，不在 cue 对象上**：`mountSubtitleText` 把原文留档到 `subSourceText`，延时用 `shiftSrtTimes(text, deltaMs)` 整体位移时间戳后重建 blob 再 `switch()`。重建时**不要传 `name`**（Artplayer 会因此弹一条「切换字幕：xxx」notice）。原文必须留档，否则每拖一次延时就在已位移的文本上再位移一次，越拖越偏。

**字幕自动挂载优先级**：外挂（精确同名 / 语言后缀变体 / 拖入的都算）> 内嵌 `default` 轨 > 唯一一条内嵌文本轨 > 位图兜底 > 静默。**不要退回「精确同名 > 内嵌默认 > 变体外挂 > 位图」的四级排序** —— 只要目录里有外挂字幕就该用它，不该因为后缀差个 `.chs` 就退而去抽内嵌（抽内嵌要读整遍文件）。内嵌只认 `default` 标记，多条都没标默认时不替用户选。一条候选都没有时静默返回，**不弹任何提示**。改这块跑 `H:\tmp\bplayer-audit\subtitle-automount-test.mjs`。
**三类存档语义别混**：`bplayer_settings`（齿轮页开关，改动即存）、`bplayer_subtitle_style`（字幕样式，**只有点「保存」才写**，预览即时生效但不落盘、不跨片）、`bplayer_resume`（播放位置，自动 5 秒节流存、`ended` 删、30 天过期）。新增任何一个存档都要带 `ver` 字段并对字段做白名单回落。配套的 `subtitleManual` / `danmakuManual` 令牌：**只有非空的手动选择才置位** —— 取消对话框、空结果、关闭字幕、清空弹幕都不置位，否则自动匹配会永久失效。

**音轨入口与切轨**：`needsAudioEntry()` = `count > 1 || (count > 0 && (dead > 0 || pin))` —— **「有多条可选」本身就是入口的理由**，不能只看「要不要修正」。只看 dead/pin 的后果：5 条全 AC-3、默认轨也对、内核能解的片子（多音轨国配片典型）音轨按钮根本不出现，用户以为文件只有一条轨（2026-11-17 实测）。注意「入口显示」和「要不要接管音频管线」是两件独立的事：同一部片可以 `audioPlan().native === true`（不用接管）却仍然必须给入口。手动切轨按编码分流：`isDeadCodec` 为假就 `-c copy` 抽进 mka（转 AAC 会被 `-ac 2` 把 5.1 压成立体声，用户只是换个语言听，不该顺手降质），为真才转 AAC。`ensureAudioTrack` 里读 `probeTracks[audioIdx].codec` **必须先判空**：探测失败/换片途中它是 null，而在 `fs_stat` 的异步回调里抛 ReferenceError 会被静默吞掉，表现成"点了没反应、胶囊卡在某个百分比"。

**设置项改默认值**：`localStorage` 里已经存过的旧值会盖住新默认。凡是改默认值，必须同时升 `SETTINGS_VER` / `CONFIG_VER` / `DENSITY_VER` / `SUB_STYLE_VER` 并让旧存档（无版本号）回落到新默认，否则老用户看不到变化。**换了档位集合时，光升版本号还不够**：旧值可能落在新 `<option>` 列表之外，`sel.value = String(旧值)` 会把下拉框置成无选中（面板空白）而样式仍在生效 —— 必须在 `loadSubStyle` 里做显式档位映射（`SUB_SIZE_V1_MAP`，字幕字号 v1→v2 就是这么迁的）。

**收敛段（done / stop / error）最容易漏，且漏得对称**：2026-11-18 的工程控制论评审扫出 9 处 P0/P1，全部集中在"回路的最后一步"——done 归属确认、stop 的状态复位、error 的收尾。规律是：**同一个受控状态有多条退出路径时，清理往往只写在其中一条上**（stop 有 blob 回收而换片没有；then 收胶囊而 catch 不收；ended 复位续播令牌而 stop 不复位；字幕有 pickSeq 而音轨没有）。加任何"进入"逻辑时，先数一遍这个状态有几条退出路径，再逐条写清理 —— 不对称就是缺陷。
**声明级 fail-closed ≠ 代码级 fail-closed**：AGENTS.md 写着"任何一条 cue 解析失败→整轨拒绝"，代码里却有 `if let Some(x)` 无 else 的静默旁路（P0）。写"绝不静默"这类承诺时，顺带把承诺对应的**每个分支**过一遍，并配一条单元测试钉住；没测试的承诺通常已经有洞。
**默认值会掩盖状态**：`end = start + 2000` 这种"给个合理默认"让"已收尾/未收尾"变得无法区分，于是收尾判断恒假（死代码）+ 兜底逻辑误伤合法值。要区分两种状态就用哨兵（`f64::INFINITY`）并在每个出口显式收敛，不要用"看起来合理的默认值"。

**发版**：GitHub Release / tag / 上传资产要**每轮单独拿同意**，跟 git push 是两个独立的门。版本号要同时改 `package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml` 三处。

## 6. 代码风格

- 注释用中文，解释**为什么**这么做（尤其是绕过某个引擎缺陷的地方），不要复述代码在做什么。
- 前端是 ES5 风格的 IIFE，别引入构建链或框架；新逻辑跟着现有的 `$()` / `toast()` / `invoke()` 走。
- Rust 侧错误一律 `Result<_, String>` 给前端直接显示，不要吞成 `Option`。
- 仓库只放代码。片源和抽出的 `(音轨N).mka` 都在各自视频旁边，别复制进仓库；临时脚本与测试素材放仓库外（`H:\tmp\`），仓库里不留 `tmp/`、`test*` 这类痕迹。
