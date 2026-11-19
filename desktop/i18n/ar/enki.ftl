# رسائل Enki لسطح المكتب: الإنجليزية هي المرجع. معرّفات الرسائل واحدة في كل اللغات.

app-title = Enki

group-video = الفيديو
group-app = التطبيق

status-label = الحالة
status-stopped = متوقف
status-preparing = تجهيز الاتصال بالهاتف
status-waiting-app = بانتظار تطبيق الهاتف
status-opening-camera = فتح الكاميرا
status-connected = متصل
status-reconnecting = إعادة الاتصال
status-streaming = متصل · { $fps } إطار/ث · { $mbps } Mbps
status-rtt = { $ms } ms
status-target = الهدف { $mbps } Mbps
status-phone = الهاتف: { $name }

setting-camera-facing = كاميرا الهاتف
setting-camera-facing--back = خلفية
setting-camera-facing--front = أمامية
setting-video-auto-rotate = تدوير مع الهاتف (مستشعر الاتجاه)
setting-video-rotate = تدوير الصورة
setting-video-rotate--auto = تلقائي (حسب الكاميرا)
setting-video-rotate--0 = 0°
setting-video-rotate--90 = 90°
setting-video-rotate--180 = 180°
setting-video-rotate--270 = 270°
setting-video-mirror = المرآة
setting-video-mirror--auto = تلقائي
setting-video-mirror--on = مفعّلة
setting-video-mirror--off = متوقفة
setting-ui-language = اللغة
setting-ui-language--system = مثل النظام
setting-ui-language--en = English
setting-ui-language--ar = العربية
setting-ui-language--zh = 中文
setting-source-mode = المصدر
setting-source-mode--enki-camera = كاميرا الهاتف (تطبيق Enki) ← جهاز V4L2
setting-source-mode--scrcpy-camera = كاميرا الهاتف (scrcpy، أندرويد 12+) ← جهاز V4L2
setting-source-mode--screen-window = شاشة الهاتف ← نافذة
setting-source-mode--screen-v4l2 = شاشة الهاتف ← جهاز V4L2
setting-output-device = جهاز V4L2

label-resolution = الدقة (مصادر scrcpy)
check-audio = تضمين المايكروفون (نقل صوت scrcpy)

button-modprobe = 1. تفعيل درايفر النواة
button-start = 2. تشغيل الكاميرا
button-stop = إيقاف البث
fix-load-module = تفعيل درايفر النواة
fix-retry = إعادة المحاولة

log-title = سجلات النظام والأخطاء الفورية
log-no-stream = لا يوجد بث نشط حالياً.
log-invalid-device = مسار غير صالح: { $device }. الصيغة المطلوبة: /dev/videoN
log-starting-enki = كاميرا Enki ({ $facing }) ← { $device }...
log-settings-warning = الإعدادات: { $message }
log-setting-rolled-back = الهاتف رفض { $setting }: { $message }
