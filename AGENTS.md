# AGENTS.md

给在这个仓库里改代码的 AI / 人类协作者。只写容易踩错、又不好从代码里一眼看出来的东西。

## 1. 这是什么

Tauri 2 桌面播放器：打开本机视频，自动配同目录的弹幕 XML 与字幕一起播。

- 前端**没有构建步骤**：`src/index.html` 是唯一入口，CSS 与 JS 全部内联，`src/vendor/` 下的 Artplayer 与两个插件是本地化副本，不走 CDN。
- 后端是 Rust，9 个模块，多数命令是"调外部 ffmpeg / 读二进制 / 开系统对话框"的薄封装。

## 2. 代码地图

| 文件 | 职责 |
|---|---|
| `src/index.html` (~3500 行) | 整个前端。播放器装配、弹幕、字幕菜单 + 字幕样式/位置/延时、音轨菜单、画面适配、停止播放、断点续播、设置页、拖放、进度胶囊 |
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

## 3. 构建与验证

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
cargo test --lib                     # Rust 单测 74 条，含合成的 MKV / SUP 端到端、argv 解析
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

## 4. 硬性约束（都是踩过的坑）

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

**三类存档语义别混**：`bplayer_settings`（齿轮页开关，改动即存）、`bplayer_subtitle_style`（字幕样式，**只有点「保存」才写**，预览即时生效但不落盘、不跨片）、`bplayer_resume`（播放位置，自动 5 秒节流存、`ended` 删、30 天过期）。新增任何一个存档都要带 `ver` 字段并对字段做白名单回落。配套的 `subtitleManual` / `danmakuManual` 令牌：**只有非空的手动选择才置位** —— 取消对话框、空结果、关闭字幕、清空弹幕都不置位，否则自动匹配会永久失效。

**音轨入口与切轨**：`needsAudioEntry()` = `count > 1 || (count > 0 && (dead > 0 || pin))` —— **「有多条可选」本身就是入口的理由**，不能只看「要不要修正」。只看 dead/pin 的后果：5 条全 AC-3、默认轨也对、内核能解的片子（多音轨国配片典型）音轨按钮根本不出现，用户以为文件只有一条轨（2026-11-17 实测）。注意「入口显示」和「要不要接管音频管线」是两件独立的事：同一部片可以 `audioPlan().native === true`（不用接管）却仍然必须给入口。手动切轨按编码分流：`isDeadCodec` 为假就 `-c copy` 抽进 mka（转 AAC 会被 `-ac 2` 把 5.1 压成立体声，用户只是换个语言听，不该顺手降质），为真才转 AAC。`ensureAudioTrack` 里读 `probeTracks[audioIdx].codec` **必须先判空**：探测失败/换片途中它是 null，而在 `fs_stat` 的异步回调里抛 ReferenceError 会被静默吞掉，表现成"点了没反应、胶囊卡在某个百分比"。

**设置项改默认值**：`localStorage` 里已经存过的旧值会盖住新默认。凡是改默认值，必须同时升 `SETTINGS_VER` / `CONFIG_VER` / `DENSITY_VER` 并让旧存档（无版本号）回落到新默认，否则老用户看不到变化。

**发版**：GitHub Release / tag / 上传资产要**每轮单独拿同意**，跟 git push 是两个独立的门。版本号要同时改 `package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml` 三处。

## 5. 代码风格

- 注释用中文，解释**为什么**这么做（尤其是绕过某个引擎缺陷的地方），不要复述代码在做什么。
- 前端是 ES5 风格的 IIFE，别引入构建链或框架；新逻辑跟着现有的 `$()` / `toast()` / `invoke()` 走。
- Rust 侧错误一律 `Result<_, String>` 给前端直接显示，不要吞成 `Option`。
- 仓库只放代码。片源和抽出的 `(音轨N).mka` 都在各自视频旁边，别复制进仓库；临时脚本与测试素材放仓库外（`H:\tmp\`），仓库里不留 `tmp/`、`test*` 这类痕迹。
