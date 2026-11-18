# Enki 桌面端消息：以英文为准。各语言的消息 id 必须相同。

app-title = Enki

group-video = 视频
group-app = 应用

status-label = 状态
status-stopped = 已停止
status-preparing = 正在准备与手机的连接
status-waiting-app = 等待手机应用
status-opening-camera = 正在打开摄像头
status-connected = 已连接
status-reconnecting = 正在重新连接
status-streaming = 已连接 · { $fps } 帧/秒 · { $mbps } Mbps
status-rtt = { $ms } ms
status-target = 目标 { $mbps } Mbps
status-phone = 手机：{ $name }

setting-camera-facing = 手机摄像头
setting-camera-facing--back = 后置
setting-camera-facing--front = 前置
setting-video-auto-rotate = 随手机旋转（方向传感器）
setting-video-rotate = 画面旋转
setting-video-rotate--auto = 自动（按摄像头）
setting-video-rotate--0 = 0°
setting-video-rotate--90 = 90°
setting-video-rotate--180 = 180°
setting-video-rotate--270 = 270°
setting-video-mirror = 镜像
setting-video-mirror--auto = 自动
setting-video-mirror--on = 开
setting-video-mirror--off = 关
setting-ui-language = 语言
setting-ui-language--system = 跟随系统
setting-ui-language--en = English
setting-ui-language--ar = العربية
setting-ui-language--zh = 中文
setting-source-mode = 来源
setting-source-mode--enki-camera = 手机摄像头（Enki 应用）→ V4L2 设备
setting-source-mode--scrcpy-camera = 手机摄像头（scrcpy，Android 12+）→ V4L2 设备
setting-source-mode--screen-window = 手机屏幕 → 窗口
setting-source-mode--screen-v4l2 = 手机屏幕 → V4L2 设备
setting-output-device = V4L2 设备

label-resolution = 分辨率（scrcpy 来源）
check-audio = 包含麦克风（scrcpy 音频转发）

button-modprobe = 1. 加载内核驱动
button-start = 2. 启动摄像头
button-stop = 停止传输
fix-load-module = 加载内核驱动
fix-retry = 重试

log-title = 系统日志与实时错误
log-no-stream = 当前没有正在进行的传输。
log-invalid-device = 路径无效：{ $device }。应为：/dev/videoN
log-starting-enki = Enki 摄像头（{ $facing }）→ { $device }...
log-settings-warning = 设置：{ $message }
log-setting-rolled-back = 手机拒绝了 { $setting }：{ $message }
