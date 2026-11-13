#!/usr/bin/env python3
"""Regenerate the embedded samples used by rtflow's built-in test source.

The source is part of the `test-source` feature only. Video samples are one
GOP of a 176x144/10 fps `testsrc2` pattern; audio samples are one second of a
440 Hz tone. Run with FFmpeg 6+ (FFmpeg master/8.x is required for AV1 RTP
experiments, but sample generation only needs an AV1 encoder).

    FFMPEG=/path/to/ffmpeg python3 scripts/gen-test-source-samples.py
"""
import os
import pathlib
import struct
import subprocess

FF = os.environ.get("FFMPEG", "ffmpeg")
ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "src" / "source" / "assets"
OUT.mkdir(parents=True, exist_ok=True)


def run(args):
    subprocess.run([FF, "-hide_banner", "-loglevel", "error", "-y", *args], check=True)


def write(name, frames):
    data = bytearray(b"RTS1") + struct.pack("<H", len(frames))
    for key, payload in frames:
        data += struct.pack("<BI", 1 if key else 0, len(payload))
        data += payload
    (OUT / name).write_bytes(data)
    print(f"{name}: {len(frames)} units, {len(data)} bytes")


def video_raw(name, encoder, extra, muxer):
    path = OUT / f"{name}.raw"
    run([
        "-f", "lavfi", "-i", "testsrc2=size=176x144:rate=10:duration=1",
        "-c:v", encoder, "-g", "10", "-pix_fmt", "yuv420p", *extra,
        "-f", muxer, str(path),
    ])
    return path


def split_annexb(buf, aud_type):
    starts = []
    i = 0
    while i + 3 < len(buf):
        if buf[i:i + 3] == b"\x00\x00\x01":
            starts.append((i, 3))
            i += 3
        elif i + 4 < len(buf) and buf[i:i + 4] == b"\x00\x00\x00\x01":
            starts.append((i, 4))
            i += 4
        else:
            i += 1
    frames = []
    current = None
    for pos, sc in starts:
        nal_type = (buf[pos + sc] & 0x1F) if aud_type == 9 else ((buf[pos + sc] >> 1) & 0x3F)
        if nal_type == aud_type:
            if current is not None:
                frames.append(buf[current:pos])
            current = pos
    if current is not None:
        frames.append(buf[current:])
    return frames


def annexb_keyframe(au, is_h265):
    for index in range(len(au) - 4):
        if au[index:index + 4] == b"\x00\x00\x00\x01":
            start = index + 4
        elif au[index:index + 3] == b"\x00\x00\x01":
            start = index + 3
        else:
            continue
        if start >= len(au):
            continue
        header = au[start]
        if is_h265:
            nal_type = (header >> 1) & 0x3F
            if 16 <= nal_type <= 21:
                return True
        elif header & 0x1F == 5:
            return True
    return False


# H.264 / H.265: split at access-unit delimiters (aud=1).
for name, encoder, params, muxer, is_h265, aud in [
    ("h264", "libx264", ["-preset", "ultrafast", "-tune", "zerolatency",
                         "-x264-params", "aud=1:repeat-headers=1"], "h264", False, 9),
    ("h265", "libx265", ["-preset", "ultrafast", "-x265-params",
                         "aud=1:keyint=10:min-keyint=10:scenecut=0:repeat-headers=1"],
     "hevc", True, 35),
]:
    raw = video_raw(name, encoder, params, muxer)
    frames = split_annexb(raw.read_bytes(), aud)
    write(f"{name}.bin", [(annexb_keyframe(au, is_h265), au) for au in frames])
    raw.unlink()

# IVF codecs.
for name, encoder, extra in [
    ("vp8", "libvpx", ["-b:v", "200k"]),
    ("vp9", "libvpx-vp9", ["-b:v", "200k"]),
    ("av1", "libsvtav1", ["-preset", "12", "-crf", "50"]),
]:
    path = OUT / f"{name}.ivf"
    run([
        "-f", "lavfi", "-i", "testsrc2=size=176x144:rate=10:duration=1",
        "-c:v", encoder, "-g", "10", "-pix_fmt", "yuv420p", *extra,
        "-f", "ivf", str(path),
    ])
    buf = path.read_bytes()
    offset = 32
    frames = []
    while offset + 12 <= len(buf):
        size = struct.unpack("<I", buf[offset:offset + 4])[0]
        offset += 12
        frames.append(buf[offset:offset + size])
        offset += size
    write(f"{name}.bin", [(index == 0, frame) for index, frame in enumerate(frames)])
    path.unlink()

# MPEG-4 Part 2: each VOP starts an access unit; keep the VOS/VO/VOL prefix.
raw = video_raw("mpeg4", "mpeg4", ["-q:v", "5"], "m4v")
buf = raw.read_bytes()
vops = [i for i in range(len(buf) - 4) if buf[i:i + 4] == b"\x00\x00\x01\xb6"]
frames = []
for index, start in enumerate(vops):
    end = vops[index + 1] if index + 1 < len(vops) else len(buf)
    payload = buf[start:end]
    if index == 0:
        payload = buf[:start] + payload
    frames.append(((buf[start + 4] >> 6) == 0, payload))
write("mpeg4.bin", frames)
raw.unlink()

# H.263: every picture starts with the 22-bit PSC.
raw = video_raw("h263", "h263", [], "h263")
buf = raw.read_bytes()
psc = [i for i in range(len(buf) - 2) if buf[i:i + 3] == b"\x00\x00\x80"]
frames = []
for index, start in enumerate(psc):
    end = psc[index + 1] if index + 1 < len(psc) else len(buf)
    frames.append((True, buf[start:end]))
write("h263.bin", frames)
raw.unlink()

# MJPEG: concatenated SOI..EOI frames.
raw = video_raw("jpeg", "mjpeg", ["-huffman", "default", "-q:v", "8", "-pix_fmt", "yuvj420p"], "mjpeg")
buf = raw.read_bytes()
soi = [i for i in range(len(buf) - 1) if buf[i:i + 2] == b"\xff\xd8"]
frames = []
for index, start in enumerate(soi):
    end = soi[index + 1] if index + 1 < len(soi) else len(buf)
    frames.append((True, buf[start:end]))
write("jpeg.bin", frames)
raw.unlink()

# AAC: ADTS frames without the 7/9-byte ADTS headers.
path = OUT / "aac.adts"
run(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=1",
     "-c:a", "aac", "-b:a", "64k", "-ac", "2", "-f", "adts", str(path)])
buf = path.read_bytes()
offset = 0
frames = []
while offset + 7 <= len(buf) and buf[offset] == 0xFF and (buf[offset + 1] & 0xF0) == 0xF0:
    header = 7 + (0 if (buf[offset + 1] & 1) else 2)
    length = ((buf[offset + 3] & 3) << 11) | (buf[offset + 4] << 3) | (buf[offset + 5] >> 5)
    frames.append((True, buf[offset + header:offset + length]))
    offset += length
write("aac.bin", frames)
path.unlink()

# MP3: strip the ID3 tag, then split at frame headers.
path = OUT / "mp3.mp3"
run(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100:duration=1",
     "-c:a", "libmp3lame", "-b:a", "64k", "-f", "mp3", str(path)])
buf = path.read_bytes()
offset = 0
if buf[:3] == b"ID3":
    size = ((buf[6] & 0x7F) << 21) | ((buf[7] & 0x7F) << 14) | ((buf[8] & 0x7F) << 7) | (buf[9] & 0x7F)
    offset = 10 + size
bitrates = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320]
rates = {0: 44100, 1: 48000, 2: 32000}
frames = []
while offset + 4 <= len(buf) and buf[offset] == 0xFF and (buf[offset + 1] & 0xE0) == 0xE0:
    bitrate = bitrates[(buf[offset + 2] >> 4) & 0xF] * 1000
    sample_rate = rates[(buf[offset + 2] >> 2) & 3]
    length = 144 * bitrate // sample_rate + ((buf[offset + 2] >> 1) & 1)
    if length <= 0:
        break
    frames.append((True, buf[offset:offset + length]))
    offset += length
write("mp3.bin", frames)
path.unlink()

# G.726 at 16/24/32/40 kbit/s: 20 ms frames (40/60/80/100 bytes).
for rate, size in [(16, 40), (24, 60), (32, 80), (40, 100)]:
    path = OUT / f"g726_{rate}.raw"
    run(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=8000:duration=1",
         "-c:a", "g726", "-b:a", f"{rate}k", "-f", "g726", str(path)])
    buf = path.read_bytes()
    write(f"g726_{rate}.bin", [(True, buf[i:i + size]) for i in range(0, len(buf) - size + 1, size)])
    path.unlink()

# Opus: Ogg pages -> packets; drop OpusHead/OpusTags.
path = OUT / "opus.ogg"
run(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=1",
     "-c:a", "libopus", "-b:a", "64k", "-f", "ogg", str(path)])
buf = path.read_bytes()
offset = 0
packets = []
current = bytearray()
while offset + 27 <= len(buf) and buf[offset:offset + 4] == b"OggS":
    segments = buf[offset + 27:offset + 27 + buf[offset + 26]]
    data_offset = offset + 27 + buf[offset + 26]
    for segment in segments:
        current += buf[data_offset:data_offset + segment]
        data_offset += segment
        if segment < 255:
            packets.append(bytes(current))
            current = bytearray()
    offset = data_offset
write("opus.bin", [(True, p) for p in packets if not p.startswith((b"OpusHead", b"OpusTags"))])
path.unlink()
