# 文件流设计与支持矩阵

## 数据流

```text
File / custom reader
        │ blocking Demuxer::next_sample
        ▼
bounded prefetch (default 8 samples)
        │ DTS/PTS wall-clock pacing
        ├── LocalPublisher → RTSP/RTP fan-out
        └── PipelineIngress → Element DAG
```

`Demuxer` 只输出编码后的 `DemuxedSample`，不解码、不转码。`FileSource` 把同步磁盘读取放入 `spawn_blocking` worker，异步任务负责实时节流、空闲暂停和投递。取消时先关闭预取接收端，再等待 worker，确保阻塞在 `blocking_send` 的线程能够退出。

## 格式矩阵

| 文件/容器 | 编码 | 处理 |
|---|---|---|
| MP4、MOV、M4A（含 fragmented MP4 sample table） | H.264、H.265、AAC；`alaw`/`ulaw` sample entry | 读取 DTS/PTS/CTTS/sample table；AVCC/HVCC 转 Annex-B；关键帧注入 VPS/SPS/PPS；AAC 去容器封装 |
| Matroska/MKV | H.264、H.265、AAC、A_MS/ACM G.711 | 支持 lacing、CodecPrivate、CodecDelay、TimestampScale、Cue/线性 seek |
| MPEG-TS、M2TS | H.264、H.265、ADTS AAC、约定 G.711 | PAT/PMT、连续计数、PES、PTS/DTS 33-bit 回绕；AAC PES 拆为独立 AU；支持 188/192 字节包 |
| WAV | PCMA、PCMU | RIFF chunk 扫描，格式 tag 6/7，按采样率切帧 |
| 裸流 | Annex-B H.264/H.265、ADTS AAC、PCMA、PCMU | 视频访问单元聚合；ADTS 去头；帧率/采样参数由 options 提供 |

WebM 容器可以被 Matroska parser 识别，但第一期 RTP 编码集合不含 WebM 常用的 VP8/VP9/AV1/Opus；这类文件会明确返回“不含受支持轨道”，不会把未知编码误发为 H.264/AAC。

## 公开接口

- `FileSource::open` / `open_async`：按扩展名自动选择后端，也可用 `ContainerFormat` 强制指定；异步服务启动路径优先使用后者，避免索引解析占用 runtime worker。
- `FileSource::description` / `duration`：注册 mount 前取得 SDP 所需轨道信息。
- `FileSource::play`：按时间戳推送给 `LocalPublisher`。
- `FileSource::play_into_pipeline`：按时间戳注入已有 Element DAG。
- `FileSource::into_demuxer`：交给自定义调度器或厂商媒体管线。
- `FilePlayback::{cancel, stop, wait}`：显式取消和可等待完成；Drop 也会取消。
- `FileSourceOptions`：格式、0.01–100 倍播放速率、起播位置、循环、空闲策略、预取和样本上限，以及裸流参数。

`LoopMode::Repeat(n)` 表示首次播放后再重复 n 次。`start_position` 会在每次循环开始时应用。MP4 视频 seek 会退到目标前的同步样本以保留解码前滚。

## 资源与零拷贝边界

- 预取队列和单样本大小均有硬上限；默认分别是 8 个样本和 8 MiB。
- MP4 sample table 和 Matroska 索引属于文件元数据，内存规模随 sample/cue 数量增长；payload 不会整文件缓存。
- 容器读取必须把文件字节放入一个拥有所有权的缓冲区。单 NAL AVCC/HVCC 转换用 `Bytes::slice` 复用存储；多 NAL 或参数集注入分配一次连续 Annex-B 缓冲区。
- 进入 `AccessUnit` 后，RTSP fan-out、RTP packetizer 和 pipeline 继续共享 `Bytes`；只有分片重组和系统调用边界发生必要拷贝。

## 共享文件 mount 与 VOD

当前 `MountConfig::Local + FileSource` 是共享播放时间线：所有客户端看到同一位置，适合循环宣传片、设备回放频道、告警片段广播和文件转直播。默认无订阅者时暂停，第一位订阅者到来后继续。

每个客户端独立 `Range: npt=`、PLAY/PAUSE 游标、随机 seek 和速率控制需要把 demuxer 放入 RTSP session 作用域，并为每个 session 创建独立调度器。这属于后续 VOD session 模块，不应通过为共享 mount 隐式复制 N 份 reader 来实现。
