#!/bin/bash
cd /home/devel/workspace/varphone/rtflow

pkill -f dash_server 2>/dev/null
pkill -f "ffmpeg.*8554" 2>/dev/null
sleep 1

# Start server
RUST_LOG=debug cargo run --example dash_server --features "dash,server,http" -- --codecs h264 --http-listen 0.0.0.0:38080 > /tmp/dash_server.log 2>&1 &
SERVER_PID=$!
sleep 4

# Start ffmpeg
ffmpeg -re -stream_loop -1 -i tests/fixtures/h264-aac.mp4 -map 0:v -c copy -f rtsp rtsp://127.0.0.1:8554/live/stream > /tmp/ffmpeg_push.log 2>&1 &
FFMPEG_PID=$!
sleep 12

# Download
curl -s -o /tmp/init.mp4 http://127.0.0.1:38080/live/stream/init.mp4
curl -s -o /tmp/seg0.m4s http://127.0.0.1:38080/live/stream/segment_0.m4s
curl -s -o /tmp/seg1.m4s http://127.0.0.1:38080/live/stream/segment_1.m4s

echo "=== Sizes ==="
ls -la /tmp/init.mp4 /tmp/seg0.m4s /tmp/seg1.m4s 2>&1

echo ""
echo "=== Combined (init + seg0) ffprobe verbose ==="
cat /tmp/init.mp4 /tmp/seg0.m4s > /tmp/combined0.mp4
ffprobe -v error -show_streams /tmp/combined0.mp4 2>&1 | head -25

echo ""
echo "=== Try decoding with ffmpeg ==="
ffmpeg -v error -i /tmp/combined0.mp4 -f null - 2>&1 | head -10
echo "Exit code: $?"

echo ""
echo "=== Combined (init + seg1) decode ==="
cat /tmp/init.mp4 /tmp/seg1.m4s > /tmp/combined1.mp4
ffmpeg -v error -i /tmp/combined1.mp4 -f null - 2>&1 | head -10
echo "Exit code: $?"

echo ""
echo "=== Multi-segment decode (init + seg0 + seg1) ==="
cat /tmp/init.mp4 /tmp/seg0.m4s /tmp/seg1.m4s > /tmp/combined_multi.mp4
ffmpeg -v error -i /tmp/combined_multi.mp4 -f null - 2>&1 | head -10
echo "Exit code: $?"

echo ""
echo "=== avcC hex dump ==="
python3 -c "
with open('/tmp/init.mp4', 'rb') as f:
    data = f.read()
pos = data.find(b'avcC')
if pos >= 0:
    # avcC box: 4 bytes size before 'avcC', then payload
    size_pos = pos - 4
    import struct
    box_size = struct.unpack('>I', data[size_pos:size_pos+4])[0]
    print(f'avcC box at offset {size_pos}, size={box_size}')
    payload = data[pos+4:size_pos+box_size]
    print(f'Payload ({len(payload)} bytes): {payload.hex()}')
    print(f'  configVersion: {payload[0]}')
    print(f'  profile: {payload[1]:#04x}')
    print(f'  compat: {payload[2]:#04x}')
    print(f'  level: {payload[3]:#04x}')
    print(f'  lengthSizeMinusOne: {payload[4] & 0x03}')
    num_sps = payload[5] & 0x1f
    print(f'  numSPS: {num_sps}')
    if num_sps > 0:
        sps_len = struct.unpack('>H', payload[6:8])[0]
        sps = payload[8:8+sps_len]
        print(f'  SPS ({sps_len} bytes): {sps.hex()}')
        pps_offset = 8 + sps_len
        num_pps = payload[pps_offset]
        print(f'  numPPS: {num_pps}')
        if num_pps > 0:
            pps_len = struct.unpack('>H', payload[pps_offset+1:pps_offset+3])[0]
            pps = payload[pps_offset+3:pps_offset+3+pps_len]
            print(f'  PPS ({pps_len} bytes): {pps.hex()}')
"

echo ""
echo "=== First sample data from seg0 ==="
python3 -c "
import struct
with open('/tmp/seg0.m4s', 'rb') as f:
    data = f.read()
# Find moof
moof_pos = data.find(b'moof') - 4
moof_size = struct.unpack('>I', data[moof_pos:moof_pos+4])[0]
# mdat starts after moof
mdat_pos = moof_pos + moof_size
mdat_size = struct.unpack('>I', data[mdat_pos:mdat_pos+4])[0]
print(f'moof at {moof_pos}, size={moof_size}')
print(f'mdat at {mdat_pos}, size={mdat_size}')
# First sample starts at mdat_pos + 8
sample_start = mdat_pos + 8
# Read first NAL length
nal_len = struct.unpack('>I', data[sample_start:sample_start+4])[0]
nal_type = data[sample_start+4] & 0x1f
print(f'First NAL: len={nal_len}, type={nal_type} (7=SPS,8=PPS,5=IDR,1=non-IDR)')
# Show first 32 bytes of sample data
print(f'First 32 bytes of mdat payload: {data[sample_start:sample_start+32].hex()}')
"

kill $FFMPEG_PID 2>/dev/null
kill $SERVER_PID 2>/dev/null
echo ""
echo "=== DONE ==="
