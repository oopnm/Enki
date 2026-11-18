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
