//! Wi-Fi: pairing, pinned TLS and a whole session over TLS, against the fake phone.

use super::*;
use enki_fakephone::{Behavior, TlsPhone};
use std::net::SocketAddr;

const CODE: &str = "ABCDE-FGHJK";
const PC_ID: &str = "fedcba9876543210fedcba9876543210";

async fn tls_phone() -> (SocketAddr, TlsPhone, enki_fakephone::Received) {
    let tls = TlsPhone::new("0123456789abcdef0123456789abcdef");
    let mut behavior = Behavior::new([0; TOKEN_LEN]);
    behavior.hold_media = true;
    behavior.tls = Some(tls.clone());
    behavior.tls_listener = true;
    let (port, received) = enki_fakephone::spawn(behavior).await;
    (SocketAddr::from(([127, 0, 0, 1], port)), tls, received)
}

fn drain(rx: &mut mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

#[tokio::test]
async fn pairing_with_the_code_gives_the_secret_and_the_pinned_fingerprint() {
    let (addr, tls, received) = tls_phone().await;
    tls.offer_code(CODE);
    let phone = pair_with_code(CODE, "my-laptop", PC_ID, Some(addr), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(phone.fingerprint, to_hex(&tls.fingerprint));
    assert_eq!(phone.id, "0123456789abcdef0123456789abcdef");
    assert_eq!(phone.last_host.as_deref(), Some("127.0.0.1"));
    assert!(tls
        .paired
        .lock()
        .unwrap()
        .contains(&phone.secret_bytes().unwrap()));
    assert!(
        tls.offer.lock().unwrap().is_none(),
        "the code is good for one pairing"
    );
    assert!(received
        .lock()
        .unwrap()
        .iter()
        .any(|m| m.contains("my-laptop")));
    // A second try with the same code finds no offer and gives up.
    let again = pair_with_code(
        CODE,
        "thief",
        PC_ID,
        Some(addr),
        Duration::from_millis(1200),
    )
    .await;
    assert_eq!(again, Err(PairError::NotFound));
}

#[tokio::test]
async fn a_wrong_code_is_refused_and_nothing_is_paired() {
    let (addr, tls, _) = tls_phone().await;
    tls.offer_code(CODE);
    let r = pair_with_code(
        "ZZZZZ-ZZZZZ",
        "x",
        PC_ID,
        Some(addr),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(r, Err(PairError::WrongCode));
    assert!(tls.paired.lock().unwrap().is_empty());
    assert!(
        tls.offer.lock().unwrap().is_some(),
        "the right code still works"
    );
    assert_eq!(
        pair_with_code("short", "x", PC_ID, Some(addr), Duration::from_secs(1)).await,
        Err(PairError::BadCode)
    );
}

#[tokio::test]
async fn without_a_code_on_the_phone_pairing_waits_then_gives_up() {
    let (addr, tls, _) = tls_phone().await;
    let r = pair_with_code(CODE, "x", PC_ID, Some(addr), Duration::from_millis(1300)).await;
    assert_eq!(r, Err(PairError::NotFound));
    assert!(tls.paired.lock().unwrap().is_empty());
}

fn wifi_config(addr: SocketAddr, phone: PairedPhone, out: &std::path::Path) -> Config {
    let mut cfg = Config::new("/dev/null");
    cfg.setup_adb = false;
    cfg.transport = Transport::Wifi;
    cfg.wifi = Some(phone);
    cfg.wifi_addr = Some(addr);
    cfg.sink_override = Some((
        "sh".into(),
        vec!["-c".into(), format!("cat > {}", out.display())],
    ));
    cfg
}

async fn paired(addr: SocketAddr, tls: &TlsPhone) -> PairedPhone {
    tls.offer_code(CODE);
    pair_with_code(CODE, "laptop", PC_ID, Some(addr), Duration::from_secs(5))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_session_over_tls_streams_like_one_over_usb() {
    let (addr, tls, _) = tls_phone().await;
    let phone = paired(addr, &tls).await;
    let out = std::env::temp_dir().join(format!(
        "enki-wifi-test-{}-{}.h264",
        std::process::id(),
        addr.port()
    ));
    let cfg = wifi_config(addr, phone.clone(), &out);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(run(cfg, tx, stop_rx));
    let expected_start: Vec<u8> = vec![0, 0, 1, 0x67, 0, 0, 1, 0x65, 0x41, 0x9a];
    let mut got = Vec::new();
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        got = std::fs::read(&out).unwrap_or_default();
        if got.starts_with(&expected_start) {
            break;
        }
    }
    assert!(
        got.starts_with(&expected_start),
        "frames arrive through TLS: {got:?}"
    );
    let _ = stop_tx.send(());
    task.await.unwrap();
    let _ = std::fs::remove_file(&out);
    let events = drain(&mut rx);
    assert!(events.iter().any(
        |e| matches!(e, Event::WifiConnected { id, host } if *id == phone.id && host == "127.0.0.1")
    ));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::State(State::Streaming))));
}

#[tokio::test]
async fn another_certificate_is_refused() {
    let (addr, tls, _) = tls_phone().await;
    let mut phone = paired(addr, &tls).await;
    phone.fingerprint = to_hex(&[1u8; 32]); // the pin no longer matches what the phone shows
    let cfg = wifi_config(addr, phone, std::path::Path::new("/dev/null"));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_stop, stop_rx) = oneshot::channel();
    tokio::time::timeout(Duration::from_secs(10), run(cfg, tx, stop_rx))
        .await
        .expect("ends by itself");
    let problem = drain(&mut rx).into_iter().find_map(|e| {
        if let Event::Problem(p) = e {
            Some(p)
        } else {
            None
        }
    });
    assert_eq!(
        problem.map(|p| p.code),
        Some(ProblemCode::CertificateChanged)
    );
}

#[tokio::test]
async fn a_phone_that_forgot_us_is_reported_as_not_paired() {
    let (addr, tls, _) = tls_phone().await;
    let phone = paired(addr, &tls).await;
    tls.paired.lock().unwrap().clear(); // "Forget" pressed on the phone
    let cfg = wifi_config(addr, phone, std::path::Path::new("/dev/null"));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_stop, stop_rx) = oneshot::channel();
    tokio::time::timeout(Duration::from_secs(10), run(cfg, tx, stop_rx))
        .await
        .expect("ends by itself");
    let problem = drain(&mut rx).into_iter().find_map(|e| {
        if let Event::Problem(p) = e {
            Some(p)
        } else {
            None
        }
    });
    assert_eq!(problem.map(|p| p.code), Some(ProblemCode::NotPaired));
}

#[tokio::test]
async fn wifi_without_a_chosen_phone_is_a_clear_problem() {
    let mut cfg = Config::new("/dev/null");
    cfg.setup_adb = false;
    cfg.transport = Transport::Wifi;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_stop, stop_rx) = oneshot::channel();
    run(cfg, tx, stop_rx).await;
    let problem = drain(&mut rx).into_iter().find_map(|e| {
        if let Event::Problem(p) = e {
            Some(p)
        } else {
            None
        }
    });
    assert_eq!(problem.map(|p| p.code), Some(ProblemCode::NoPairedPhone));
}

#[tokio::test]
async fn auto_falls_back_to_wifi_when_no_phone_is_plugged_in() {
    let (addr, tls, _) = tls_phone().await;
    let phone = paired(addr, &tls).await;
    let out = std::env::temp_dir().join(format!(
        "enki-auto-test-{}-{}.h264",
        std::process::id(),
        addr.port()
    ));
    let mut cfg = wifi_config(addr, phone, &out);
    cfg.transport = Transport::Auto;
    cfg.setup_adb = true;
    cfg.adb = "enki-no-such-adb".into(); // adb is not installed: no cable, so Wi-Fi
    let (tx, _rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(run(cfg, tx, stop_rx));
    let mut got = Vec::new();
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        got = std::fs::read(&out).unwrap_or_default();
        if !got.is_empty() {
            break;
        }
    }
    assert!(!got.is_empty(), "streaming over Wi-Fi without adb");
    let _ = stop_tx.send(());
    task.await.unwrap();
    let _ = std::fs::remove_file(&out);
}

#[tokio::test]
async fn pairing_over_the_cable_needs_no_code() {
    // The plain listener is the phone's loopback port; a fake adb stands in for the real one.
    let tls = TlsPhone::new("0123456789abcdef0123456789abcdef");
    let mut behavior = Behavior::new([0; TOKEN_LEN]);
    behavior.accept_any_token = true;
    behavior.tls = Some(tls.clone());
    let (port, received) = enki_fakephone::spawn(behavior).await;
    let dir = tempfile::tempdir().unwrap();
    let adb = dir.path().join("adb");
    std::fs::write(&adb, "#!/bin/sh\ncase \"$*\" in *\"content call\"*) echo 'Result: Bundle[{ok=true}]';; esac\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).unwrap();
    let phone = pair_over_adb(adb.to_str().unwrap(), port, "my-laptop", PC_ID)
        .await
        .unwrap();
    assert_eq!(phone.fingerprint, to_hex(&tls.fingerprint));
    assert!(tls
        .paired
        .lock()
        .unwrap()
        .contains(&phone.secret_bytes().unwrap()));
    assert!(received
        .lock()
        .unwrap()
        .iter()
        .any(|m| m.contains("\"type\":\"pair\"")));
}

#[tokio::test]
async fn pairing_over_the_cable_says_when_the_app_is_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let adb = dir.path().join("adb");
    std::fs::write(&adb, "#!/bin/sh\ncase \"$*\" in *\"content call\"*) echo 'Result: Bundle[{ok=true}]';; esac\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A port nobody listens on.
    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let r = pair_over_adb(adb.to_str().unwrap(), free, "x", PC_ID).await;
    assert_eq!(r, Err(PairError::PhoneAppNotRunning));
}
