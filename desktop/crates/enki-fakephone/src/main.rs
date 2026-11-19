//! `enki-fakephone`: run the desktop app without a phone.
//!
//!     enki-fakephone --accept-any-token              # test pattern from ffmpeg on 127.0.0.1:28200
//!     ENKI_NO_ADB=1 enki                             # the desktop app then skips adb and talks to it
//!
//! Options: `--port N`, `--token HEX` (64 digits; the default accepts any token), `--canned` (tiny canned
//! frames instead of an ffmpeg test pattern), `--ffmpeg PATH`, `--device NAME`.

use enki_fakephone::{serve, testsrc_command, Behavior, MediaSource, PhoneSettings, Received};
use enki_protocol::{from_hex, TOKEN_LEN};
use std::sync::Arc;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    let mut port = 28200u16;
    let mut token: Option<[u8; TOKEN_LEN]> = None;
    let mut canned = false;
    let mut ffmpeg = "ffmpeg".to_string();
    let mut device = "Enki fake phone".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                port = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage("--port needs a number"))
            }
            "--token" => {
                token = args
                    .next()
                    .and_then(|v| from_hex(&v))
                    .and_then(|v| v.try_into().ok())
                    .or_else(|| usage("--token needs 64 hex digits"))
            }
            "--accept-any-token" => token = None,
            "--canned" => canned = true,
            "--ffmpeg" => {
                ffmpeg = args
                    .next()
                    .unwrap_or_else(|| usage("--ffmpeg needs a path"))
            }
            "--device" => {
                device = args
                    .next()
                    .unwrap_or_else(|| usage("--device needs a name"))
            }
            _ => usage(&format!("unknown option {arg}")),
        }
    }
    let mut behavior = Behavior::new(token.unwrap_or([0; TOKEN_LEN]));
    behavior.accept_any_token = token.is_none();
    behavior.hold_media = true;
    behavior.settings = Some(PhoneSettings::new("fa4e0000fa4e0000fa4e0000fa4e0000"));
    behavior.device = device;
    if !canned {
        behavior.media = testsrc_command(&ffmpeg);
    } else {
        behavior.media = MediaSource::Canned;
    }
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap_or_else(|e| {
            eprintln!("cannot listen on 127.0.0.1:{port}: {e}");
            std::process::exit(1)
        });
    eprintln!(
        "enki-fakephone listening on 127.0.0.1:{port} ({})",
        if token.is_some() {
            "token required"
        } else {
            "any token accepted"
        }
    );
    let received: Received = Arc::default();
    serve(listener, behavior, received).await;
}

fn usage<T>(message: &str) -> T {
    eprintln!("{message}\nusage: enki-fakephone [--port N] [--token HEX | --accept-any-token] [--canned] [--ffmpeg PATH] [--device NAME]");
    std::process::exit(2)
}
