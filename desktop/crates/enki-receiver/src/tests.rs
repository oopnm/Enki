use super::*;
use tokio::net::TcpListener;

use enki_fakephone::{Behavior, Received};

/// Token the fake phone accepts unless a test says otherwise; `test_config` uses the same one.
const TEST_TOKEN: [u8; TOKEN_LEN] = [42; TOKEN_LEN];

/// How the fake phone behaves at the handshake.
#[derive(Clone)]
struct Phone {
    /// Protocol version it answers with (a different one simulates an old app).
    version: u8,
    /// Token it holds. `None` = never provisioned.
    token: Option<[u8; TOKEN_LEN]>,
    /// Provisioning log written by the fake adb: the phone holds `TEST_TOKEN` once this file has at least
    /// as many lines containing "content call" as the counter says (simulates receiving the token through adb).
    adb_log: Option<(std::path::PathBuf, Arc<std::sync::atomic::AtomicUsize>)>,
}

impl Phone {
    fn normal() -> Self {
        Phone {
            version: enki_protocol::VERSION,
            token: Some(TEST_TOKEN),
            adb_log: None,
        }
    }

    fn current_token(&self) -> Option<[u8; TOKEN_LEN]> {
        if let Some((log, needed)) = &self.adb_log {
            let text = std::fs::read_to_string(log).unwrap_or_default();
            let given = text.lines().filter(|l| l.contains("content call")).count();
            return (given >= needed.load(Ordering::Relaxed)).then_some(TEST_TOKEN);
        }
        self.token
    }
}

async fn start_phone_as(hold_media: bool, phone: Phone) -> (u16, Received) {
    let mut behavior = Behavior::new(TEST_TOKEN);
    behavior.hold_media = hold_media;
    behavior.version = phone.version;
    behavior.token = Arc::new(move || phone.current_token());
    enki_fakephone::spawn(behavior).await
}

async fn start_phone(hold_media: bool) -> (u16, Received) {
    start_phone_as(hold_media, Phone::normal()).await
}

fn test_config(port: u16) -> Config {
    let mut cfg = Config::new("/dev/null");
    cfg.port = port;
    cfg.token = TEST_TOKEN;
    cfg.setup_adb = false;
    cfg
}

fn drain(rx: &mut mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

#[tokio::test]
async fn streams_video_frames_to_the_sink_and_reconnects() {
    let (port, _) = start_phone(false).await;
    let out = std::env::temp_dir().join(format!(
        "enki-receiver-test-{}-{port}.h264",
        std::process::id()
    ));
    let mut cfg = test_config(port);
    cfg.sink_override = Some((
        "sh".into(),
        vec!["-c".into(), format!("cat > {}", out.display())],
    ));

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
    assert_eq!(
        got, expected,
        "audio frames must be skipped, video concatenated in order"
    );

    let _ = stop_tx.send(());
    task.await.unwrap();
    let _ = std::fs::remove_file(&out);

    let events = drain(&mut rx);
    let states: Vec<State> = events
        .iter()
        .filter_map(|e| {
            if let Event::State(s) = e {
                Some(*s)
            } else {
                None
            }
        })
        .collect();
    assert!(states.contains(&State::Streaming));
    assert_eq!(states.last(), Some(&State::Stopped));
    let logs: String = events
        .iter()
        .filter_map(|e| {
            if let Event::Log(t) = e {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(logs.contains("اتصل بالهاتف"));
}

#[tokio::test]
async fn missing_sink_program_is_a_clear_error() {
    let (port, _) = start_phone(false).await;
    let mut cfg = test_config(port);
    cfg.ffmpeg = "enki-no-such-ffmpeg".into();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_stop_tx, stop_rx) = oneshot::channel();
    tokio::time::timeout(Duration::from_secs(5), run(cfg, tx, stop_rx))
        .await
        .expect("run must end by itself");

    let logs: String = drain(&mut rx)
        .into_iter()
        .filter_map(|e| if let Event::Log(t) = e { Some(t) } else { None })
        .collect();
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

    let hints = drain(&mut rx)
        .into_iter()
        .filter(|e| matches!(e, Event::Log(t) if t.contains("اضغط Start")))
        .count();
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
        streaming |= drain(&mut rx)
            .iter()
            .any(|e| matches!(e, Event::State(State::Streaming)));
        if streaming {
            break;
        }
    }
    assert!(streaming, "must reach the streaming state");
    {
        let got = received.lock().unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            got[0].contains(r#""key":"camera.facing""#) && got[0].contains(r#""value":"front""#),
            "{got:?}"
        );
    }

    cmd_tx.send(Control::SetFacing(Facing::Back)).unwrap();
    let mut switched = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        switched = received
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.contains(r#""value":"back""#));
        if switched {
            break;
        }
    }
    assert!(
        switched,
        "a live camera switch must reach the phone: {:?}",
        received.lock().unwrap()
    );

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
    Live {
        facing,
        auto_rotate,
        phone_rotation,
        ..Live::default()
    }
}

#[test]
fn default_orientation_per_camera_and_overrides() {
    assert_eq!(orientation(&live(Facing::Back, false, None)), (180, true));
    assert_eq!(orientation(&live(Facing::Front, false, None)), (180, false));
    let fixed = Live {
        rotate: Some(90),
        mirror: Some(false),
        ..live(Facing::Back, false, None)
    };
    assert_eq!(orientation(&fixed), (90, false));
    assert_eq!(
        orientation(&Live {
            facing: Facing::Front,
            ..fixed
        }),
        (90, false)
    );
}

#[test]
fn auto_rotate_uses_the_phone_sensor_and_keeps_the_mirror_default() {
    // sensor value wins when auto-rotate is on and the phone has reported one
    assert_eq!(
        orientation(&live(Facing::Back, true, Some(270))),
        (270, true)
    );
    assert_eq!(orientation(&live(Facing::Front, true, Some(0))), (0, false));
    // no report yet: fall back to the fixed default
    assert_eq!(orientation(&live(Facing::Back, true, None)), (180, true));
    // auto-rotate off: the phone's value is ignored
    assert_eq!(
        orientation(&live(Facing::Back, false, Some(270))),
        (180, true)
    );
}

fn state(key: &str, value: enki_protocol::control::Value) -> Message {
    Message::State {
        key: key.into(),
        value,
    }
}

#[test]
fn rotation_bitrate_and_pong_messages_update_the_shared_state() {
    let shared = Shared::new(
        live(Facing::Back, false, None),
        Hub::in_memory("pc").handle(),
    );
    let (tx, _rx) = mpsc::unbounded_channel();
    let _ = handle_message(&shared, &tx, state("image.rotation", 180.into()));
    assert_eq!(shared.live.borrow().phone_rotation, Some(180));
    let _ = handle_message(&shared, &tx, state("image.rotation", 45.into())); // not a right angle: ignored
    let _ = handle_message(&shared, &tx, state("image.rotation", "90".into())); // wrong type: ignored
    assert_eq!(shared.live.borrow().phone_rotation, Some(180));
    let _ = handle_message(&shared, &tx, state("stream.bitrate", 2500.into()));
    assert_eq!(shared.bitrate_kbps.load(Ordering::Relaxed), 2500);

    let seq = shared.next_ping();
    let _ = handle_message(&shared, &tx, Message::Pong { seq: seq + 1 }); // someone else's answer: no round trip
    assert_eq!(shared.rtt_ms.load(Ordering::Relaxed), 0);
    let _ = handle_message(&shared, &tx, Message::Pong { seq });
    assert!(shared.rtt_ms.load(Ordering::Relaxed) >= 1);
    shared.reset_link();
    assert_eq!(
        shared.rtt_ms.load(Ordering::Relaxed),
        0,
        "a new connection forgets the old measurements"
    );
    assert_eq!(shared.live.borrow().phone_rotation, None);
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
        vec![
            "-c".into(),
            format!("echo started >> {}; cat > /dev/null", marker.display()),
        ],
    ));
    let starts = || {
        std::fs::read_to_string(&marker)
            .map(|t| t.lines().count())
            .unwrap_or(0)
    };

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
    assert!(
        wait_for(|| starts() == 2).await,
        "auto-rotate must restart the decoder"
    );
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
        vec![
            "-c".into(),
            format!("echo started >> {}; cat > /dev/null", marker.display()),
        ],
    ));
    let starts = || {
        std::fs::read_to_string(&marker)
            .map(|t| t.lines().count())
            .unwrap_or(0)
    };

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
    assert!(
        wait_for(|| starts() == 2).await,
        "a different orientation restarts the decoder"
    );

    let _ = stop_tx.send(());
    task.await.unwrap();
    let _ = std::fs::remove_file(&marker);
}

/// Runs a session that must end by itself with an error; returns everything it logged and the problem it reported.
async fn run_to_failure_with_problem(cfg: Config) -> (String, Option<Problem>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_stop_tx, stop_rx) = oneshot::channel();
    tokio::time::timeout(Duration::from_secs(10), run(cfg, tx, stop_rx))
        .await
        .expect("run must end by itself");
    let mut logs = String::new();
    let mut problem = None;
    for e in drain(&mut rx) {
        match e {
            Event::Log(t) => logs.push_str(&t),
            Event::Problem(p) => problem = Some(p),
            _ => {}
        }
    }
    (logs, problem)
}

async fn run_to_failure(cfg: Config) -> String {
    run_to_failure_with_problem(cfg).await.0
}

#[tokio::test]
async fn a_phone_with_another_token_is_refused_with_a_clear_message() {
    let phone = Phone {
        token: Some([1; TOKEN_LEN]),
        ..Phone::normal()
    };
    let (port, _) = start_phone_as(false, phone).await;
    let (logs, problem) = run_to_failure_with_problem(test_config(port)).await;
    assert!(logs.contains("رفض مفتاح الجلسة"), "{logs}");
    assert_eq!(problem.map(|p| p.code), Some(ProblemCode::AuthRejected));
}

#[tokio::test]
async fn a_phone_that_was_never_provisioned_says_so() {
    let phone = Phone {
        token: None,
        ..Phone::normal()
    };
    let (port, _) = start_phone_as(false, phone).await;
    let logs = run_to_failure(test_config(port)).await;
    assert!(logs.contains("لم يستلم مفتاح الجلسة"), "{logs}");
}

#[tokio::test]
async fn an_old_phone_app_is_reported_instead_of_waiting_forever() {
    let phone = Phone {
        version: 0,
        ..Phone::normal()
    };
    let (port, _) = start_phone_as(false, phone).await;
    let (logs, problem) = run_to_failure_with_problem(test_config(port)).await;
    assert!(
        logs.contains("بروتوكولاً مختلفاً") && logs.contains("النسخة 0"),
        "{logs}"
    );
    assert_eq!(problem.map(|p| p.code), Some(ProblemCode::VersionMismatch));
}

/// A stand-in for `adb`: records its arguments, accepts `forward`, and answers `shell content call` like the
/// provider does.
fn fake_adb(dir: &std::path::Path) -> (String, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("adb.log");
    let script = dir.join("adb");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho \"$@\" >> {}\nif [ \"$1\" = shell ]; then echo 'Result: Bundle[{{ok=true}}]'; fi\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script.display().to_string(), log)
}

#[tokio::test]
async fn the_token_goes_to_the_phone_through_adb_and_streaming_starts() {
    let dir = std::env::temp_dir().join(format!("enki-adb-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (adb, log) = fake_adb(&dir);
    let needed = Arc::new(std::sync::atomic::AtomicUsize::new(1));
    let phone = Phone {
        adb_log: Some((log.clone(), needed)),
        ..Phone::normal()
    };
    let (port, _) = start_phone_as(true, phone).await;
    let mut cfg = test_config(port);
    cfg.setup_adb = true;
    cfg.adb = adb;
    cfg.sink_override = Some(("sh".into(), vec!["-c".into(), "cat > /dev/null".into()]));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(run(cfg, tx, stop_rx));
    let mut streaming = false;
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        streaming |= drain(&mut rx)
            .iter()
            .any(|e| matches!(e, Event::State(State::Streaming)));
        if streaming {
            break;
        }
    }
    let _ = stop_tx.send(());
    task.await.unwrap();
    assert!(streaming, "must stream once the phone has the token");

    let text = std::fs::read_to_string(&log).unwrap();
    let call = text
        .lines()
        .find(|l| l.contains("content call"))
        .expect("provisioning call");
    assert_eq!(
        call,
        format!(
            "shell content call --uri content://io.github.oopnm.enki.provision --method provision --arg {}",
            to_hex(&TEST_TOKEN)
        )
    );
    assert!(text.lines().any(|l| l.starts_with("forward tcp:")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_phone_that_restarted_and_lost_the_token_is_provisioned_again() {
    let dir = std::env::temp_dir().join(format!("enki-adb-again-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (adb, log) = fake_adb(&dir);
    let needed = Arc::new(std::sync::atomic::AtomicUsize::new(1));
    let phone = Phone {
        adb_log: Some((log.clone(), needed.clone())),
        ..Phone::normal()
    };
    let (port, _) = start_phone_as(false, phone).await; // media closes by itself, so the receiver reconnects
    let mut cfg = test_config(port);
    cfg.setup_adb = true;
    cfg.adb = adb;
    cfg.sink_override = Some(("sh".into(), vec!["-c".into(), "cat > /dev/null".into()]));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(run(cfg, tx, stop_rx));
    let mut streams = 0;
    let mut logs = String::new();
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        for e in drain(&mut rx) {
            match e {
                Event::State(State::Streaming) => {
                    streams += 1;
                    if streams == 1 {
                        needed.store(2, Ordering::Relaxed); // the phone app "restarts": it needs a second provisioning
                    }
                }
                Event::Log(t) => logs.push_str(&t),
                _ => {}
            }
        }
        if streams >= 2 {
            break;
        }
    }
    let _ = stop_tx.send(());
    task.await.unwrap();
    assert!(
        streams >= 2,
        "must stream again after provisioning again: {logs}"
    );
    assert!(logs.contains("أعيد تزويده"), "{logs}");
    let given = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter(|l| l.contains("content call"))
        .count();
    assert_eq!(given, 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn failed_provisioning_is_a_clear_error() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("enki-adb-fail-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("adb");
    std::fs::write(&script, "#!/bin/sh\nif [ \"$1\" = shell ]; then echo 'Error: No content provider found' >&2; exit 1; fi\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (port, _) = start_phone(false).await;
    let mut cfg = test_config(port);
    cfg.setup_adb = true;
    cfg.adb = script.display().to_string();
    let (logs, problem) = run_to_failure_with_problem(cfg).await;
    assert!(
        logs.contains("تعذر تزويد الهاتف بمفتاح الجلسة")
            && logs.contains("No content provider found"),
        "{logs}"
    );
    assert_eq!(problem.map(|p| p.code), Some(ProblemCode::ProvisionFailed));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn missing_adb_is_reported_with_its_own_code() {
    let (port, _) = start_phone(false).await;
    let mut cfg = test_config(port);
    cfg.setup_adb = true;
    cfg.adb = "/nonexistent/adb".into();
    let (logs, problem) = run_to_failure_with_problem(cfg).await;
    assert_eq!(
        problem.map(|p| p.code),
        Some(ProblemCode::AdbMissing),
        "{logs}"
    );
}

#[tokio::test]
async fn no_phone_on_adb_is_reported_with_its_own_code() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("enki-adb-nodev-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("adb");
    std::fs::write(
        &script,
        "#!/bin/sh\necho 'error: no devices/emulators found' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (port, _) = start_phone(false).await;
    let mut cfg = test_config(port);
    cfg.setup_adb = true;
    cfg.adb = script.display().to_string();
    let (_, problem) = run_to_failure_with_problem(cfg).await;
    assert_eq!(problem.map(|p| p.code), Some(ProblemCode::NoDevice));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Streams from a fake phone until `done` says enough events arrived (or 20 s pass); returns every event.
async fn collect_events(
    cfg: Config,
    mut done: impl FnMut(&[Event]) -> bool,
    mut on_event: impl FnMut(&Event),
) -> Vec<Event> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(run(cfg, tx, stop_rx));
    let mut all = Vec::new();
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        for e in drain(&mut rx) {
            on_event(&e);
            all.push(e);
        }
        if done(&all) {
            break;
        }
    }
    let _ = stop_tx.send(());
    task.await.unwrap();
    all
}

fn sink_to_null(mut cfg: Config) -> Config {
    cfg.sink_override = Some(("sh".into(), vec!["-c".into(), "cat > /dev/null".into()]));
    cfg
}

#[tokio::test]
async fn link_states_and_capabilities_follow_the_connection() {
    let (port, _) = start_phone(true).await;
    let events = collect_events(
        sink_to_null(test_config(port)),
        |e| {
            e.iter()
                .any(|x| matches!(x, Event::Link(LinkState::Streaming)))
        },
        |_| {},
    )
    .await;
    let position = |want: LinkState| {
        events
            .iter()
            .position(|e| *e == Event::Link(want.clone()))
            .expect("state reported")
    };
    assert!(
        position(LinkState::Ready) < position(LinkState::Streaming),
        "{events:?}"
    );
    let caps = events
        .iter()
        .find_map(|e| {
            if let Event::Capabilities(c) = e {
                Some(c)
            } else {
                None
            }
        })
        .expect("capabilities");
    assert_eq!(caps.cameras.len(), 2);
    assert_eq!(caps.camera("front").unwrap().sensor_orientation, 270);
    let logs: String = events
        .iter()
        .filter_map(|e| {
            if let Event::Log(t) = e {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(
        logs.contains("كاميرات الهاتف") && logs.contains("Fake phone"),
        "{logs}"
    );
}

#[tokio::test]
async fn round_trip_time_is_measured_and_reported_in_the_stats() {
    let (port, _) = start_phone(true).await;
    let events = collect_events(
        sink_to_null(test_config(port)),
        |e| {
            e.iter()
                .any(|x| matches!(x, Event::Stats(s) if s.rtt_ms.is_some()))
        },
        |_| {},
    )
    .await;
    let rtt = events.iter().find_map(|e| {
        if let Event::Stats(s) = e {
            s.rtt_ms
        } else {
            None
        }
    });
    assert!(
        rtt.is_some_and(|ms| ms < 500),
        "loopback round trip should be tiny: {rtt:?}"
    );
}

#[tokio::test]
async fn a_frozen_phone_is_noticed_and_the_receiver_reconnects() {
    use std::sync::atomic::AtomicUsize;
    let mut behavior = Behavior::new(TEST_TOKEN);
    behavior.hold_media = true;
    let pings = behavior.answer_pings.clone();
    let (port, _) = enki_fakephone::spawn(behavior).await;
    let streams = Arc::new(AtomicUsize::new(0));
    let (seen_by_done, seen_by_events) = (streams.clone(), streams.clone());
    let events = collect_events(
        sink_to_null(test_config(port)),
        move |e| {
            seen_by_done.load(Ordering::Relaxed) >= 2
                && e.iter()
                    .any(|x| matches!(x, Event::Link(LinkState::Reconnecting(_))))
        },
        move |e| {
            if let Event::Link(LinkState::Streaming) = e {
                match seen_by_events.fetch_add(1, Ordering::Relaxed) + 1 {
                    1 => pings.store(false, Ordering::Relaxed), // the link freezes: connected, but silent
                    2 => pings.store(true, Ordering::Relaxed),
                    _ => {}
                }
            }
        },
    )
    .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Link(LinkState::Reconnecting(r)) if r == "no heartbeat")),
        "the missing pongs must be noticed: {events:?}"
    );
    let logs: String = events
        .iter()
        .filter_map(|e| {
            if let Event::Log(t) = e {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(logs.contains("انقطع الاتصال بالهاتف"), "{logs}");
    assert!(
        streams.load(Ordering::Relaxed) >= 2,
        "must stream again after the link recovers"
    );
}

/// The whole desktop path with real video: the fake phone's ffmpeg test pattern goes through the NAL splitter and
/// the protocol into the receiver, whose sink is an ffmpeg that decodes it. Skipped where ffmpeg is missing.
#[tokio::test]
async fn real_h264_from_the_fake_phone_decodes() {
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("ffmpeg not installed: skipping");
        return;
    }
    let mut behavior = Behavior::new(TEST_TOKEN);
    behavior.hold_media = true;
    behavior.media = enki_fakephone::testsrc_command("ffmpeg");
    let (port, _) = enki_fakephone::spawn(behavior).await;
    let mut cfg = test_config(port);
    // The decoder reads Annex-B H.264 on stdin, like the real sink, and throws the pictures away.
    cfg.sink_override = Some((
        "ffmpeg".into(),
        [
            "-loglevel",
            "error",
            "-f",
            "h264",
            "-i",
            "-",
            "-f",
            "null",
            "-",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
    ));
    let events = collect_events(
        cfg,
        |e| {
            e.iter()
                .any(|x| matches!(x, Event::Stats(s) if s.fps >= 10))
        },
        |_| {},
    )
    .await;
    let logs: String = events
        .iter()
        .filter_map(|e| {
            if let Event::Log(t) = e {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect();
    let fps = events
        .iter()
        .filter_map(|e| {
            if let Event::Stats(s) = e {
                Some(s.fps)
            } else {
                None
            }
        })
        .max();
    assert!(
        fps.is_some_and(|f| f >= 10),
        "video must flow at a real frame rate: {fps:?}\n{logs}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Problem(_))),
        "{logs}"
    );
}

#[test]
fn every_config_gets_its_own_token() {
    assert_ne!(
        Config::new("/dev/null").token,
        Config::new("/dev/null").token
    );
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

// ---- settings sync with a phone that speaks it ----

use enki_fakephone::PhoneSettings;
use serde_json::json;

const PHONE_ID: &str = "aaaa1111bbbb2222cccc3333dddd4444";

async fn start_syncing_phone() -> (u16, Received, PhoneSettings) {
    let phone = PhoneSettings::new(PHONE_ID);
    let mut behavior = Behavior::new(TEST_TOKEN);
    behavior.hold_media = true;
    behavior.settings = Some(phone.clone());
    let (port, received) = enki_fakephone::spawn(behavior).await;
    (port, received, phone)
}

/// Runs a session, calls `act` once streaming starts, and keeps going until `done` is true (or 20 s).
async fn session_with(
    cfg: Config,
    cmd_rx: mpsc::UnboundedReceiver<Control>,
    mut act: impl FnMut(),
    mut done: impl FnMut(&[Event]) -> bool,
) -> Vec<Event> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(run_with_commands(cfg, tx, stop_rx, cmd_rx));
    let mut all = Vec::new();
    let mut acted = false;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        all.extend(drain(&mut rx));
        if !acted
            && all
                .iter()
                .any(|e| matches!(e, Event::State(State::Streaming)))
        {
            acted = true;
            act();
        }
        if acted && done(&all) {
            break;
        }
    }
    let _ = stop_tx.send(());
    task.await.unwrap();
    all
}

async fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cond()
}

#[tokio::test]
async fn the_phones_settings_reach_the_pc_at_connect_and_the_pc_switches_to_that_phones_profile() {
    let (port, _received, phone) = start_syncing_phone().await;
    phone.change("video.rotate", "90").unwrap();
    phone.change("camera.facing", "front").unwrap();
    let cfg = sink_to_null(test_config(port));
    let hub = cfg.settings.clone();
    let (_keep, cmd_rx) = mpsc::unbounded_channel();
    let events = session_with(cfg, cmd_rx, || {}, |_| true).await;
    let hub = hub.lock().unwrap();
    assert_eq!(hub.get("video.rotate"), Some(json!("90")));
    assert_eq!(
        hub.get("camera.facing"),
        Some(json!("front")),
        "the phone's later choice beat the PC's default"
    );
    assert_eq!(hub.profile(), PHONE_ID);
    assert_eq!(hub.phone_name(), "Fake phone");
    assert!(
        hub.cached_capabilities().unwrap().contains("\"cameras\""),
        "capabilities are cached for next time"
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::Setting { key, value } if key == "video.rotate" && *value == json!("90"))),
        "{events:?}"
    );
}

#[tokio::test]
async fn what_the_pc_chose_later_goes_to_the_phone_and_only_the_difference_travels() {
    let (port, received, phone) = start_syncing_phone().await;
    phone.change("video.mirror", "off").unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let cfg = sink_to_null(test_config(port));
    cfg.settings
        .lock()
        .unwrap()
        .set_local("video.mirror", &json!("on"))
        .unwrap(); // later than the phone's
    let (_keep, cmd_rx) = mpsc::unbounded_channel();
    let _ = session_with(cfg, cmd_rx, || {}, |_| true).await;
    assert_eq!(
        phone.get("video.mirror"),
        Some(json!("on")),
        "the newer PC value won on the phone too"
    );
    let got = received.lock().unwrap();
    let snapshots: Vec<_> = got
        .iter()
        .filter(|m| m.contains("settings_snapshot"))
        .collect();
    assert_eq!(snapshots.len(), 1, "{got:?}");
    assert!(
        snapshots[0].contains("video.mirror") && !snapshots[0].contains("video.auto_rotate"),
        "{snapshots:?}"
    );
    assert!(
        got.iter().all(|m| !m.contains(r#""type":"set""#)),
        "no legacy set for a phone with settings sync: {got:?}"
    );
}

#[tokio::test]
async fn a_change_on_either_side_while_streaming_shows_on_the_other_and_is_acknowledged() {
    let (port, received, phone) = start_syncing_phone().await;
    let cfg = sink_to_null(test_config(port));
    let hub = cfg.settings.clone();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let p = phone.clone();
    let events = session_with(
        cfg,
        cmd_rx,
        move || {
            cmd_tx
                .send(Control::Set("video.rotate".into(), json!("270")))
                .unwrap(); // PC -> phone
            p.change("video.mirror", "on").unwrap(); // phone -> PC
        },
        |_| false,
    )
    .await;
    assert!(
        wait_until(|| phone.get("video.rotate") == Some(json!("270"))).await,
        "PC change reached the phone"
    );
    assert_eq!(
        hub.lock().unwrap().get("video.mirror"),
        Some(json!("on")),
        "phone change reached the PC"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Setting { key, .. } if key == "video.mirror")),
        "{events:?}"
    );
    let got = received.lock().unwrap();
    assert!(
        got.iter()
            .any(|m| m.contains("setting_ack") && m.contains("video.mirror")),
        "the PC acknowledged: {got:?}"
    );
    // Nothing is echoed back: the PC sent exactly one `setting` (its own change).
    assert_eq!(
        got.iter()
            .filter(|m| m.contains(r#""type":"setting""#))
            .count(),
        1,
        "{got:?}"
    );
    assert!(
        !hub.lock().unwrap().engine().has_pending(),
        "the phone's acknowledgement cleared the pending change"
    );
}

#[tokio::test]
async fn a_change_the_phone_refuses_goes_back_and_the_user_is_told() {
    let (port, _received, phone) = start_syncing_phone().await;
    phone.refuse.lock().unwrap().push("camera.facing".into());
    let cfg = sink_to_null(test_config(port));
    let hub = cfg.settings.clone();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let events = session_with(
        cfg,
        cmd_rx,
        move || {
            cmd_tx.send(Control::SetFacing(Facing::Front)).unwrap();
        },
        |e| {
            e.iter()
                .any(|x| matches!(x, Event::SettingRolledBack { .. }))
        },
    )
    .await;
    let rolled = events.iter().find_map(|e| {
        if let Event::SettingRolledBack {
            key, value, code, ..
        } = e
        {
            Some((key, value, code))
        } else {
            None
        }
    });
    assert_eq!(
        rolled,
        Some((
            &"camera.facing".to_string(),
            &json!("back"),
            &"unsupported".to_string()
        )),
        "{events:?}"
    );
    assert_eq!(
        hub.lock().unwrap().get("camera.facing"),
        Some(json!("back"))
    );
}

#[tokio::test]
async fn a_phone_without_settings_sync_still_gets_the_old_set_message() {
    let (port, received) = start_phone(true).await;
    let mut cfg = sink_to_null(test_config(port));
    cfg.facing = Facing::Front;
    let hub = cfg.settings.clone();
    let (_keep, cmd_rx) = mpsc::unbounded_channel();
    let _ = session_with(cfg, cmd_rx, || {}, |_| true).await;
    let got = received.lock().unwrap();
    assert!(
        got.iter()
            .any(|m| m.contains(r#""type":"set""#) && m.contains("front")),
        "{got:?}"
    );
    assert_eq!(
        hub.lock().unwrap().profile(),
        enki_settings::DEFAULT_PROFILE,
        "no phone id, no profile switch"
    );
}

#[tokio::test]
async fn after_stop_nothing_keeps_running_or_reporting() {
    let (port, _received) = start_phone(true).await;
    let cfg = sink_to_null(test_config(port));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(run(cfg, tx, stop_rx));
    let mut streaming = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if drain(&mut rx).iter().any(|e| matches!(e, Event::Stats(_))) {
            streaming = true;
            break;
        }
    }
    assert!(streaming, "the session streamed");
    let _ = stop_tx.send(());
    task.await.unwrap();
    // Every task holding the event sender must be gone: the channel closes instead of reporting stats forever.
    let closed = tokio::time::timeout(Duration::from_secs(3), async {
        while rx.recv().await.is_some() {}
    })
    .await;
    assert!(
        closed.is_ok(),
        "a background task kept sending events after stop"
    );
}
