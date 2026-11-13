# 绝对时间进度（NPT）模式

默认情况下 RTSP 服务端对直播返回 `Range: npt=now-`（或客户端请求的
`npt=0-`），播放器每次连接都从 0 开始计时。

摄像机的“设备运行时间/绝对时间”进度可以通过 `ServerConfig::timeline`
选择：

```rust
use rtflow::{ServerConfig, TimelineMode};

let config = ServerConfig::builder("0.0.0.0:8554".parse().unwrap())
    // 设备风格：npt = 服务进程已运行秒数，重连不回 0
    .timeline(TimelineMode::ServerUptime)
    .build()?;
```

`TimelineMode`：

| 模式 | PLAY 返回的 NPT 起点 | 说明 |
|---|---|---|
| `Relative`（默认） | `now`/`0` | 每次会话从 0 开始 |
| `ServerUptime` | 服务进程运行秒数 | 重连不归零，进程重启才归零 |
| `Unix` | Unix 时间戳秒 | 很大的绝对墙钟值 |
| `Absolute { offset }` | `offset + 进程运行秒数` | 模拟已有设备 uptime |

## 协议层发生什么

PLAY 响应：

```text
Range: npt=123456.789-
RTP-Info: url=trackID=0;seq=42;rtptime=90000
```

- `Range` 起点是客户端进度条显示的初始位置；
- `RTP-Info.rtptime` 必须对应 `Range` 起点所在帧的 RTP 时间戳；
- 客户端按 `NPT = Range.start + (rtp_ts - rtptime) / clock_rate`
  计算后续进度。

rtflow 的直播发送端使用启动关键帧的 RTP 时间戳生成 `RTP-Info`，因此
绝对模式下 VLC 会从 `npt=<uptime>` 开始并继续增长；每次重新 PLAY 重新
取当前 uptime，所以不会归零。

## live_stream_demo

```bash
# 设备 uptime 风格
cargo run --example live_stream_demo -- --timeline uptime

# 固定起始 uptime（例如 1,000,000 秒）
cargo run --example live_stream_demo -- \
  --timeline absolute --timeline-offset-secs 1000000

# Unix 墙钟时间
cargo run --example live_stream_demo -- --timeline unix
```

不传 `--timeline` 时保持 `relative`。

## 限制

- 只影响**直播**；VOD 仍按文件时间轴。
- 客户端显式请求非零 seek（如 `Range: npt=100-`）仍按 400/457 规则拒绝，
  因为直播没有可 seek 的存储后端。
- `ServerUptime`/`Absolute` 模式下，RTCP SR 的 NTP 锚点也使用同一
  时间轴（而非真实墙钟），以便优先用 SR 校准的播放器显示相同进度。
  `Unix` 模式使用真实 Unix NTP。
