//! The picture the computer makes from the phone's stream: the ffmpeg filter chain and command line.
//!
//! Everything here is pure (no processes, no I/O) so it is tested without a phone. A change to any field of
//! [`Look`] restarts the decoder, because ffmpeg cannot change its filter graph while it runs.

/// How the phone encodes its video (the `stream.codec` state it sends when its encoder starts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Codec {
    #[default]
    H264,
    Hevc,
}

impl Codec {
    /// The ffmpeg raw demuxer name for this codec.
    pub fn demuxer(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
        }
    }

    /// Tells the codec from the parameter sets the phone sends before every key frame: H.264 starts with an SPS
    /// (NAL type 7), H.265 with a VPS or SPS (types 32 and 33). `None` when the bytes say neither.
    pub fn detect(config: &[u8]) -> Option<Codec> {
        let start = config
            .windows(3)
            .position(|w| w == [0, 0, 1])
            .map(|i| i + 3)?;
        let b = *config.get(start)?;
        if b & 0x80 != 0 {
            return None; // forbidden_zero_bit set: not a NAL header
        }
        match (b & 0x1f, (b >> 1) & 0x3f) {
            (7, _) => Some(Codec::H264),
            (_, 32) | (_, 33) => Some(Codec::Hevc),
            _ => None,
        }
    }

    /// `None` for a name this program does not know (an unknown codec must not silently become H.264).
    pub fn from_wire(name: &str) -> Option<Codec> {
        match name {
            "h264" => Some(Codec::H264),
            "hevc" => Some(Codec::Hevc),
            _ => None,
        }
    }
}

/// Picture filters, in hundredths so the type stays `Eq` (brightness 0 = unchanged, contrast and saturation
/// 100 = unchanged, sharpness 0 = off) and crop in percent of each side length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Filters {
    pub brightness: i32,
    pub contrast: i32,
    pub saturation: i32,
    pub sharpness: i32,
    pub crop: u32,
}

impl Default for Filters {
    fn default() -> Self {
        Filters {
            brightness: 0,
            contrast: 100,
            saturation: 100,
            sharpness: 0,
            crop: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Look {
    /// Clockwise degrees (0, 90, 180, 270).
    pub rotate: u16,
    pub mirror: bool,
    /// Flip upside down (after the rotation).
    pub flip: bool,
    pub filters: Filters,
    pub codec: Codec,
    pub fps: u32,
    /// Output height in pixels (480, 720 or 1080); the width is 16:9 of it.
    pub out_height: u32,
}

impl Default for Look {
    fn default() -> Self {
        Look {
            rotate: 0,
            mirror: false,
            flip: false,
            filters: Filters::default(),
            codec: Codec::H264,
            fps: 30,
            out_height: 720,
        }
    }
}

impl Look {
    pub fn out_size(&self) -> (u32, u32) {
        let h = self.out_height.clamp(2, 2160) & !1;
        ((h * 16 / 9 + 1) & !1, h)
    }
}

/// The filters that change the picture itself: orientation, flip, crop, colour, sharpness.
fn adjust_chain(look: &Look) -> Vec<String> {
    let mut chain: Vec<String> = Vec::new();
    match look.rotate % 360 {
        90 => chain.push("transpose=1".into()),
        180 => {
            chain.push("hflip".into());
            chain.push("vflip".into());
        }
        270 => chain.push("transpose=2".into()),
        _ => {}
    }
    if look.mirror {
        chain.push("hflip".into());
    }
    if look.flip {
        chain.push("vflip".into());
    }
    let f = &look.filters;
    if f.crop > 0 {
        let keep = (100 - f.crop.min(50)) as f64 / 100.0;
        chain.push(format!(
            "crop=trunc(iw*{keep:.2}/2)*2:trunc(ih*{keep:.2}/2)*2"
        ));
    }
    if f.brightness != 0 || f.contrast != 100 || f.saturation != 100 {
        chain.push(format!(
            "eq=brightness={:.2}:contrast={:.2}:saturation={:.2}",
            f.brightness as f64 / 100.0,
            f.contrast as f64 / 100.0,
            f.saturation as f64 / 100.0
        ));
    }
    if f.sharpness > 0 {
        chain.push(format!(
            "unsharp=5:5:{:.2}:5:5:0",
            f.sharpness as f64 / 100.0
        ));
    }
    chain
}

/// The filter chain: the adjustments, then a fit into the fixed output frame with black bars. The frame size only
/// changes when the user picks another resolution, because programs that already opened the V4L2 device expect the
/// size they negotiated.
pub fn filter_chain(look: &Look) -> String {
    let mut chain = adjust_chain(look);
    let (w, h) = look.out_size();
    chain.push(format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease:force_divisible_by=2"
    ));
    chain.push(format!("pad={w}:{h}:(ow-iw)/2:(oh-ih)/2"));
    chain.push("setsar=1".into());
    chain.join(",")
}

fn input_args(look: &Look) -> Vec<String> {
    let mut args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "warning",
        "-fflags",
        "nobuffer",
        "-flags",
        "low_delay",
        "-probesize",
        "200000",
        "-analyzeduration",
        "0",
        "-framerate",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(look.fps.to_string());
    args.extend(["-f", look.codec.demuxer(), "-i", "-"].map(String::from));
    args
}

/// ffmpeg arguments tuned for live use: no probing delay and no decoder-side buffering.
/// `-framerate` matters: without it ffmpeg cannot timestamp the raw stream and drops almost every frame.
pub fn ffmpeg_args_for(device: &str, look: &Look) -> Vec<String> {
    let mut args = input_args(look);
    args.push("-vf".into());
    args.push(filter_chain(look));
    args.extend(
        [
            "-pix_fmt",
            "yuv420p",
            "-fps_mode",
            "passthrough",
            "-f",
            "v4l2",
        ]
        .map(String::from),
    );
    args.push(device.to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_add_no_filters_and_keep_the_old_chain() {
        assert_eq!(
            filter_chain(&Look::default()),
            "scale=1280:720:force_original_aspect_ratio=decrease:force_divisible_by=2,pad=1280:720:(ow-iw)/2:(oh-ih)/2,setsar=1"
        );
    }

    #[test]
    fn output_size_follows_the_resolution() {
        let size = |h| {
            Look {
                out_height: h,
                ..Look::default()
            }
            .out_size()
        };
        assert_eq!(size(480), (854, 480));
        assert_eq!(size(720), (1280, 720));
        assert_eq!(size(1080), (1920, 1080));
    }

    #[test]
    fn every_filter_shows_up_in_order() {
        let look = Look {
            rotate: 90,
            mirror: true,
            flip: true,
            filters: Filters {
                brightness: 10,
                contrast: 120,
                saturation: 150,
                sharpness: 80,
                crop: 20,
            },
            ..Look::default()
        };
        let chain = filter_chain(&look);
        let order = [
            "transpose=1",
            "hflip",
            "vflip",
            "crop=",
            "eq=brightness=0.10:contrast=1.20:saturation=1.50",
            "unsharp=5:5:0.80",
            "scale=",
            "pad=",
            "setsar=1",
        ];
        let mut at = 0;
        for part in order {
            let found = chain[at..]
                .find(part)
                .unwrap_or_else(|| panic!("{part} missing or out of order in {chain}"));
            at += found + part.len();
        }
    }

    #[test]
    fn crop_never_goes_past_half() {
        let look = Look {
            filters: Filters {
                crop: 90,
                ..Filters::default()
            },
            ..Look::default()
        };
        assert!(filter_chain(&look).contains("crop=trunc(iw*0.50/2)*2"));
    }

    #[test]
    fn the_codec_is_recognised_from_the_parameter_sets() {
        assert_eq!(
            Codec::detect(&[0, 0, 0, 1, 0x67, 0x42, 0x00]),
            Some(Codec::H264)
        );
        assert_eq!(Codec::detect(&[0, 0, 1, 0x67, 0x42]), Some(Codec::H264));
        assert_eq!(
            Codec::detect(&[0, 0, 0, 1, 0x40, 0x01, 0x0c]),
            Some(Codec::Hevc)
        );
        assert_eq!(Codec::detect(&[0, 0, 0, 1, 0x42, 0x01]), Some(Codec::Hevc));
        assert_eq!(Codec::detect(&[1, 2, 3]), None);
        assert_eq!(Codec::detect(&[0, 0, 0, 1, 0x65]), None);
    }

    #[test]
    fn the_codec_picks_the_demuxer() {
        let args = ffmpeg_args_for(
            "/dev/video10",
            &Look {
                codec: Codec::Hevc,
                ..Look::default()
            },
        )
        .join(" ");
        assert!(args.contains("-framerate 30 -f hevc -i -"));
        assert_eq!(Codec::from_wire("hevc"), Some(Codec::Hevc));
        assert_eq!(Codec::from_wire("vp9"), None);
    }

    /// Runs the real ffmpeg (when installed) on a generated stream: the graph must be valid.
    #[test]
    fn ffmpeg_accepts_the_full_graph() {
        use std::process::{Command, Stdio};
        let have = |p: &str| {
            Command::new(p)
                .arg("-version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !have("ffmpeg") {
            eprintln!("ffmpeg not installed: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("enki-look-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.h264");
        let made = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=640x360:rate=30",
                "-t",
                "3",
                "-g",
                "15",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-f",
                "h264",
            ])
            .arg(&src)
            .status();
        if !matches!(made, Ok(s) if s.success()) {
            eprintln!("ffmpeg has no libx264: skipped");
            return;
        }
        let look = Look {
            rotate: 90,
            mirror: true,
            flip: true,
            filters: Filters {
                brightness: 10,
                contrast: 120,
                saturation: 150,
                sharpness: 80,
                crop: 20,
            },
            out_height: 480,
            ..Look::default()
        };
        let mut args = ffmpeg_args_for("OUT", &look);
        let at = args.iter().position(|a| a == "v4l2").unwrap();
        args[at] = "null".into();
        args[at + 1] = "-".into();
        let run = Command::new("ffmpeg")
            .args(&args)
            .stdin(std::fs::File::open(&src).unwrap())
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
    }
}
