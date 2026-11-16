# SRT 与 ffmpeg 互操作验证（2026-11-16）

环境：ffmpeg/ffprobe 6.1.1（libsrt），`rtflow-server` 本地构建，loopback。
所有推流均为 `testsrc2` +（部分场景）`sine`，MPEG-TS 封装。

## 场景与结果

| 场景 | 配置 | 结果 |
| --- | --- | --- |
| 明文推流 → HLS | SRT :19100，HLS 开 | 连接建立、mount 注册；HLS 200，`ffmpeg -i` 解码 3s 成功；15s/3129 包/3.84MB |
| 推流 → 录制/VOD | 路径 `record = true` | 6 段录制 3.66MB 完成；VOD playlist 可播放，ffprobe 时长 14.96s |
| AES-128/192/256 | `passphrase=secretpass12345`，pbkeylen 16/24/32 | 三种均连接成功、HLS 200、`encrypted=true`、`undecryptable=0` |
| 错误口令 | `wrongpass12345` | 服务端 `reason=1010`（BADSECRET），ffmpeg 退出码 251 |
| 明文对端 + 强制加密 | 无 passphrase | 服务端 `reason=1011`（UNSECURE），ffmpeg 退出码 251 |
| SRT 流密钥 | `[auth] mode="token"`，签发 publish/read key | 带 key 推流成功，HLS `?key=` 200；无 key / 错 key 均 251 |
| Stream ID 白名单 | `allowed_stream_ids=["live/allowed"]` | `live/allowed` 成功且 HLS 200；`live/denied` 251 |
| SRT 回源（plain） | `[[srt.pull]]` → ffmpeg listener :19110 | mount 在线，HLS 200，解码 3s 成功，RTT 74µs |
| SRT 回源（加密） | pull passphrase + ffmpeg listener 加密 | mount 在线，HLS 200，`encrypted=true`，RTT 73µs |
| SRT 输出 | RTMP 源 → `[[srt.push]]` → ffmpeg listener :19111 | ffmpeg 写出 12.0s TS，H264+AAC，解码成功；`/api/v1/srt` 显示 sender、bytes_sent 实时增长 |
| 发送端 10% 丢包 | UDP 代理丢数据包 | ffmpeg 收到 12.01s 完整可解码文件 |
| 接收端 10% 丢包 | UDP 代理丢数据包，latency 300ms | 2507 包 / 2428 交付 / **0 skipped** / **0 未恢复** / 156 重传；HLS 解码成功 |
| 高码率 | 720p 4Mbps 20s | 8238 包全部交付，0 丢包/0 跳过，RTT ~107µs |

## 验证中发现并修复的问题

1. **TSBPD 时间基错误导致 ARQ 失效**（严重）
   - 现象：代理丢包后缺失包全部被 TLPKTDROP 跳过，`losses/skipped` 相等，重传数几乎为 0。
   - 根因：`set_time_base` 用 `now - handshake_ts`，首个数据包又用自身时间戳作外部基准，握手偏移被重复扣除；包一到就被判定过期。
   - 修复：改为 `now + (packet_ts - handshake_ts)` 语义；NAK 每 20ms 持续重发。
   - 复测：10% 丢包下 0 skipped、156 次重传恢复；HLS 正常解码。回归测试见 `crates/rtflow-srt/src/tsbpd.rs`。

2. **SRT RTT 指标恒为 0**
   - ACKACK 分支缺少 metrics 写入；补齐后 `/metrics` 显示 `rtflow_srt_rtt_microseconds ≈ 81`。

3. **push 状态 bytes_sent/RTT 只在断开时更新**
   - 改为每帧发送后更新，`/api/v1/srt` 实时可见。

4. **token 模式短密钥启动失败信息晦涩**
   - `auth.token_secrets` 少于 32 字节时启动报 `resource limit exceeded: Digest nonce secret`；
   - 配置校验改为明确错误：`auth.token_secrets entries must be at least 32 bytes (got N)`。

## 后续排查与修复（同日）

### HLS 直播 AAC DTS 非单调

- 根因：SRT 的 MPEG-TS demux 把一个音频 PES（含约 9 个 ADTS 帧）当成一个 AccessUnit，
  HLS 适配层却按 1024 ticks 的单个 AAC 样本处理，时间轴被压缩，段边界出现
  `10240 >= 1920` 之类回跳。
- 修复：`ts_demux` 按 ADTS 头拆分帧、去掉 ADTS 头、逐帧递增 PTS，并从 ADTS 解析
  采样率/声道（此前固定 44100/2，源为 48kHz 时 init 里的 esds 错误）。
- 同时修正首个音频 PES 早于视频 PES 时 33 位时间戳按有符号差值处理（此前被取模成
  95443s）。
- 复测：直播 HLS 解码 0 条 non-monotonic 警告，ffprobe 显示 48000Hz/1ch（源为单声道）。

### VOD duration 与首段不可解码

- `VOD duration_seconds` 只在目录首次出现时写入，录制结束不再刷新 → 一直是首次扫描的
  0.6s。改为按“目录 + 段数 + 时长”变化刷新条目，最终显示 11.44s，与 playlist 一致。
- 录制从 mount 中途开始，首帧是 P 帧且无 SPS/PPS；VOD 的 fMP4 init 在首个关键帧切段前
  生成，avcC 为空，整个 VOD 无法解码。修复：
  - 录制等待首个视频关键帧再开始；
  - HLS segmenter 在切段前先提取关键帧的 SPS/PPS/参数集，避免 init 使用空配置。
- 复测：VOD `ffmpeg -i ... -f null -` 退出 0，无 “No start code / Invalid data”，
  avcC 长度 45；直播 HLS 仍正常。

## 复现方式

测试脚本与日志位于运行环境的 `/tmp/vrf/`（非仓库内容）；核心命令：

```bash
# 明文推流
ffmpeg -re -f lavfi -i testsrc2=size=640x360:rate=25 -f lavfi -i sine=frequency=440 \
  -t 15 -c:v libx264 -preset ultrafast -pix_fmt yuv420p -g 25 -c:a aac -shortest \
  -f mpegts 'srt://127.0.0.1:19100?streamid=live/plain&latency=120'

# 加密推流
ffmpeg ... -f mpegts 'srt://127.0.0.1:19101?streamid=live/enc&passphrase=secretpass12345&pbkeylen=32'

# 播放验证
ffmpeg -i http://127.0.0.1:18100/hls/live/plain/index.m3u8 -t 3 -f null -
```

### 修复后的回归

| 场景 | 结果 |
| --- | --- |
| SRT 直播 HLS（48kHz 单声道） | 解码 0 条 non-monotonic；ffprobe 48000Hz/1ch |
| SRT 录制 → VOD | duration 10.44s（12s 推流、从关键帧起录）；VOD 解码退出 0，avcC 45 字节 |
| RTMP 录制 → VOD | VOD 解码退出 0，无 start code/invalid data |
| 纯音频 RTMP 录制 → VOD | 录制正常（不等视频关键帧），VOD 解码退出 0 |

### RTMP 录像时长偏大（进一步根因）

此前把“10s 素材录成 14.5s”归因于 RTMP 时间戳膨胀，实测发现根因在
`rtflow-rtmp` 的 chunk 解析器：

- 解析 fmt0/1/2 头时先提交 timestamp/delta，再检查整个 chunk 的 payload 是否
  已到齐；TCP 分段导致 `NeedMoreData` 时状态不回滚，下一次 poll 会把同一个
  delta 再加一遍。
- 结果音频时间戳随 TCP 切分方式膨胀（字节流离线回放正确，线上按 7/100/1000
  字节分段分别放大到 76s/7s/2.5s），录像 PTS 跨度随之变大。
- 修复：先把头部解析到局部变量，确认 payload 完整后再提交状态与消费字节；
  新增“逐字节/小块 feed 时间戳不变”的回归测试。
- 复测：10s RTMP 推流 VOD 时长 9.98s，6s 纯音频 VOD 5.50s，解码退出 0。

## 自动化测试

```text
cargo test --workspace --all-features   522 passed / 0 failed
cargo clippy --workspace --all-targets --all-features -- -D warnings  clean
```
