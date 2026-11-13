# Built-in live source

`SyntheticSource` (feature `test-source`) feeds a local mount with a real,
decoder-ready stream — useful for RTSP demos, player smoke tests and CI
without an external encoder.

```rust
let mount = server
    .register_mount("/camera".into(), MountConfig::Local { description })
    .await?;
SyntheticSource::spawn(
    mount.publisher()?,
    description,
    SyntheticSourceConfig { fps: 25, keyframe_interval: 25 },
)?;
```

## What it sends

| Track | Payload |
|---|---|
| H.264 / H.265 | 1-second GOP (10 frames) of a 176x144 test pattern |
| VP8 / VP9 / AV1 | 10-frame GOP, IVF frame boundaries |
| MPEG-4 Part 2 / H.263 | 10-frame GOP in the codec's elementary stream format |
| JPEG | 10 complete MJPEG frames (packetizer strips the JPEG header for RFC 2435) |
| AAC / Opus / MP3 / G.726 | One second of 440 Hz tone, real encoded packets |
| PCMA / PCMU | Generated 20 ms G.711 tone packets |

Video timestamps advance at `clock_rate / fps`; audio timestamps advance by the
codec's real packet duration (20 ms G.711/G.726, 1024 samples AAC, 960 samples
Opus, 1152 samples MP3). The keyframe interval controls the GOP loop and is
independent for each track.

The samples are embedded in the crate, so the source works offline and with no
FFmpeg/GStreamer dependency at runtime. They are only compiled with
`test-source`, which is not part of `full`.

## Regenerating the samples

```bash
FFMPEG=/path/to/ffmpeg python3 scripts/gen-test-source-samples.py
```

The script writes `src/source/assets/{codec}.bin` in the `RTS1` format:

```
magic "RTS1" | u16 unit count | per unit: u8 flags | u32 length | payload
```

The generator requires FFmpeg with `libx264`, `libx265`, `libvpx`,
`libvpx-vp9`, `libsvtav1`, `mjpeg`, `mpeg4`, `h263`, `aac`, `libopus`,
`libmp3lame` and `g726`. Regenerate and commit the assets when changing the
pattern or codec settings.

## Tests

```bash
cargo test --features test-source --test rtsp builtin_source
cargo test --features test-source --test rtsp builtin_source_plays_with_ffmpeg
```

The second test starts the source for every codec and asks FFmpeg to decode
one second over RTSP; AV1 is enabled only when the FFmpeg in use has AV1 RTP
support (FFmpeg master/8.x).
