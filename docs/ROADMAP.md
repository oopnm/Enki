# Enki roadmap

Enki turns an Android phone into a Linux webcam, microphone and screen source.
This file is the plan. Phases are ordered so that nothing is built twice: the
protocol comes first, then the settings engine and translations, then the
screens, then the look.

| Phase | Goal | Status |
|---|---|---|
| 0 | Project skeleton, packaging, scrcpy screen modes | done |
| 1 | Phone camera (Camera2 → H.264 → TCP), desktop receiver, auto rotate | done (PR #1) |
| 2 | Release 0.2.0 (packaging dependencies, version bump) | done |
| 3 | Enki-Link v2: best possible communication | done, tested (PR #3) |
| 4 | Unified settings engine, i18n skeleton, design tokens | done, tested (PR #4) |
| 5 | DroidCam-class camera settings and live preview | done, tested (PR #5) |
| 6 | "Abzu" theme, wave animation, full translations | done, tested (PR #6) |
| 7 | Wi-Fi, pairing and encryption (moved before audio, 2026-11-23) | built, waiting for his phone test |
| 8 | Microphone and audio | planned |
| 9 | Polish and 1.0 | planned |

Phases 7 and 8 can swap places.

## Phase 3: Enki-Link v2

- Two separate channels: a small control channel (length-prefixed JSON with a
  `proto_version` field) and a video channel whose header carries a sequence
  number, a PTS and a key-frame flag. A large video frame must never delay a
  control message, and the PTS is needed later to line audio up with video.
- Handshake: versions and identity, then the phone's capabilities (cameras,
  resolutions, zoom, focus, flash, stabilisation), then the settings state.
- Security: the phone's `LinkServer` listens on `127.0.0.1:28200`, so any other
  app on the phone can connect to it. Use a random per-session token, passed by
  the desktop through adb, and refuse any connection without it.
- Explicit state machine: disconnected, device found, port forwarded,
  handshake, ready, streaming, degraded / reconnecting.
- Robustness: 1 s heartbeat with a 3 s timeout; automatic reconnect with
  back-off that survives cable replug, adb restart and the phone app being
  closed; specific error messages with a fix button (missing v4l2loopback,
  adb not authorised, and so on).
- Quality: measure RTT and lost frames; lower the bit rate automatically when
  the link is congested.
- Testing without a phone: a fake phone written in Rust that speaks the
  protocol, plus golden test vectors in `protocol/vectors` used by both the Rust
  and Kotlin sides.

Steps, all done in the same branch:

- 3a authentication (protocol v1, session token through adb, challenge on every connection).
- 3b typed control messages with shared JSON vectors (`protocol/vectors/control.txt`), and `enki-fakephone`, a fake
  phone usable from tests and by hand (`enki-fakephone --accept-any-token` with `ENKI_NO_ADB=1 enki` runs the
  whole desktop side without a phone; it streams an ffmpeg test pattern).
- 3c capabilities message from Camera2 and MediaCodec (the desktop receives it; the settings screens that use it
  come in phase 5).
- 3d link states (preparing, waiting for the app, ready, streaming, reconnecting), heartbeat with round-trip time,
  lost-link detection on both sides, automatic bit-rate control on the phone.
- 3e problems with codes in the receiver and a fix button in the GUI (load the kernel module, or try again).

Decision: the video header keeps its 16 bytes (stream, kind, flags, length, PTS). A sequence number would not add
anything: a frame the phone drops is reported with the discontinuity flag, and TCP does not reorder or lose bytes.

Done when: unplugging and replugging the cable during a stream recovers by
itself, and another app on the phone cannot read the stream.

## Phase 4: Unified settings and languages

- One schema file describes every setting: key, type, range, default and scope
  (synced or local to one device). Rust and Kotlin code is generated from it,
  and a CI check fails if a setting has no control in either app. This is what
  guarantees that both apps show the same settings.
- Two-way sync: every key carries a version and the id of the device that wrote
  it; the newest write wins (hybrid logical clock for edits made while
  disconnected). Only differences are sent on connect, a change is never echoed
  back to its sender, and every change is answered with accept or reject plus a
  reason so the UI always shows the truth.
- Live settings (zoom, exposure, flash, mirror) apply at once. Settings that
  restart the encoder (resolution, FPS, codec) roll back automatically if the
  restart fails.
- A profile per paired phone; its capabilities are cached so the right options
  show even before the phone connects.
- Storage: TOML under `~/.config/enki` on the desktop, DataStore on Android.
- Languages (skeleton only): Fluent on the desktop, `strings.xml` on Android, a
  CI check that both have the same keys, full right-to-left support for Arabic,
  UI language is a local setting that is not synced by default.
- Design tokens (colours, radii, spacing) in one shared file.

## Phase 5: DroidCam-class settings

The same settings on both apps, grouped as follows.

- Video: camera (front, back, wide, tele as the phone offers), resolution
  480p / 720p / 1080p, FPS, bit rate (auto or manual), H.264 or H.265,
  orientation (fixed or sensor-driven, already done), mirror, flip, zoom,
  focus (continuous, tap, manual, lock), exposure compensation, ISO and
  exposure time, white balance, flash, stabilisation, noise reduction, and a
  50/60 Hz anti-flicker option.
- Desktop-side filters (ffmpeg): brightness, contrast, saturation, sharpness,
  digital crop.
- Phone behaviour: keep screen on, black-screen mode to save battery (tested on
  Android 9, background camera rules differ between versions), service
  notification with a stop button, battery and temperature readout, quality
  reduction on thermal throttling, start streaming automatically on connect.
- Live preview inside the desktop app from a small second output (320x180,
  10 fps) of the same ffmpeg process.

## Phase 6: Abzu theme

Enki is the god of fresh water and wisdom. From the Abzu, the fresh-water ocean
below the earth, the two rivers flow out. The look follows from that.

| Motif | In the interface |
|---|---|
| The Abzu (deep water) | Background gradient from near black to deep blue with a faint watery glow |
| The two rivers | Two flowing lines that meet in the connection indicator: computer + phone = one river when linked |
| Waves | Translucent wave layers along the bottom of the window; calm when idle, active when streaming; later driven by the real microphone level |
| Wisdom and writing | Abstract cuneiform-style marks, faint, as dividers (shapes, not text) |
| Craft | Precise small motions, soft rounded corners, a light inner glow on cards |
| The goat-fish (suhur-mash) | Original logo as a cylinder seal |

Starting colours (tuned on screen): near black `#04080F` to `#07111F`, deep blue
`#0A1F3A`, lapis `#26619C`, aqua `#3FD0D4` for active state, foam `#BFEFFF`, and
muted gold `#E0B25A` used very sparingly, for a successful pairing only.

Motion: Cairo drawing with a precomputed sine table on the desktop, capped at
30 fps, paused when the window is minimised or unfocused, target under 1% CPU
when idle. On Android, Canvas with an infinite transition, paused in black-screen
mode. Both honour "reduce motion".

Languages: English, Arabic, Simplified Chinese, then Turkish, Persian, Sorani
Kurdish, Spanish, French, Russian, German, Portuguese, Japanese, Hindi. First
translations are machine-assisted and marked "needs review". CJK fonts are not
bundled; they are an optional dependency of the packages.

What was built (all in one branch):

- Shared tokens (`protocol/tokens.json`) now also generate `tokens.rs`, the colors as Cairo floats.
- Desktop: `theme.css` (gradient window, glowing cards, aqua buttons, sliders, drop-downs, log), `abzu.rs` (waves,
  the two rivers, cuneiform-style dividers, one timer capped at 30 fps and 15 fps when calm, stopped when the window
  is not focused or motion is reduced), the seal logo in the header.
- Android: `Theme.Enki`, the same cards, `WaveView`, `RiverView`, `CuneiformView` and one clock (`AbzuMotion`) that
  stops in black-screen mode, when the screen is not in front, and with "reduce motion".
- New local setting `ui.reduce_motion` (also follows the desktop's / phone's own "no animations").
- Logo: a cylinder seal with an original goat-fish, `packaging/icons/enki.svg`; PNGs for the desktop, AppImage and
  the Android launcher come from it.
- 13 languages (see `docs/LANGUAGES.md`); the ten new ones come from `tools/translations/*.ftl` through
  `tools/gen_translations.py`; CI checks that every language has every text and that generated files are current.

Done when: both apps look and read the same, Arabic RTL has no broken widget,
and idle drawing stays under the CPU target (check: `top -p $(pgrep -x enki)` with the window focused and idle,
expected under 1%).

## Phase 7: Wi-Fi and pairing

- mDNS discovery and pairing with a one-time code typed on the phone (or over
  USB). A QR code shown by the PC is a later add-on.
- TLS with a pinned phone certificate plus a paired secret, so nobody on the
  network can see or use the camera (see `docs/protocol.md`, "Wi-Fi").
- USB, Wi-Fi and hotspot all use the same protocol from phase 3.

## Phase 8: Audio

- Phone microphone over an audio channel with timestamps shared with video.
- Shows up on the computer as a real microphone through a virtual PipeWire /
  PulseAudio source.
- Synced settings: mute, gain, noise suppression, echo cancellation, source.

## Phase 9: Polish and 1.0

- Review the Arch, Fedora, Debian and AppImage packages.
- "Export diagnostics" (logs, versions, capabilities; no personal data).
- Performance gates: latency, CPU and battery.
- User and contributor documentation.

## Working rules

- Each phase is split into small pull requests, the cheapest and most
  foundational first (settings engine and fake phone before screens).
- Every phase has a stated completion test, checked on a real Android 9 phone,
  and everything else runs in CI against the fake phone.
