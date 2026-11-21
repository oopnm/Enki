# Enki desktop messages: English is the reference. Message ids are the same in every language
# (tools/check_i18n.py and the enki-i18n tests fail when they differ).
# Settings: `setting-<key>` is the label and `setting-<key>--<value>` the label of one choice, where the key and the
# value have '.' and '_' replaced by '-' (camera.facing / front -> setting-camera-facing--front).

app-title = Enki

group-video = Video
group-app = App

status-label = Status
status-stopped = Stopped
status-preparing = Preparing the link to the phone
status-waiting-app = Waiting for the phone app
status-opening-camera = Opening the camera
status-connected = Connected
status-reconnecting = Reconnecting
status-streaming = Connected · { $fps } fps · { $mbps } Mbps
status-rtt = { $ms } ms
status-target = target { $mbps } Mbps
status-phone = Phone: { $name }

setting-camera-facing = Phone camera
setting-camera-facing--back = Back
setting-camera-facing--front = Front
setting-video-auto-rotate = Rotate with the phone (orientation sensor)
setting-video-rotate = Picture rotation
setting-video-rotate--auto = Automatic (per camera)
setting-video-rotate--0 = 0°
setting-video-rotate--90 = 90°
setting-video-rotate--180 = 180°
setting-video-rotate--270 = 270°
setting-video-mirror = Mirror
setting-video-mirror--auto = Automatic
setting-video-mirror--on = On
setting-video-mirror--off = Off
setting-ui-language = Language
setting-ui-language--system = Same as the system
setting-ui-language--en = English
setting-ui-language--ar = العربية
setting-ui-language--zh = 中文
setting-source-mode = Source
setting-source-mode--enki-camera = Phone camera (Enki app) → V4L2 device
setting-source-mode--scrcpy-camera = Phone camera (scrcpy, Android 12+) → V4L2 device
setting-source-mode--screen-window = Phone screen → window
setting-source-mode--screen-v4l2 = Phone screen → V4L2 device
setting-output-device = V4L2 device

label-resolution = Resolution (scrcpy sources)
check-audio = Include the microphone (scrcpy audio forwarding)

button-modprobe = 1. Load the kernel driver
button-start = 2. Start the camera
button-stop = Stop streaming
fix-load-module = Load the kernel driver
fix-retry = Try again

log-title = System log and live errors
log-no-stream = There is no active stream.
log-invalid-device = Not a valid path: { $device }. Expected form: /dev/videoN
log-starting-enki = Enki camera ({ $facing }) → { $device }...
log-settings-warning = Settings: { $message }
log-setting-rolled-back = The phone refused { $setting }: { $message }

# Phase 5 settings
group-camera = Camera
group-filters = Picture filters (applied on the computer)
group-phone = Phone
setting-video-resolution = Resolution
setting-video-resolution--480p = 480p
setting-video-resolution--720p = 720p (HD)
setting-video-resolution--1080p = 1080p (Full HD)
setting-video-fps = Frame rate
setting-video-fps--15 = 15 fps
setting-video-fps--24 = 24 fps
setting-video-fps--30 = 30 fps
setting-video-fps--60 = 60 fps
setting-video-codec = Video codec
setting-video-codec--h264 = H.264 (most compatible)
setting-video-codec--hevc = H.265 / HEVC (smaller)
setting-video-bitrate-mode = Bit rate
setting-video-bitrate-mode--auto = Automatic
setting-video-bitrate-mode--manual = Manual
setting-video-bitrate-kbps = Manual bit rate (kbit/s)
setting-video-flip = Flip upside down
setting-camera-zoom = Zoom
setting-camera-focus-mode = Focus
setting-camera-focus-mode--continuous = Continuous
setting-camera-focus-mode--once = Focus once, then hold
setting-camera-focus-mode--manual = Manual
setting-camera-focus-distance = Manual focus distance (far → near)
setting-camera-exposure-comp = Exposure compensation
setting-camera-iso = ISO
setting-camera-iso--auto = Auto
setting-camera-iso--100 = 100
setting-camera-iso--200 = 200
setting-camera-iso--400 = 400
setting-camera-iso--800 = 800
setting-camera-iso--1600 = 1600
setting-camera-iso--3200 = 3200
setting-camera-shutter = Shutter time
setting-camera-shutter--auto = Auto
setting-camera-shutter--30 = 1/30 s
setting-camera-shutter--60 = 1/60 s
setting-camera-shutter--120 = 1/120 s
setting-camera-shutter--250 = 1/250 s
setting-camera-shutter--500 = 1/500 s
setting-camera-white-balance = White balance
setting-camera-white-balance--auto = Auto
setting-camera-white-balance--incandescent = Incandescent
setting-camera-white-balance--fluorescent = Fluorescent
setting-camera-white-balance--daylight = Daylight
setting-camera-white-balance--cloudy = Cloudy
setting-camera-torch = Flash light (torch)
setting-camera-stabilization = Video stabilization
setting-camera-noise-reduction = Noise reduction
setting-camera-noise-reduction--auto = Auto
setting-camera-noise-reduction--off = Off
setting-camera-noise-reduction--fast = Fast
setting-camera-noise-reduction--quality = High quality
setting-camera-antiflicker = Anti-flicker (mains frequency)
setting-camera-antiflicker--auto = Auto
setting-camera-antiflicker--off = Off
setting-camera-antiflicker--50 = 50 Hz
setting-camera-antiflicker--60 = 60 Hz
setting-filter-brightness = Brightness
setting-filter-contrast = Contrast
setting-filter-saturation = Saturation
setting-filter-sharpness = Sharpness
setting-filter-crop = Digital crop (%)
setting-phone-keep-screen-on = Keep the phone screen on
setting-phone-black-screen = Black screen (saves battery)
setting-phone-thermal-reduce = Lower quality when the phone gets hot
setting-phone-auto-start = Start streaming when the app opens
status-battery = Battery { $percent }%
status-temperature = Temperature { $degrees } °C
status-charging = Charging
setting-video-fps--120 = 120 fps (high-speed mode, 720p, higher delay)
button-preview = Preview (mpv)
log-preview-missing = The preview needs mpv. Install it (for example: sudo pacman -S mpv) and press Preview again.
log-preview-failed = mpv could not show the camera. Is the stream running?
setting-phone-show-preview = Show the camera preview on this phone

# Phase 6
setting-ui-language--tr = Türkçe
setting-ui-language--fa = فارسی
setting-ui-language--ckb = کوردیی ناوەندی
setting-ui-language--es = Español
setting-ui-language--fr = Français
setting-ui-language--ru = Русский
setting-ui-language--de = Deutsch
setting-ui-language--pt = Português
setting-ui-language--ja = 日本語
setting-ui-language--hi = हिन्दी
setting-ui-reduce-motion = Reduce motion (still water, no moving waves)
