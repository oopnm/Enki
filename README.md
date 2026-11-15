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

Status: Phase 0 (foundations). The camera app arrives in Phase 1; until then the desktop app
still works with scrcpy's own camera mode on Android 12+.
