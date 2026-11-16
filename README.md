# Enki

Use your phone as a **webcam, microphone and screen source** on Linux.
Named after Enki, the Sumerian god of fresh water and wisdom.

- **Camera and microphone:** the Enki Android app (Android 7.0+) streams to the desktop app over the Enki Link protocol (`docs/protocol.md`).
- **Phone screen:** streamed to the computer by [scrcpy](https://github.com/Genymobile/scrcpy).
- **Desktop:** Rust + GTK4, exposes the camera as a virtual `/dev/videoN` device through v4l2loopback.

## Layout

| Path | Contents |
|---|---|
| `desktop/` | Cargo workspace: `enki-gui` (GTK4 app), `enki-protocol` (wire format) |
| `android/` | Android app (Kotlin, minSdk 24) and a pure-JVM `protocol` module |
| `protocol/` | Shared test vectors for both sides |
| `packaging/` | Arch, Fedora, AppImage files (Debian files are in `debian/`) |
| `docs/` | Protocol specification |

## Build

```
cargo build --release -p enki-gui     # binary: target/release/enki
cd android && gradle :app:assembleDebug
```

Status: Phase 2 (desktop receiver). Install the Enki app on the phone (Android 7+), press Start there, then in
the desktop app choose "كاميرا الهاتف (تطبيق Enki)" and press start. The receiver runs `adb forward`, connects,
reconnects by itself when the phone app is restarted, and feeds `/dev/videoN` through ffmpeg
(needs `adb` and `ffmpeg`; the V4L2 device is detected automatically when v4l2loopback was loaded by Enki).

Front/back camera can be chosen before starting and switched while streaming. The picture orientation is set
automatically for each camera (landscape phone, screen facing you) and can be overridden in the desktop app;
with "auto rotate" the phone's orientation sensor decides (landscape/portrait/upside down), and the picture is rotated in software on the desktop;
the output is always 1280x720 (black bars when the picture is rotated to portrait).

Manual testing of the phone side without the GUI:

```
adb forward tcp:28200 tcp:28200      # then tap Start in the Enki app
python3 tools/enki_dump.py | mpv --profile=low-latency --untimed --no-cache -
```

scrcpy remains for screen streaming, and for camera streaming on Android 12+ without the Enki app.
