use super::*;
use tokio::net::TcpListener;

fn frame(stream: u8, kind: Kind, flags: u8, pts: u64, payload: &[u8]) -> Vec<u8> {
    let h = FrameHeader { stream_id: stream, kind, flags, len: payload.len() as u32, pts_us: pts };
    let mut v = h.encode().to_vec();
    v.extend_from_slice(payload);
    v
}

/// Fake phone: answers the handshake, sends a few frames, then closes.
async fn fake_phone(listener: TcpListener) {
    let (mut s, _) = listener.accept().await.unwrap();
    let mut pre = [0u8; 8];
    s.read_exact(&mut pre).await.unwrap();
    assert_eq!(Preamble::decode(&pre).unwrap().channel, Channel::Media);
    s.write_all(&pre).await.unwrap();
    s.write_all(&frame(0, Kind::VideoConfig, 0, 0, &[0, 0, 1, 0x67])).await.unwrap();
    s.write_all(&frame(0, Kind::VideoKey, 0, 0, &[0, 0, 1, 0x65])).await.unwrap();
    s.write_all(&frame(1, Kind::AudioFrame, 0, 5, &[9, 9, 9])).await.unwrap(); // must be ignored
    s.write_all(&frame(0, Kind::VideoDelta, FLAG_DISCONTINUITY, 33_333, &[0x41, 0x9a])).await.unwrap();
}

#[tokio::test]
async fn streams_video_frames_to_the_sink_and_reconnects() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(fake_phone(listener));

    let out = std::env::temp_dir().join(format!("enki-receiver-test-{}-{port}.h264", std::process::id()));
    let mut cfg = Config::new("/dev/null");
    cfg.port = port;
    cfg.setup_adb = false;
    cfg.sink_override = Some(("sh".into(), vec!["-c".into(), format!("cat > {}", out.display())]));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(run(cfg, tx, stop_rx));

    let expected: Vec<u8> = vec![0, 0, 1, 0x67, 0, 0, 1, 0x65, 0x41, 0x9a];
    let mut got = Vec::new();
    for _ in 0..50 {
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

    let mut states = Vec::new();
    let mut logs = String::new();
    while let Ok(ev) = rx.try_recv() {
        match ev {
            Event::State(s) => states.push(s),
            Event::Log(t) => logs.push_str(&t),
            Event::Stats(_) => {}
        }
    }
    assert!(states.contains(&State::Streaming));
    assert_eq!(states.last(), Some(&State::Stopped));
    assert!(logs.contains("اتصل بالهاتف"));
}

#[tokio::test]
async fn missing_sink_program_is_a_clear_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(fake_phone(listener));

    let mut cfg = Config::new("/dev/null");
    cfg.port = port;
    cfg.setup_adb = false;
    cfg.ffmpeg = "enki-no-such-ffmpeg".into();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_stop_tx, stop_rx) = oneshot::channel();
    tokio::time::timeout(Duration::from_secs(5), run(cfg, tx, stop_rx)).await.expect("run must end by itself");

    let mut logs = String::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::Log(t) = ev {
            logs.push_str(&t);
        }
    }
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
    let mut cfg = Config::new("/dev/null");
    cfg.port = port;
    cfg.setup_adb = false;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(run(cfg, tx, stop_rx));
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let _ = stop_tx.send(());
    task.await.unwrap();

    let hints = std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|e| matches!(e, Event::Log(t) if t.contains("اضغط Start")))
        .count();
    assert_eq!(hints, 1, "the hint is shown once, not on every retry");
}

#[test]
fn ffmpeg_args_contain_the_settings_that_fixed_the_frozen_picture() {
    let a = ffmpeg_args("/dev/video10", 30);
    let s = a.join(" ");
    assert!(s.contains("-framerate 30 -f h264 -i -"));
    assert!(s.contains("-fps_mode passthrough"));
    assert!(s.ends_with("-f v4l2 /dev/video10"));
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
