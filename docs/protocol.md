# Enki Link — protocol v1

Enki Link connects the Enki Android app to the Linux desktop app. It carries **camera and microphone** streams
and a **control channel**. Screen streaming is **not** part of Enki Link: it is done by scrcpy.

Transport: two TCP connections to the phone (over `adb forward` on USB, or Wi-Fi later).
The phone listens on `127.0.0.1:28200` (loopback only, so nothing else on the network can reach it);
use `adb forward tcp:28200 tcp:28200`. All integers are big-endian.

## Preamble (8 bytes, first bytes of every connection)

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | magic `ENKI` |
| 4 | 1 | version (`1` for this document) |
| 5 | 1 | channel: `0` media, `1` control |
| 6 | 2 | reserved, must be `0` |

The PC connects and sends its preamble first; the phone validates it and answers with its own preamble
for the same channel. A peer that sees a bad magic or an unsupported version closes the connection. A phone
that does not support the PC's version still answers with its own preamble before closing, so the PC can say
which versions differ instead of just waiting. The version-0 handshake (no authentication) is no longer accepted.
The phone accepts one media connection at a time and runs the camera only while it is open.

## Authentication

The phone listens on the loopback interface, which every app on the phone can reach, so a connection must prove
it comes from the PC before the phone serves anything (camera, control, or the single media slot).

**Session token.** The PC makes 32 random bytes per session and gives them to the phone app through adb:

```
adb shell content call --uri content://io.github.oopnm.enki.provision --method provision --arg <64 hex digits>
```

The phone's content provider is exported so adb can reach it, but it refuses every caller except the adb shell
(uid 2000) and root. The calling uid is set by the kernel, so another app cannot pretend to be adb and cannot learn
or set the token. It replies `Result: Bundle[{ok=true}]`. The token lives in the phone app's memory only and is
dropped when the stream is stopped or the app process ends.

**Challenge and answer.** After the two preambles, on every connection (media and control):

| Step | Direction | Bytes |
|---|---|---|
| 1 | phone → PC | `nonce`, 16 random bytes |
| 2 | PC → phone | `mac` = HMAC-SHA256(key = token, message = `"ENKI-AUTH-1"` ‖ channel byte ‖ nonce), 32 bytes |
| 3 | phone → PC | verdict, 1 byte: `0` ok, `1` bad token, `2` phone has no token |

The phone compares the answer in constant time. On a verdict other than `0` it closes the connection. The channel
byte is part of the message, so an answer cannot be replayed on the other channel, and the fresh nonce stops replay
across connections. The token itself never crosses the Enki Link socket. Test vectors shared by both sides:
`protocol/vectors/auth.txt`.

A PC that gets verdict `1` or `2` after it has provisioned in an earlier connection gives the phone the token again
once (the app was probably restarted and lost it) and reconnects; a refusal right after provisioning is final and is
shown to the user. Handshake steps time out after 5 s on the phone and 3 s on the PC.

Threat model: this stops other apps on the phone from reading the camera or taking the single media slot. It does not
hide the stream from someone who controls adb or root on the phone or the PC. Wi-Fi transport will add TLS (phase 8).

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
| `hello` | phone → PC | First message: `{"type":"hello","version":1,"app":"enki-android","app_version":"0.2.0","device":"Pixel 6","android":28}` |
| `capabilities` | phone → PC | Sent right after `hello`: what each camera can do (below) |
| `ping` / `pong` | PC → phone / phone → PC | Heartbeat, `{"type":"ping","seq":7}` answered by `{"type":"pong","seq":7}` |
| `set` | PC → phone | Change one setting live: `{"type":"set","key":"camera.facing","value":"front"}` |
| `state` | phone → PC | A setting's current value: `{"type":"state","key":"image.rotation","value":180}` |
| `error` | phone → PC | `{"type":"error","code":"bad_value","message":"..."}` |

The exact JSON of each message is in `protocol/vectors/control.txt`; the Rust and the Android tests both check it.

### Capabilities

```json
{"type":"capabilities","cameras":[{"id":"0","facing":"back","sensor_orientation":90,
 "sizes":[[1920,1080],[1280,720]],"fps_ranges":[[15,30],[30,30]],"zoom_max":8.0,"flash":true,
 "focus_modes":["continuous","auto"],"ev_range":[-4,4],"ev_step":0.5}],"codecs":["h264","hevc"]}
```

`sizes` are video sizes the camera can feed the encoder, largest first (at most 12, none above 3840x2160).
`focus_modes` is a subset of `continuous`, `auto`, `manual`; `ev_range` is in steps of `ev_step` EV. Every field is
optional for the reader (missing ones take empty or zero values) and unknown fields are ignored. The desktop UI offers
only what the phone reports.

### Heartbeat and lost links

The PC sends a `ping` once a second. The phone answers at once with a `pong` carrying the same `seq`, which gives the
PC a round-trip time. The PC treats the link as lost when the phone has sent nothing for 3 s; the phone does the same
when it has received nothing for 5 s, and then closes the media connection so the camera stops. TCP over `adb forward`
can stay open for minutes after the cable is pulled, so this is what makes unplug and replug recover by itself: the
PC then reconnects and hands the phone a token again if the phone app lost it.

### Bit rate

The phone watches its send queue. When the PC or the cable cannot keep up and frames are dropped, it lowers the
encoder's bit rate by 30 % (never below 0.5 Mbit/s) and raises it by 15 % after 5 quiet seconds, up to the 4 Mbit/s
target. It reports the current target as `{"type":"state","key":"stream.bitrate","value":2800}` (kbit/s), once when
streaming starts and after every change.

### Settings implemented so far

| Key | Values | Behavior |
|---|---|---|
| `camera.facing` | `"back"`, `"front"` | Chooses the camera. The phone remembers it and uses it for the next media connection; if a media stream is running it switches camera at once (a new config + key frame follows). The phone confirms with `{"type":"state","key":"camera.facing","value":"front"}` (after an `image.rotation` state), or answers `{"type":"error","code":"bad_value",...}`. |
| `image.rotation` | `0`, `90`, `180`, `270` (phone → PC, `state` only) | Clockwise rotation that makes the picture upright, computed on the phone from the camera's sensor orientation and the phone's accelerometer (`OrientationEventListener`, independent of the screen-rotation lock; 15° hysteresis). Sent after every `camera.facing` set and whenever the phone is turned. The PC applies it when "auto rotate" is on. |
| `stream.bitrate` | kbit/s (phone → PC, `state` only) | The encoder's current target bit rate, see above. |

The PC opens the control connection first, reads `hello` (and `capabilities`), sends `camera.facing`, waits up to 1 s
for the `state` reply, and only then opens the media connection.

Unknown message types and unknown keys are ignored, so newer peers can talk to older ones.

## Compatibility

`version` changes only for incompatible wire changes (version 1 added the authentication exchange). Additive
changes (new message types, new settings keys, new `kind` values behind a capability) keep the same version.
The PC program and the phone app must be from the same protocol version; both report a clear message when they are not.
