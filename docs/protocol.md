# Enki Link — protocol v0

Enki Link connects the Enki Android app to the Linux desktop app. It carries **camera and microphone** streams
and a **control channel**. Screen streaming is **not** part of Enki Link: it is done by scrcpy.

Transport: two TCP connections to the phone (over `adb forward` on USB, or Wi-Fi later).
The phone listens on `127.0.0.1:28200` (loopback only, so nothing else on the network can reach it);
use `adb forward tcp:28200 tcp:28200`. All integers are big-endian.

## Preamble (8 bytes, first bytes of every connection)

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | magic `ENKI` |
| 4 | 1 | version (`0` for this document) |
| 5 | 1 | channel: `0` media, `1` control |
| 6 | 2 | reserved, must be `0` |

The PC connects and sends its preamble first; the phone validates it and answers with its own preamble
for the same channel. A peer that sees a bad magic or an unsupported version closes the connection.
The phone accepts one media connection at a time and runs the camera only while it is open.

## Media channel

After the preamble the phone sends frames: a 16-byte header, then `len` payload bytes.

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | `stream_id` (0 = camera video, 1 = microphone; others reserved) |
| 1 | 1 | `kind`: 0 video config (SPS/PPS), 1 video key frame, 2 video delta frame, 3 audio config, 4 audio frame |
| 2 | 1 | flags: bit 0 = discontinuity (data was dropped before this frame) |
| 3 | 1 | reserved, must be `0` |
| 4 | 4 | `len`, payload bytes (max 16 MiB) |
| 8 | 8 | `pts_us`, presentation time in microseconds |

Video is H.264 in Annex-B form. The phone re-sends the config frame (SPS/PPS) before every key frame so a
receiver can join at any key frame. Test vectors shared by both sides: `protocol/vectors/frame_header.txt`.

## Control channel

After the preamble: a sequence of messages, each a `u32` length followed by that many bytes of UTF-8 JSON
(max 1 MiB). Every message is an object with a `"type"` field.

| Type | Direction | Purpose |
|---|---|---|
| `hello` | both | `{"type":"hello","version":0,"app":"enki-android","app_version":"0.1.0"}` |
| `capabilities` | phone → PC | cameras, sizes, fps ranges, supported controls (ranges and enums) |
| `start` / `stop` | PC → phone | begin or end a stream with the chosen settings |
| `set` | PC → phone | change one setting live: `{"type":"set","key":"camera.zoom","value":2.0}` |
| `state` | phone → PC | current stream state and applied settings |
| `error` | both | `{"type":"error","code":"camera_in_use","message":"..."}` |

The desktop UI is built from `capabilities`; a setting the phone does not report is not shown.
Unknown message types and unknown keys are ignored, so newer peers can talk to older ones.

## Compatibility

`version` changes only for incompatible wire changes. Additive changes (new message types, new settings
keys, new `kind` values behind a capability) keep the same version.
