use super::*;
use std::sync::Mutex;
use tokio::net::TcpListener;

fn frame(stream: u8, kind: Kind, flags: u8, pts: u64, payload: &[u8]) -> Vec<u8> {
    let h = FrameHeader { stream_id: stream, kind, flags, len: payload.len() as u32, pts_us: pts };
    let mut v = h.encode().to_vec();
    v.extend_from_slice(payload);
    v
}

type Received = Arc<Mutex<Vec<String>>>;

/// Fake phone app. Accepts any number of connections. On a control connection it says hello, records every
/// message and confirms `set` with a `state` reply. On a media connection it sends a few frames and then
/// closes, or, with `hold_media`, keeps the connection open until the receiver hangs up.
async fn fake_phone(listener: TcpListener, hold_media: bool, received: Received) {
    loop {
        let Ok((mut s, _)) = listener.accept().await else { return };
        let received = received.clone();
        tokio::spawn(async move {
            let mut pre = [0u8; 8];
            if s.read_exact(&mut pre).await.is_err() {
                return;
            }
            let channel = Preamble::decode(&pre).unwrap().channel;
            let _ = s.write_all(&pre).await;
            match channel {
                Channel::Control => {
                    let _ = write_control(&mut s, r#"{"type":"hello","version":0}"#).await;
                    while let Ok(message) = read_control(&mut s).await {
                        received.lock().unwrap().push(message.clone());
                        if message.contains("\"set\"") {
                            // like the real app: current picture rotation first, then the camera confirmation
                            let _ = write_control(&mut s, r#"{"value":90,"key":"image.rotation","type":"state"}"#).await;
                            let _ = write_control(&mut s, r#"{"type":"state","key":"camera.facing","value":"x"}"#).await;
                        }
                    }
                }
                Channel::Media => {
                    let _ = s.write_all(&frame(0, Kind::VideoConfig, 0, 0, &[0, 0, 1, 0x67])).await;
                    let _ = s.write_all(&frame(0, Kind::VideoKey, 0, 0, &[0, 0, 1, 0x65])).await;
                    let _ = s.write_all(&frame(1, Kind::AudioFrame, 0, 5, &[9, 9, 9])).await; // must be ignored
                    let _ = s.write_all(&frame(0, Kind::VideoDelta, FLAG_DISCONTINUITY, 33_333, &[0x41, 0x9a])).await;
                    // Keep streaming until the receiver hangs up (restart tests need frames to keep arriving).
                    while hold_media {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        if s.write_all(&frame(0, Kind::VideoDelta, 0, 66_666, &[0x41, 0x9a])).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
}

async fn start_phone(hold_media: bool) -> (u16, Received) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let received: Received = Arc::default();
    tokio::spawn(fake_phone(listener, hold_media, received.clone()));
    (port, received)
}

fn test_config(port: u16) -> Config {
    let mut cfg = Config::new("/dev/null");
    cfg.port = port;
    cfg.setup_adb = false;
    cfg
}

fn drain(rx: &mut mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

#[tokio::test]
async fn streams_video_frames_to_the_sink_and_reconnects() {
    let (port, _) = start_phone(false).await;
    let out = std::env::temp_dir().join(format!("enki-receiver-test-{}-{port}.h264", std::process::id()));
    let mut cfg = test_config(port);
    cfg.sink_override = Some(("sh".into(), vec!["-c".into(), format!("cat > {}", out.display())]));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(run(cfg, tx, stop_rx));

    let expected: Vec<u8> = vec![0, 0, 1, 0x67, 0, 0, 1, 0x65, 0x41, 0x9a];
    let mut got = Vec::new();
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        got = std::fs::read(&out).unwrap_or_default();
        if got == expected {
            break;
        }
    }
    assert_eq!(got, expected, "audio frames must be skipped, video concatenated in order");

    let _ = stop_tx.send(());
    task.await.unwrap();
    let _ = std::fs::remove_file(&out);

    let events = drain(&mut rx);
    let states: Vec<State> = events.iter().filter_map(|e| if let Event::State(s) = e { Some(*s) } else { None }).collect();
    assert!(states.contains(&State::Streaming));
    assert_eq!(states.last(), Some(&State::Stopped));
    let logs: String = events.iter().filter_map(|e| if let Event::Log(t) = e { Some(t.as_str()) } else { None }).collect();
    assert!(logs.contains("اتصل بالهاتف"));
}

#[tokio::test]
async fn missing_sink_program_is_a_clear_error() {
    let (port, _) = start_phone(false).await;
    let mut cfg = test_config(port);
    cfg.ffmpeg = "enki-no-such-ffmpeg".into();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_stop_tx, stop_rx) = oneshot::channel();
    tokio::time::timeout(Duration::from_secs(5), run(cfg, tx, stop_rx)).await.expect("run must end by itself");

    let logs: String = drain(&mut rx).into_iter().filter_map(|e| if let Event::Log(t) = e { Some(t) } else { None }).collect();
    assert!(logs.contains("غير مثبت"), "{logs}");
}

#[tokio::test]
async fn phone_not_listening_behind_adb_forward_is_retried_quietly() {
    // A server that accepts and closes immediately, like `adb forward` with no app listening.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let _ = listener.accept().await;
        }
    });

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(run(test_config(port), tx, stop_rx));
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let _ = stop_tx.send(());
    task.await.unwrap();

    let hints = drain(&mut rx).into_iter().filter(|e| matches!(e, Event::Log(t) if t.contains("اضغط Start"))).count();
    assert_eq!(hints, 1, "the hint is shown once, not on every retry");
}

#[tokio::test]
async fn camera_choice_is_sent_before_streaming_and_can_change_live() {
    let (port, received) = start_phone(true).await;
    let mut cfg = test_config(port);
    cfg.facing = Facing::Front;
    cfg.sink_override = Some(("sh".into(), vec!["-c".into(), "cat > /dev/null".into()]));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(run_with_commands(cfg, tx, stop_rx, cmd_rx));

    let mut streaming = false;
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        streaming |= drain(&mut rx).iter().any(|e| matches!(e, Event::State(State::Streaming)));
        if streaming {
            break;
        }
    }
    assert!(streaming, "must reach the streaming state");
    {
        let got = received.lock().unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains(r#""key":"camera.facing""#) && got[0].contains(r#""value":"front""#), "{got:?}");
    }

    cmd_tx.send(Control::SetFacing(Facing::Back)).unwrap();
    let mut switched = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        switched = received.lock().unwrap().iter().any(|m| m.contains(r#""value":"back""#));
        if switched {
            break;
        }
    }
    assert!(switched, "a live camera switch must reach the phone: {:?}", received.lock().unwrap());

    let _ = stop_tx.send(());
    task.await.unwrap();
}

#[test]
fn ffmpeg_args_contain_the_settings_that_fixed_the_frozen_picture() {
    let s = ffmpeg_args("/dev/video10", 30, 0, false).join(" ");
    assert!(s.contains("-framerate 30 -f h264 -i -"));
    assert!(s.contains("-fps_mode passthrough"));
    assert!(s.contains("pad=1280:720"));
    assert!(s.ends_with("-f v4l2 /dev/video10"));
}

const FIT: &str = "scale=1280:720:force_original_aspect_ratio=decrease:force_divisible_by=2,pad=1280:720:(ow-iw)/2:(oh-ih)/2,setsar=1";

#[test]
fn video_filter_rotation_and_mirror_keep_a_fixed_output_size() {
    assert_eq!(video_filter(0, false), FIT);
    assert_eq!(video_filter(180, false), format!("hflip,vflip,{FIT}"));
    assert_eq!(video_filter(90, false), format!("transpose=1,{FIT}"));
    assert_eq!(video_filter(270, true), format!("transpose=2,hflip,{FIT}"));
    assert_eq!(video_filter(0, true), format!("hflip,{FIT}"));
}

fn live(facing: Facing, auto_rotate: bool, phone_rotation: Option<u16>) -> Live {
    Live { facing, auto_rotate, phone_rotation }
}

#[test]
fn default_orientation_per_camera_and_overrides() {
    let mut cfg = Config::new("/dev/null");
    assert_eq!(orientation(&cfg, &live(Facing::Back, false, None)), (180, true));
    assert_eq!(orientation(&cfg, &live(Facing::Front, false, None)), (180, false));
    cfg.rotate = Some(90);
    cfg.mirror = Some(false);
    assert_eq!(orientation(&cfg, &live(Facing::Back, false, None)), (90, false));
    assert_eq!(orientation(&cfg, &live(Facing::Front, false, None)), (90, false));
}

#[test]
fn auto_rotate_uses_the_phone_sensor_and_keeps_the_mirror_default() {
    let cfg = Config::new("/dev/null");
    // sensor value wins when auto-rotate is on and the phone has reported one
    assert_eq!(orientation(&cfg, &live(Facing::Back, true, Some(270))), (270, true));
    assert_eq!(orientation(&cfg, &live(Facing::Front, true, Some(0))), (0, false));
    // no report yet: fall back to the fixed default
    assert_eq!(orientation(&cfg, &live(Facing::Back, true, None)), (180, true));
    // auto-rotate off: the phone's value is ignored
    assert_eq!(orientation(&cfg, &live(Facing::Back, false, Some(270))), (180, true));
}

#[test]
fn rotation_is_parsed_whatever_the_key_order() {
    assert_eq!(parse_rotation(r#"{"type":"state","key":"image.rotation","value":180}"#), Some(180));
    assert_eq!(parse_rotation(r#"{"value":90,"key":"image.rotation","type":"state"}"#), Some(90));
    assert_eq!(parse_rotation(r#"{"type":"state","key":"image.rotation","value": 0}"#), Some(0));
    assert_eq!(parse_rotation(r#"{"type":"state","key":"camera.facing","value":"front"}"#), None);
    assert_eq!(parse_rotation(r#"{"type":"state","key":"image.rotation","value":45}"#), None);
    assert_eq!(parse_rotation(r#"{"type":"state","key":"image.rotation","value":360}"#), None);
}

#[tokio::test]
async fn turning_auto_rotate_on_restarts_the_decoder_with_the_phones_rotation() {
    let (port, _received) = start_phone(true).await; // phone reports rotation 90
    let marker = std::env::temp_dir().join(format!("enki-auto-{}-{port}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let mut cfg = test_config(port);
    cfg.facing = Facing::Front; // fixed default would be 180
    cfg.sink_override = Some((
        "sh".into(),
        vec!["-c".into(), format!("echo started >> {}; cat > /dev/null", marker.display())],
    ));
    let starts = || std::fs::read_to_string(&marker).map(|t| t.lines().count()).unwrap_or(0);

    let (tx, _rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(run_with_commands(cfg, tx, stop_rx, cmd_rx));

    async fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..60 {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }
    assert!(wait_for(|| starts() == 1).await, "first decoder start");
    cmd_tx.send(Control::SetAutoRotate(true)).unwrap(); // 180 -> 90 reported by the phone
    assert!(wait_for(|| starts() == 2).await, "auto-rotate must restart the decoder");
    cmd_tx.send(Control::SetAutoRotate(false)).unwrap(); // and back to 180
    assert!(wait_for(|| starts() == 3).await);

    let _ = stop_tx.send(());
    task.await.unwrap();
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn decoder_restarts_only_when_the_orientation_changes_with_the_camera() {
    let (port, received) = start_phone(true).await;
    let marker = std::env::temp_dir().join(format!("enki-restart-{}-{port}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let mut cfg = test_config(port);
    cfg.facing = Facing::Front;
    cfg.sink_override = Some((
        "sh".into(),
        vec!["-c".into(), format!("echo started >> {}; cat > /dev/null", marker.display())],
    ));
    let starts = || std::fs::read_to_string(&marker).map(|t| t.lines().count()).unwrap_or(0);

    let (tx, _rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(run_with_commands(cfg, tx, stop_rx, cmd_rx));

    async fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..60 {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }
    assert!(wait_for(|| starts() == 1).await, "first decoder start");
    assert!(wait_for(|| received.lock().unwrap().len() == 1).await);

    cmd_tx.send(Control::SetFacing(Facing::Front)).unwrap(); // same camera: nothing to restart
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(starts(), 1, "same orientation must not restart the decoder");

    cmd_tx.send(Control::SetFacing(Facing::Back)).unwrap(); // front (180, no mirror) -> back (180, mirror)
    assert!(wait_for(|| starts() == 2).await, "a different orientation restarts the decoder");

    let _ = stop_tx.send(());
    task.await.unwrap();
    let _ = std::fs::remove_file(&marker);
}

#[test]
fn default_device_prefers_the_enki_loopback() {
    let devs = vec![
        ("/dev/video0".to_string(), "Integrated Camera".to_string()),
        ("/dev/video10".to_string(), "Enki".to_string()),
    ];
    assert_eq!(pick_default_device(&devs).as_deref(), Some("/dev/video10"));
    assert_eq!(pick_default_device(&devs[..1]), None);
}
