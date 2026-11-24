//! Enki desktop window. The settings controls are built from the shared schema (`protocol/settings.json`), so this
//! window shows the same settings as the phone app, and a change made on either side shows on the other. Texts come
//! from the Fluent files in `desktop/i18n`; changing the language rebuilds the window in the new language, with
//! right-to-left layout for Arabic.

mod abzu;
mod tokens;

use abzu::{Motion, Visual};
use enki_i18n::I18n;
use enki_receiver::enki_settings::{Hub, Kind, Scope, SettingsHandle, Store};
use gtk::glib;
use gtk::prelude::*;
use gtk::Box as GtkBox;
use gtk::{
    Application, ApplicationWindow, Button, CheckButton, DropDown, Entry, Label, Orientation,
    Scale, ScrolledWindow, TextBuffer, TextView,
};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::process::Stdio;
use std::rc::Rc;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

const APP_ID: &str = "io.github.oopnm.Enki";
/// Named colors shared with the phone app (generated from protocol/tokens.json).
const TOKENS_CSS: &str = include_str!("tokens.css");
/// The Abzu look: deep-water gradient, glowing cards, aqua accents.
const THEME_CSS: &str = include_str!("theme.css");
/// The logo (a cylinder seal), shown in the window header.
const LOGO_PNG: &[u8] = include_bytes!("logo.png");

/// What the status line says. Rendered with the current language each time, so a language change updates it.
#[derive(Debug, Clone, PartialEq)]
enum Status {
    Stopped,
    Preparing,
    WaitingApp,
    OpeningCamera,
    Connected,
    Reconnecting,
    Streaming {
        fps: u32,
        mbps: f64,
        rtt_ms: Option<u32>,
        target_mbps: Option<f64>,
    },
}

/// رسائل تُرسل من مهام Tokio إلى واجهة GTK (تُعالَج فقط على الخيط الرئيسي).
enum UiEvent {
    Log(String),
    Running(bool),
    Status(Status),
    /// مشكلة أنهت الجلسة: يظهر زر الإصلاح المناسب لها إن وُجد.
    Problem(enki_receiver::Problem),
    /// Pairing finished (a phone to remember, or the reason it failed).
    Paired(Result<enki_receiver::PairedPhone, String>),
    /// Close the pairing window (pairing ended).
    CloseDialog,
    /// The address a Wi-Fi session reached the phone at, remembered for next time.
    WifiHost {
        id: String,
        host: String,
    },
    /// A setting (or the phone profile) changed behind the window's back: redraw the controls.
    Refresh,
    /// Battery and temperature reported by the phone.
    Phone(enki_receiver::PhoneInfo),
    /// The mpv preview window was closed (or never started).
    PreviewClosed,
    /// The phone refused a change made here (already rolled back in the settings).
    RolledBack {
        key: String,
        message: String,
    },
}

type EventTx = mpsc::UnboundedSender<UiEvent>;

/// One settings control, so it can be set from the stored value.
enum Control {
    Choice(DropDown, Vec<String>),
    Toggle(CheckButton),
    Text(Entry),
    Slider(Scale),
}

/// Message id of a setting label (`camera.facing` → `setting-camera-facing`) and of one choice.
fn setting_id(key: &str) -> String {
    format!("setting-{}", key.replace(['.', '_'], "-"))
}

fn choice_id(key: &str, value: &str) -> String {
    format!("{}--{}", setting_id(key), value.replace(['.', '_'], "-"))
}

/// نص زر الإصلاح لكل نوع مشكلة (None = لا يوجد إصلاح بزر؛ الرسالة في السجل تكفي).
fn fix_message(code: enki_receiver::ProblemCode) -> Option<&'static str> {
    use enki_receiver::ProblemCode::*;
    match code {
        OutputDevice => Some("fix-load-module"),
        NoDevice | ProvisionFailed | AuthRejected | Other => Some("fix-retry"),
        AdbMissing | FfmpegMissing | VersionMismatch | NotPaired | CertificateChanged
        | NoPairedPhone => None,
    }
}

struct App {
    rt: Handle,
    hub: SettingsHandle,
    tx: EventTx,
    window: ApplicationWindow,
    i18n: RefCell<I18n>,
    status: RefCell<Status>,
    log_buffer: TextBuffer,
    controls: RefCell<HashMap<String, Control>>,
    /// True while controls are being set from the stored values, so that does not count as a user change.
    refreshing: Cell<bool>,
    status_label: RefCell<Label>,
    phone_label: RefCell<Label>,
    info_label: RefCell<Label>,
    /// True while the mpv preview window is open.
    preview_open: Cell<bool>,
    start_btn: RefCell<Button>,
    fix_btn: RefCell<Button>,
    resolution: RefCell<DropDown>,
    audio: RefCell<CheckButton>,
    /// مرسل الإيقاف للجلسة الحالية (None = لا يوجد بث).
    stop_slot: RefCell<Option<oneshot::Sender<()>>>,
    /// مرسل أوامر الجلسة الحالية (تغيير الإعدادات أثناء البث).
    control_slot: RefCell<Option<mpsc::UnboundedSender<enki_receiver::Control>>>,
    /// نوع المشكلة التي يعالجها زر الإصلاح الظاهر حالياً.
    fix_slot: RefCell<Option<enki_receiver::ProblemCode>>,
    /// The list of paired phones with its buttons (rebuilt after every change).
    wifi_box: RefCell<GtkBox>,
    /// The running pairing attempt, so closing the code window stops it.
    pair_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    pair_dialog: RefCell<Option<gtk::Window>>,
    /// Drives the waves and the two rivers.
    motion: Rc<Motion>,
}

fn main() -> glib::ExitCode {
    // GTK يملك الخيط الرئيسي، لذلك يعمل Tokio في خيط خلفي منفصل.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("تعذر إنشاء Tokio runtime");
    let handle = rt.handle().clone();

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build_ui(app, handle.clone()));
    // عند الخروج يُسقَط الـ runtime، ومعه تُقتل عمليات scrcpy (kill_on_drop).
    app.run()
}

fn build_ui(app: &Application, rt: Handle) {
    if let Some(display) = gtk::gdk::Display::default() {
        let provider = gtk::CssProvider::new();
        provider.load_from_data(&format!("{TOKENS_CSS}\n{THEME_CSS}"));
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_application_prefer_dark_theme(true);
    }
    let (hub, warning) = Hub::open(Store::new(Store::default_dir()));
    let hub = hub.handle();
    // اكتشاف تلقائي: إن وُجد جهاز v4l2loopback باسم Enki نستخدمه بدل الكتابة اليدوية (إن لم يختر المستخدم جهازاً)
    {
        let mut h = hub.lock().unwrap();
        if h.engine().entry("output.device").is_none() {
            if let Some(found) =
                enki_receiver::pick_default_device(&enki_receiver::list_v4l2_devices())
            {
                let _ = h.set_local("output.device", &json!(found));
            }
        }
    }
    let i18n = I18n::new(&hub.lock().unwrap().get_string("ui.language"));

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Enki")
        .default_width(680)
        .default_height(760)
        .build();
    window.add_css_class("enki");
    // tokio::sync::mpsc لا يحتاج runtime لانتظار الاستقبال، فيمكن انتظاره مباشرة داخل حلقة GLib الرئيسية.
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();

    let app_state = Rc::new(App {
        rt,
        hub,
        tx: tx.clone(),
        window,
        i18n: RefCell::new(i18n),
        status: RefCell::new(Status::Stopped),
        log_buffer: TextBuffer::new(None),
        controls: RefCell::new(HashMap::new()),
        refreshing: Cell::new(false),
        status_label: RefCell::new(Label::new(None)),
        phone_label: RefCell::new(Label::new(None)),
        info_label: RefCell::new(Label::new(None)),
        preview_open: Cell::new(false),
        start_btn: RefCell::new(Button::new()),
        fix_btn: RefCell::new(Button::new()),
        resolution: RefCell::new(DropDown::new(
            None::<gtk::StringList>,
            gtk::Expression::NONE,
        )),
        audio: RefCell::new(CheckButton::new()),
        stop_slot: RefCell::new(None),
        control_slot: RefCell::new(None),
        fix_slot: RefCell::new(None),
        wifi_box: RefCell::new(GtkBox::new(Orientation::Vertical, 6)),
        pair_task: RefCell::new(None),
        pair_dialog: RefCell::new(None),
        motion: Motion::new(),
    });
    if let Some(w) = warning {
        let message = app_state
            .i18n
            .borrow()
            .tr_args("log-settings-warning", &[("message", w.as_str().into())]);
        append_log(&app_state.log_buffer, &format!("⚠️ {message}\n"));
    }
    app_state.rebuild();
    app_state.apply_motion_settings();
    {
        // The water rests while the window is not in front (also when it is minimised).
        let motion = app_state.motion.clone();
        app_state
            .window
            .connect_is_active_notify(move |w| motion.set_active(w.is_active()));
    }

    // --- قناة الأحداث: Tokio -> GTK ---
    {
        let app = app_state.clone();
        glib::spawn_future_local(async move {
            while let Some(event) = rx.recv().await {
                app.handle_event(event);
            }
        });
    }
    app_state.window.present();
}

impl App {
    fn tr(&self, id: &str) -> String {
        self.i18n.borrow().tr(id)
    }

    /// "Reduce motion" is on when the setting says so or when the desktop turned animations off.
    fn apply_motion_settings(&self) {
        let setting = self.hub.lock().unwrap().get_bool("ui.reduce_motion");
        let system_off = gtk::Settings::default()
            .map(|s| !s.is_gtk_enable_animations())
            .unwrap_or(false);
        self.motion.set_reduce(setting || system_off);
    }

    fn log(&self, text: &str) {
        append_log(&self.log_buffer, text);
    }

    /// Builds the window content in the current language. Called at start and whenever the language changes; the log
    /// text, the running session and the stored settings survive it.
    fn rebuild(self: &Rc<Self>) {
        let (lang_rtl, schema) = {
            let i18n = self.i18n.borrow();
            (i18n.rtl(), self.hub.lock().unwrap().schema().clone())
        };
        gtk::Widget::set_default_direction(if lang_rtl {
            gtk::TextDirection::Rtl
        } else {
            gtk::TextDirection::Ltr
        });
        self.controls.borrow_mut().clear();

        let root = GtkBox::new(Orientation::Vertical, 0);
        let main_box = GtkBox::new(Orientation::Vertical, 12);
        main_box.set_vexpand(true);
        for set in [
            GtkBox::set_margin_start,
            GtkBox::set_margin_end,
            GtkBox::set_margin_top,
            GtkBox::set_margin_bottom,
        ] {
            set(&main_box, 16);
        }
        // Header: the seal and the name, then the two rivers (computer and phone).
        let header = GtkBox::new(Orientation::Horizontal, 12);
        if let Ok(pixbuf) = gtk::gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(LOGO_PNG)) {
            let logo = gtk::Image::from_pixbuf(Some(&pixbuf));
            logo.set_pixel_size(44);
            header.append(&logo);
        }
        let title = Label::builder()
            .label(self.tr("app-title"))
            .xalign(0.0)
            .build();
        title.add_css_class("enki-title");
        header.append(&title);
        main_box.append(&header);
        main_box.append(&self.motion.river_area(64));

        // مؤشر حالة البث واسم الهاتف
        let status_label = Label::builder().xalign(0.0).use_markup(true).build();
        let phone_label = Label::builder().xalign(0.0).build();
        status_label.add_css_class("enki-status");
        status_label.set_halign(gtk::Align::Start);
        phone_label.add_css_class("enki-dim");
        let info_label = Label::builder().xalign(0.0).build();
        info_label.add_css_class("enki-dim");
        main_box.append(&status_label);
        main_box.append(&phone_label);
        main_box.append(&info_label);
        *self.info_label.borrow_mut() = info_label;
        *self.status_label.borrow_mut() = status_label;
        *self.phone_label.borrow_mut() = phone_label;

        // الإعدادات: تُبنى من المخطط المشترك، مجموعة بعد مجموعة.
        let settings_box = GtkBox::new(Orientation::Vertical, 12);
        for group in &schema.groups {
            let defs: Vec<_> = schema
                .settings
                .iter()
                .filter(|d| &d.group == group && d.on_desktop())
                .collect();
            if defs.is_empty() {
                continue;
            }
            // Each group is a card: a heading, a line of cuneiform-style marks, then the controls.
            let card = GtkBox::new(Orientation::Vertical, 8);
            card.add_css_class("enki-card");
            card.append(&section_heading(&self.tr(&format!("group-{group}")), group));
            for def in defs {
                if let Some(row) = self.control_row(def) {
                    card.append(&row);
                }
            }
            if group == "link" {
                let wifi = GtkBox::new(Orientation::Vertical, 6);
                *self.wifi_box.borrow_mut() = wifi.clone();
                card.append(&wifi);
                self.refresh_wifi();
            }
            settings_box.append(&card);
        }

        // خيارات scrcpy فقط (ليست جزءاً من المخطط بعد)
        let scrcpy_card = GtkBox::new(Orientation::Vertical, 8);
        scrcpy_card.add_css_class("enki-card");
        scrcpy_card.append(&section_heading("scrcpy", "scrcpy"));
        let res_box = GtkBox::new(Orientation::Horizontal, 10);
        res_box.append(&row_label(&self.tr("label-resolution")));
        let resolution = DropDown::new(
            Some(gtk::StringList::new(&[
                "1920x1080",
                "1280x720",
                "3840x2160 (4K)",
            ])),
            gtk::Expression::NONE,
        );
        resolution.set_selected(self.resolution.borrow().selected().min(2));
        res_box.append(&resolution);
        scrcpy_card.append(&res_box);
        *self.resolution.borrow_mut() = resolution;
        let audio = CheckButton::builder()
            .label(self.tr("check-audio"))
            .active(self.audio.borrow().is_active())
            .build();
        scrcpy_card.append(&audio);
        settings_box.append(&scrcpy_card);
        *self.audio.borrow_mut() = audio;
        // الإعدادات كثيرة: تُعرض داخل منطقة تمرير كي لا تطول النافذة.
        main_box.append(
            &ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .min_content_height(300)
                .vexpand(true)
                .child(&settings_box)
                .build(),
        );

        // أزرار التحكم
        let action_box = GtkBox::new(Orientation::Horizontal, 10);
        let modprobe_btn = Button::builder().label(self.tr("button-modprobe")).build();
        let start_btn = Button::builder()
            .label(self.tr("button-start"))
            .sensitive(self.stop_slot.borrow().is_none())
            .build();
        start_btn.add_css_class("enki-primary");
        let stop_btn = Button::builder().label(self.tr("button-stop")).build();
        // يظهر فقط بعد مشكلة لها إصلاح (انظر fix_message).
        let fix_btn = Button::builder().visible(false).build();
        fix_btn.add_css_class("enki-fix");
        if let Some(code) = *self.fix_slot.borrow() {
            if let Some(id) = fix_message(code) {
                fix_btn.set_label(&self.tr(id));
                fix_btn.set_visible(true);
            }
        }
        let preview_btn = Button::builder().label(self.tr("button-preview")).build();
        for b in [&modprobe_btn, &start_btn, &stop_btn, &preview_btn, &fix_btn] {
            action_box.append(b);
        }
        main_box.append(&action_box);
        *self.start_btn.borrow_mut() = start_btn.clone();
        *self.fix_btn.borrow_mut() = fix_btn.clone();

        // السجلات
        let log_card = GtkBox::new(Orientation::Vertical, 6);
        log_card.add_css_class("enki-card");
        log_card.set_vexpand(true);
        log_card.append(&section_heading(&self.tr("log-title"), "log"));
        let text_view = TextView::builder()
            .buffer(&self.log_buffer)
            .editable(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .build();
        text_view.add_css_class("enki-log");
        text_view.set_left_margin(6);
        text_view.set_top_margin(4);
        log_card.append(
            &ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Automatic)
                .vscrollbar_policy(gtk::PolicyType::Automatic)
                .min_content_height(110)
                .child(&text_view)
                .vexpand(true)
                .build(),
        );
        main_box.append(&log_card);
        root.append(&main_box);
        root.append(&self.motion.wave_area(76));
        self.window.set_child(Some(&root));

        // --- ربط الأزرار ---
        {
            let app = self.clone();
            modprobe_btn.connect_clicked(move |_| app.load_module_from_settings());
            let app = self.clone();
            preview_btn.connect_clicked(move |_| app.open_preview());
        }
        {
            let app = self.clone();
            fix_btn.connect_clicked(move |btn| {
                let problem = app.fix_slot.borrow_mut().take();
                btn.set_visible(false);
                match problem {
                    Some(enki_receiver::ProblemCode::OutputDevice) => {
                        app.load_module_from_settings()
                    }
                    Some(_) => app.start_btn.borrow().emit_clicked(),
                    None => {}
                }
            });
        }
        {
            let app = self.clone();
            start_btn.connect_clicked(move |_| app.start());
        }
        {
            let app = self.clone();
            stop_btn.connect_clicked(move |_| match app.stop_slot.borrow_mut().take() {
                Some(stop_tx) => {
                    let _ = stop_tx.send(());
                }
                None => app.log(&format!("{}\n", app.tr("log-no-stream"))),
            });
        }

        self.refresh_controls();
        self.render_status();
    }

    /// One row for a setting, or `None` for kinds this window has no control for yet.
    fn control_row(self: &Rc<Self>, def: &enki_receiver::enki_settings::Def) -> Option<GtkBox> {
        let row = GtkBox::new(Orientation::Horizontal, 10);
        let key = def.key.clone();
        let label = self.tr(&setting_id(&key));
        match &def.kind {
            Kind::Enum(values) => {
                row.append(&row_label(&label));
                let names: Vec<String> = values
                    .iter()
                    .map(|v| self.tr(&choice_id(&key, v)))
                    .collect();
                let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                let dropdown =
                    DropDown::new(Some(gtk::StringList::new(&refs)), gtk::Expression::NONE);
                row.append(&dropdown);
                let app = self.clone();
                let (k, vals) = (key.clone(), values.clone());
                dropdown.connect_selected_notify(move |dd| {
                    if let Some(value) = vals.get(dd.selected() as usize) {
                        app.user_set(&k, json!(value));
                    }
                });
                self.controls
                    .borrow_mut()
                    .insert(key, Control::Choice(dropdown, values.clone()));
            }
            Kind::Bool => {
                let check = CheckButton::builder().label(label).build();
                row.append(&check);
                let app = self.clone();
                let k = key.clone();
                check.connect_toggled(move |c| app.user_set(&k, json!(c.is_active())));
                self.controls
                    .borrow_mut()
                    .insert(key, Control::Toggle(check));
            }
            Kind::Str { .. } => {
                row.append(&row_label(&label));
                let entry = Entry::builder().hexpand(true).build();
                row.append(&entry);
                let app = self.clone();
                let k = key.clone();
                entry.connect_changed(move |e| app.user_set(&k, json!(e.text().to_string())));
                self.controls.borrow_mut().insert(key, Control::Text(entry));
            }
            Kind::Int { min, max, step } => {
                let scale = Scale::with_range(
                    Orientation::Horizontal,
                    *min as f64,
                    *max as f64,
                    (*step).max(1) as f64,
                );
                self.slider_row(&row, &label, scale, key, 0);
            }
            Kind::Float { min, max } => {
                let scale =
                    Scale::with_range(Orientation::Horizontal, *min, *max, (max - min) / 100.0);
                self.slider_row(&row, &label, scale, key, 2);
            }
        }
        Some(row)
    }

    /// A slider for a number. The value is sent 250 ms after the last movement, so dragging does not flood the phone.
    fn slider_row(
        self: &Rc<Self>,
        row: &GtkBox,
        label: &str,
        scale: Scale,
        key: String,
        digits: i32,
    ) {
        row.append(&row_label(label));
        scale.set_hexpand(true);
        scale.set_digits(digits);
        scale.set_draw_value(true);
        scale.set_value_pos(gtk::PositionType::Right);
        row.append(&scale);
        let app = self.clone();
        let k = key.clone();
        let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        scale.connect_value_changed(move |sc| {
            if app.refreshing.get() {
                return;
            }
            if let Some(id) = pending.borrow_mut().take() {
                id.remove();
            }
            let (app, k, sc, slot) = (app.clone(), k.clone(), sc.clone(), pending.clone());
            let id =
                glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
                    slot.borrow_mut().take();
                    let v = sc.value();
                    app.user_set(
                        &k,
                        if digits == 0 {
                            json!(v.round() as i64)
                        } else {
                            json!((v * 100.0).round() / 100.0)
                        },
                    );
                });
            *pending.borrow_mut() = Some(id);
        });
        self.controls
            .borrow_mut()
            .insert(key, Control::Slider(scale));
    }

    /// Sets every control from the stored values (without counting that as a user change).
    fn refresh_controls(&self) {
        self.refreshing.set(true);
        {
            let hub = self.hub.lock().unwrap();
            for (key, control) in self.controls.borrow().iter() {
                let Some(value) = hub.get(key) else { continue };
                match control {
                    Control::Choice(dd, values) => {
                        if let Some(i) = value
                            .as_str()
                            .and_then(|s| values.iter().position(|v| v == s))
                        {
                            if dd.selected() as usize != i {
                                dd.set_selected(i as u32);
                            }
                        }
                    }
                    Control::Toggle(c) => c.set_active(value.as_bool().unwrap_or(false)),
                    Control::Slider(sc) => {
                        if let Some(v) = value.as_f64() {
                            if (sc.value() - v).abs() > 1e-9 {
                                sc.set_value(v);
                            }
                        }
                    }
                    Control::Text(e) => {
                        let text = value.as_str().unwrap_or("");
                        if e.text() != text {
                            e.set_text(text);
                        }
                    }
                }
            }
            let name = hub.phone_name();
            self.phone_label.borrow().set_label(&if name.is_empty() {
                String::new()
            } else {
                self.i18n
                    .borrow()
                    .tr_args("status-phone", &[("name", name.into())])
            });
        }
        self.refreshing.set(false);
    }

    /// A change made with a control. Synced settings go through the running session, which stores them, applies them
    /// and sends them to the phone; otherwise (no session, or a setting of this computer) they are stored here and
    /// travel in the exchange at the next connection.
    fn user_set(self: &Rc<Self>, key: &str, value: Value) {
        if self.refreshing.get() {
            return;
        }
        let synced = self
            .hub
            .lock()
            .unwrap()
            .schema()
            .get(key)
            .map(|d| d.scope == Scope::Synced)
            .unwrap_or(false);
        if synced {
            if let Some(ctl) = self.control_slot.borrow().as_ref() {
                let _ = ctl.send(enki_receiver::Control::Set(key.to_string(), value));
                return;
            }
        }
        let result = self.hub.lock().unwrap().set_local(key, &value);
        if let Err(reject) = result {
            self.log(&format!("⚠️ {}\n", reject.message));
            self.refresh_controls();
        }
        if let Some(error) = self.hub.lock().unwrap().take_save_error() {
            self.log(&format!("⚠️ {error}\n"));
        }
        if key == "ui.reduce_motion" {
            self.apply_motion_settings();
        }
        if key == "ui.language" {
            let lang = self.hub.lock().unwrap().get_string("ui.language");
            *self.i18n.borrow_mut() = I18n::new(&lang);
            // Rebuild after this handler returns: the control that fired it is destroyed by the rebuild.
            let app = self.clone();
            glib::idle_add_local_once(move || app.rebuild());
        }
    }

    fn render_status(&self) {
        let i18n = self.i18n.borrow();
        let text = match &*self.status.borrow() {
            Status::Stopped => i18n.tr("status-stopped"),
            Status::Preparing => format!("🟡 {}", i18n.tr("status-preparing")),
            Status::WaitingApp => format!("🟡 {}", i18n.tr("status-waiting-app")),
            Status::OpeningCamera => format!("🟡 {}", i18n.tr("status-opening-camera")),
            Status::Connected => format!("🟢 {}", i18n.tr("status-connected")),
            Status::Reconnecting => format!("🟠 {}", i18n.tr("status-reconnecting")),
            Status::Streaming {
                fps,
                mbps,
                rtt_ms,
                target_mbps,
            } => {
                let mut text = format!(
                    "🟢 {}",
                    i18n.tr_args(
                        "status-streaming",
                        &[
                            ("fps", (*fps as i64).into()),
                            ("mbps", format!("{mbps:.1}").into())
                        ]
                    )
                );
                if let Some(ms) = rtt_ms {
                    text.push_str(&format!(
                        " · {}",
                        i18n.tr_args("status-rtt", &[("ms", (*ms as i64).into())])
                    ));
                }
                if let Some(target) = target_mbps {
                    text.push_str(&format!(
                        " · {}",
                        i18n.tr_args("status-target", &[("mbps", format!("{target:.1}").into())])
                    ));
                }
                text
            }
        };
        let dot = if matches!(*self.status.borrow(), Status::Stopped) {
            "⚪ "
        } else {
            ""
        };
        let (class, visual) = match &*self.status.borrow() {
            Status::Stopped => ("idle", Visual::Idle),
            Status::Preparing
            | Status::WaitingApp
            | Status::OpeningCamera
            | Status::Reconnecting => ("busy", Visual::Linking),
            Status::Connected | Status::Streaming { .. } => ("live", Visual::Live),
        };
        {
            let label = self.status_label.borrow();
            for c in ["idle", "busy", "live"] {
                label.remove_css_class(c);
            }
            label.add_css_class(class);
        }
        self.motion.set_visual(visual);
        self.status_label.borrow().set_markup(&format!(
            "<b>{}: {dot}{}</b>",
            i18n.tr("status-label"),
            glib::markup_escape_text(&text)
        ));
    }

    fn handle_event(self: &Rc<Self>, event: UiEvent) {
        match event {
            UiEvent::Log(text) => self.log(&text),
            UiEvent::Status(status) => {
                *self.status.borrow_mut() = status;
                self.render_status();
            }
            UiEvent::Problem(problem) => match fix_message(problem.code) {
                Some(id) => {
                    self.fix_btn.borrow().set_label(&self.tr(id));
                    self.fix_btn.borrow().set_visible(true);
                    *self.fix_slot.borrow_mut() = Some(problem.code);
                }
                None => self.fix_btn.borrow().set_visible(false),
            },
            UiEvent::Running(running) => {
                if running {
                    self.fix_btn.borrow().set_visible(false); // جلسة جديدة: المشكلة السابقة لم تعد قائمة
                } else {
                    self.stop_slot.borrow_mut().take();
                    self.control_slot.borrow_mut().take();
                    *self.status.borrow_mut() = Status::Stopped;
                    self.info_label.borrow().set_label("");
                }
                self.start_btn.borrow().set_sensitive(!running);
                if running && matches!(*self.status.borrow(), Status::Stopped) {
                    *self.status.borrow_mut() = Status::Preparing;
                }
                self.render_status();
            }
            UiEvent::Refresh => self.refresh_controls(),
            UiEvent::Paired(result) => match result {
                Ok(phone) => {
                    let name = phone.name.clone();
                    match enki_receiver::PhoneBook::default_book().upsert(phone) {
                        Ok(()) => self.log(&format!("✓ {} {name}\n", self.tr("log-paired"))),
                        Err(e) => self.log(&format!("❌ phones.toml: {e}\n")),
                    }
                    self.refresh_wifi();
                }
                Err(reason) => self.log(&format!("❌ {} {reason}\n", self.tr("log-pair-failed"))),
            },
            UiEvent::CloseDialog => {
                let dialog = self.pair_dialog.borrow_mut().take();
                if let Some(d) = dialog {
                    d.close();
                }
            }
            UiEvent::WifiHost { id, host } => {
                let _ = enki_receiver::PhoneBook::default_book().set_last_host(&id, &host);
            }
            UiEvent::Phone(info) => {
                let mut parts: Vec<String> = Vec::new();
                if let Some(b) = info.battery {
                    parts.push(self.i18n.borrow().tr_args(
                        "status-battery",
                        &[("percent", b.to_string().as_str().into())],
                    ));
                }
                if let Some(t) = info.temp_dc {
                    parts.push(self.i18n.borrow().tr_args(
                        "status-temperature",
                        &[("degrees", format!("{:.1}", t as f64 / 10.0).as_str().into())],
                    ));
                }
                if info.charging == Some(true) {
                    parts.push(self.tr("status-charging"));
                }
                self.info_label.borrow().set_label(&parts.join(" · "));
            }
            UiEvent::PreviewClosed => self.preview_open.set(false),
            UiEvent::RolledBack { key, message } => {
                self.refresh_controls();
                let setting = self.tr(&setting_id(&key));
                let text = self.i18n.borrow().tr_args(
                    "log-setting-rolled-back",
                    &[
                        ("setting", setting.as_str().into()),
                        ("message", message.as_str().into()),
                    ],
                );
                self.log(&format!("⚠️ {text}\n"));
            }
        }
        if let Some(error) = self.hub.lock().unwrap().take_save_error() {
            self.log(&format!("⚠️ {error}\n"));
        }
    }

    /// Shows what programs get from the virtual camera, in an mpv window reading the V4L2 device itself, so it
    /// is exactly the final picture (rotation and filters included) and costs no extra decoding.
    fn open_preview(self: &Rc<Self>) {
        if self.preview_open.get() {
            return;
        }
        self.preview_open.set(true);
        let device = self.output_device();
        let tx = self.tx.clone();
        let missing = self.tr("log-preview-missing");
        let failed = self.tr("log-preview-failed");
        self.rt.spawn(async move {
            let child = Command::new("mpv")
                .args([
                    "--profile=low-latency",
                    "--untimed",
                    "--no-cache",
                    "--no-audio",
                    "--force-window=immediate",
                    "--title=Enki",
                ])
                .arg(format!("av://v4l2:{device}"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn();
            match child {
                Ok(mut child) => {
                    if let Ok(status) = child.wait().await {
                        if !status.success() {
                            let _ = tx.send(UiEvent::Log(format!("{failed}\n")));
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let _ = tx.send(UiEvent::Log(format!("{missing}\n")));
                }
                Err(e) => {
                    let _ = tx.send(UiEvent::Log(format!("mpv: {e}\n")));
                }
            }
            let _ = tx.send(UiEvent::PreviewClosed);
        });
    }

    /// This computer's name, shown on the phone when pairing.
    fn pc_name() -> String {
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "computer".into())
    }

    /// Rebuilds the paired-phones list and the two pairing buttons.
    fn refresh_wifi(self: &Rc<Self>) {
        let wifi = self.wifi_box.borrow().clone();
        while let Some(child) = wifi.first_child() {
            wifi.remove(&child);
        }
        wifi.append(&row_label(&self.tr("wifi-phones")));
        let phones = enki_receiver::PhoneBook::default_book().load();
        if phones.is_empty() {
            let none = Label::new(Some(self.tr("wifi-no-phones").as_str()));
            none.set_xalign(0.0);
            none.add_css_class("enki-dim");
            wifi.append(&none);
        }
        for phone in phones {
            let row = GtkBox::new(Orientation::Horizontal, 10);
            let name = Label::new(Some(phone.name.as_str()));
            name.set_xalign(0.0);
            name.set_hexpand(true);
            row.append(&name);
            let forget = Button::with_label(&self.tr("wifi-forget"));
            let app = self.clone();
            let id = phone.id.clone();
            forget.connect_clicked(move |_| {
                let _ = enki_receiver::PhoneBook::default_book().remove(&id);
                app.refresh_wifi();
            });
            row.append(&forget);
            wifi.append(&row);
        }
        let buttons = GtkBox::new(Orientation::Horizontal, 10);
        let usb = Button::with_label(&self.tr("wifi-pair-usb"));
        let code = Button::with_label(&self.tr("wifi-pair-code"));
        let app = self.clone();
        usb.connect_clicked(move |_| app.pair_over_usb());
        let app = self.clone();
        code.connect_clicked(move |_| app.open_pair_dialog());
        buttons.append(&usb);
        buttons.append(&code);
        wifi.append(&buttons);
    }

    fn pc_id(&self) -> String {
        self.hub.lock().unwrap().device_id().to_string()
    }

    fn pair_over_usb(self: &Rc<Self>) {
        let (name, id, tx) = (Self::pc_name(), self.pc_id(), self.tx.clone());
        self.rt.spawn(async move {
            let port = enki_receiver::DEFAULT_PORT;
            let result = enki_receiver::pair_over_adb("adb", port, &name, &id)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::Paired(result));
        });
    }

    /// Shows a one-time code and waits for the phone that has it typed in.
    fn open_pair_dialog(self: &Rc<Self>) {
        let mut random = [0u8; enki_receiver::pairing::CODE_LEN];
        let filled = std::fs::File::open("/dev/urandom")
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut random));
        if filled.is_err() {
            self.log("❌ /dev/urandom\n");
            return;
        }
        let code = enki_receiver::pairing::code_from_random(&random);
        let dialog = gtk::Window::builder()
            .transient_for(&self.window)
            .modal(true)
            .title(self.tr("wifi-dialog-title"))
            .default_width(440)
            .build();
        dialog.add_css_class("enki");
        let body = GtkBox::new(Orientation::Vertical, 12);
        body.set_margin_top(18);
        body.set_margin_bottom(18);
        body.set_margin_start(18);
        body.set_margin_end(18);
        let help = Label::new(Some(self.tr("wifi-dialog-help").as_str()));
        help.set_wrap(true);
        help.set_xalign(0.0);
        let big = Label::new(None);
        big.set_markup(&format!(
            "<span size=\"xx-large\" weight=\"bold\">{code}</span>"
        ));
        big.set_selectable(true);
        let waiting = Label::new(Some(self.tr("wifi-dialog-waiting").as_str()));
        waiting.add_css_class("enki-dim");
        let address = Entry::builder()
            .placeholder_text(self.tr("wifi-dialog-address"))
            .build();
        let cancel = Button::with_label(&self.tr("wifi-cancel"));
        for w in [
            help.upcast_ref::<gtk::Widget>(),
            big.upcast_ref(),
            waiting.upcast_ref(),
            address.upcast_ref(),
            cancel.upcast_ref(),
        ] {
            body.append(w);
        }
        dialog.set_child(Some(&body));

        let start = {
            let app = self.clone();
            let code = code.clone();
            move |manual: Option<std::net::SocketAddr>| {
                if let Some(old) = app.pair_task.borrow_mut().take() {
                    old.abort();
                }
                let (name, id, tx) = (App::pc_name(), app.pc_id(), app.tx.clone());
                let code = code.clone();
                let handle = app.rt.spawn(async move {
                    let result = enki_receiver::pair_with_code(
                        &code,
                        &name,
                        &id,
                        manual,
                        enki_receiver::PAIR_WINDOW,
                    )
                    .await
                    .map_err(|e| e.to_string());
                    let _ = tx.send(UiEvent::Paired(result));
                    let _ = tx.send(UiEvent::CloseDialog);
                });
                *app.pair_task.borrow_mut() = Some(handle);
            }
        };
        start(None);
        {
            let start = start.clone();
            address.connect_activate(move |e| {
                let text = e.text().to_string();
                let text = text.trim();
                let with_port = if text.contains(':') {
                    text.to_string()
                } else {
                    format!("{text}:{}", enki_receiver::WIFI_PORT)
                };
                if let Ok(addr) = with_port.parse() {
                    start(Some(addr));
                }
            });
        }
        {
            let dialog = dialog.clone();
            cancel.connect_clicked(move |_| dialog.close());
        }
        {
            let app = self.clone();
            dialog.connect_close_request(move |_| {
                if let Some(task) = app.pair_task.borrow_mut().take() {
                    task.abort();
                }
                *app.pair_dialog.borrow_mut() = None;
                glib::Propagation::Proceed
            });
        }
        *self.pair_dialog.borrow_mut() = Some(dialog.clone());
        dialog.present();
    }

    fn output_device(&self) -> String {
        self.hub.lock().unwrap().get_string("output.device")
    }

    fn load_module_from_settings(&self) {
        let device = self.output_device();
        match parse_video_nr(&device) {
            Some(nr) => {
                self.rt.spawn(load_module(nr, self.tx.clone()));
            }
            None => self.invalid_device(&device),
        }
    }

    fn invalid_device(&self, device: &str) {
        let text = self
            .i18n
            .borrow()
            .tr_args("log-invalid-device", &[("device", device.into())]);
        self.log(&format!("❌ {text}\n"));
    }

    /// 2. بدء البث
    fn start(self: &Rc<Self>) {
        if self.stop_slot.borrow().is_some() {
            return; // يوجد بث شغال أصلاً
        }
        let (source, facing, device) = {
            let hub = self.hub.lock().unwrap();
            (
                hub.get_string("source.mode"),
                hub.get_string("camera.facing"),
                hub.get_string("output.device"),
            )
        };
        let tx = self.tx.clone();
        if source != "screen_window" && parse_video_nr(&device).is_none() {
            self.invalid_device(&device);
            return;
        }
        if source == "enki_camera" {
            // كاميرا تطبيق Enki: يستقبلها enki-receiver ويمررها إلى V4L2 (تعمل من أندرويد 7).
            // كل الإعدادات (الكاميرا، التدوير، المرآة) تُقرأ من الإعدادات المشتركة نفسها.
            let mut cfg = enki_receiver::Config::new(device.clone());
            cfg.facing = if facing == "front" {
                enki_receiver::Facing::Front
            } else {
                enki_receiver::Facing::Back
            };
            cfg.settings = self.hub.clone();
            cfg.transport = enki_receiver::Transport::from_setting(
                &self.hub.lock().unwrap().get_string("link.transport"),
            );
            // The phone used over Wi-Fi is the one paired last.
            cfg.wifi = enki_receiver::PhoneBook::default_book().load().pop();
            let text = self.i18n.borrow().tr_args(
                "log-starting-enki",
                &[
                    ("facing", facing.as_str().into()),
                    ("device", device.as_str().into()),
                ],
            );
            self.log(&format!("> {text}\n"));
            let (stop_tx, stop_rx) = oneshot::channel();
            *self.stop_slot.borrow_mut() = Some(stop_tx);
            let _ = tx.send(UiEvent::Running(true));
            let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
            let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
            *self.control_slot.borrow_mut() = Some(ctl_tx);
            self.rt.spawn(enki_receiver::run_with_commands(
                cfg, ev_tx, stop_rx, ctl_rx,
            ));
            self.rt.spawn(async move {
                use enki_receiver::{Event, LinkState, State};
                while let Some(event) = ev_rx.recv().await {
                    let ui_event = match event {
                        Event::Log(text) => Some(UiEvent::Log(text)),
                        Event::Link(LinkState::Preparing) => {
                            Some(UiEvent::Status(Status::Preparing))
                        }
                        Event::Link(LinkState::WaitingForApp) => {
                            Some(UiEvent::Status(Status::WaitingApp))
                        }
                        Event::Link(LinkState::Ready) => {
                            Some(UiEvent::Status(Status::OpeningCamera))
                        }
                        Event::Link(LinkState::Streaming) => {
                            Some(UiEvent::Status(Status::Connected))
                        }
                        Event::Link(LinkState::Reconnecting(_)) => {
                            Some(UiEvent::Status(Status::Reconnecting))
                        }
                        Event::State(State::Stopped) => Some(UiEvent::Running(false)),
                        Event::State(_) | Event::Capabilities(_) => None,
                        Event::Phone(info) => Some(UiEvent::Phone(info)),
                        Event::Problem(problem) => Some(UiEvent::Problem(problem)),
                        Event::WifiConnected { id, host } => Some(UiEvent::WifiHost { id, host }),
                        Event::Setting { .. } => Some(UiEvent::Refresh),
                        Event::SettingRolledBack { key, message, .. } => {
                            Some(UiEvent::RolledBack { key, message })
                        }
                        Event::Stats(s) => Some(UiEvent::Status(Status::Streaming {
                            fps: s.fps,
                            mbps: s.kbps as f64 / 1000.0,
                            rtt_ms: s.rtt_ms,
                            target_mbps: s.bitrate_kbps.map(|k| k as f64 / 1000.0),
                        })),
                    };
                    if let Some(ui_event) = ui_event {
                        let _ = tx.send(ui_event);
                    }
                }
            });
            return;
        }

        let size = match self.resolution.borrow().selected() {
            1 => "1280x720",
            2 => "3840x2160",
            _ => "1920x1080",
        };
        let mut args: Vec<String>;
        let what;
        if source == "scrcpy_camera" {
            args = vec![
                "--video-source=camera".to_string(),
                format!("--camera-facing={facing}"),
                format!("--camera-size={size}"),
                format!("--v4l2-sink={device}"),
                // لا نافذة معاينة (يُبقي الصوت شغال إذا فُعّل)
                "--no-video-playback".to_string(),
            ];
            what = format!("camera: {facing}, {size}, {device}");
        } else {
            // الشاشة: --max-size هو أكبر بُعد للصورة
            let max = size.split('x').next().unwrap_or("1920");
            args = vec![format!("--max-size={max}")];
            if source == "screen_v4l2" {
                args.push(format!("--v4l2-sink={device}"));
                args.push("--no-video-playback".to_string());
                what = format!("screen → {device}, max {max}");
            } else {
                what = format!("screen → window, max {max}");
            }
        }
        if self.audio.borrow().is_active() {
            args.push("--audio-source=mic".to_string());
        } else {
            args.push("--no-audio".to_string());
        }
        self.log(&format!("> scrcpy ({what})...\n"));
        let (stop_tx, stop_rx) = oneshot::channel();
        *self.stop_slot.borrow_mut() = Some(stop_tx);
        let _ = tx.send(UiEvent::Running(true));
        self.rt.spawn(run_scrcpy(args, stop_rx, tx));
    }
}

/// تحميل وحدة النواة عبر pkexec (نافذة مصادقة رسومية؛ sudo لا يعمل بدون طرفية).
async fn load_module(nr: u32, tx: EventTx) {
    let nr_arg = format!("video_nr={nr}");
    log(
        &tx,
        format!("> pkexec modprobe v4l2loopback ({nr_arg}) — قد تظهر نافذة إدخال كلمة المرور...\n"),
    );

    let result = Command::new("pkexec")
        .args([
            "modprobe",
            "v4l2loopback",
            "devices=1",
            &nr_arg,
            "card_label=Enki",
            "exclusive_caps=1",
        ])
        .stdin(Stdio::null())
        .output()
        .await;

    match result {
        Ok(out) if out.status.success() => {
            log(&tx, format!("✓ تم تفعيل v4l2loopback على /dev/video{nr}\n"));
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let hint = match out.status.code() {
                Some(126) | Some(127) => " (أُلغيت المصادقة أو رُفضت)",
                _ => "",
            };
            log(
                &tx,
                format!("❌ فشل تفعيل الدرايفر{hint}: {}\n", stderr.trim()),
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => log(
            &tx,
            "❌ الأمر pkexec غير موجود. ثبّت polkit، أو نفّذ يدوياً: sudo modprobe v4l2loopback\n"
                .to_string(),
        ),
        Err(e) => log(&tx, format!("❌ تعذر تنفيذ pkexec: {e}\n")),
    }
}

/// تشغيل scrcpy ومراقبته حتى ينتهي أو يُطلب إيقافه.
async fn run_scrcpy(args: Vec<String>, mut stop_rx: oneshot::Receiver<()>, tx: EventTx) {
    let spawned = Command::new("scrcpy")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();

    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                "❌ scrcpy غير مثبت أو غير موجود في PATH.\n".to_string()
            } else {
                format!("❌ فشل تشغيل scrcpy: {e}\n")
            };
            log(&tx, msg);
            let _ = tx.send(UiEvent::Running(false));
            return;
        }
    };
    let _ = tx.send(UiEvent::Status(Status::Connected));

    let out_task = child
        .stdout
        .take()
        .map(|s| tokio::spawn(pump_lines(s, "", tx.clone())));
    let err_task = child
        .stderr
        .take()
        .map(|s| tokio::spawn(pump_lines(s, "⚠️ ", tx.clone())));

    tokio::select! {
        status = child.wait() => match status {
            Ok(s) if s.success() => log(&tx, "■ انتهى scrcpy.\n".to_string()),
            Ok(s) => log(&tx, format!("❌ توقف scrcpy بشكل غير متوقع ({s}). راجع السجل أعلاه.\n")),
            Err(e) => log(&tx, format!("❌ خطأ أثناء انتظار scrcpy: {e}\n")),
        },
        _ = &mut stop_rx => {
            // kill() يقتل العملية وينتظرها فلا تبقى zombie
            match child.kill().await {
                Ok(()) => log(&tx, "🛑 تم إيقاف البث.\n".to_string()),
                Err(e) => log(&tx, format!("خطأ أثناء إيقاف scrcpy: {e}\n")),
            }
        }
    }

    // اترك قارئي المخرجات ينهون ما تبقى قبل إعلان التوقف
    for task in [out_task, err_task].into_iter().flatten() {
        let _ = task.await;
    }
    let _ = tx.send(UiEvent::Running(false));
}

async fn pump_lines<R: AsyncRead + Unpin>(reader: R, prefix: &'static str, tx: EventTx) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        log(&tx, format!("{prefix}{line}\n"));
    }
}

fn log(tx: &EventTx, text: String) {
    let _ = tx.send(UiEvent::Log(text));
}

fn parse_video_nr(device: &str) -> Option<u32> {
    device.trim().strip_prefix("/dev/video")?.parse().ok()
}

/// A group heading with a line of cuneiform-style marks under it.
fn section_heading(text: &str, seed: &str) -> GtkBox {
    let b = GtkBox::new(Orientation::Vertical, 2);
    let label = Label::builder().label(text).xalign(0.0).build();
    label.add_css_class("enki-heading");
    b.append(&label);
    b.append(&Motion::divider_area(seed));
    b
}

fn row_label(text: &str) -> Label {
    Label::builder()
        .label(text)
        .xalign(0.0)
        .width_request(160)
        .build()
}

fn append_log(buffer: &TextBuffer, text: &str) {
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_ids_follow_the_documented_rule() {
        assert_eq!(setting_id("camera.facing"), "setting-camera-facing");
        assert_eq!(setting_id("video.auto_rotate"), "setting-video-auto-rotate");
        assert_eq!(
            choice_id("source.mode", "enki_camera"),
            "setting-source-mode--enki-camera"
        );
        assert_eq!(choice_id("video.rotate", "90"), "setting-video-rotate--90");
    }

    /// Every setting the window shows has its texts: the same rule as tools/check_i18n.py, checked from Rust too.
    #[test]
    fn every_desktop_setting_and_choice_has_a_translation() {
        let schema = enki_receiver::enki_settings::Schema::builtin();
        for (lang, _, _) in enki_i18n::LANGUAGES.iter() {
            let i18n = I18n::new(lang);
            for def in schema.settings.iter().filter(|d| d.on_desktop()) {
                assert_ne!(
                    i18n.tr(&setting_id(&def.key)),
                    setting_id(&def.key),
                    "{lang}: label of {}",
                    def.key
                );
                if let Kind::Enum(values) = &def.kind {
                    for v in values {
                        let id = choice_id(&def.key, v);
                        assert_ne!(i18n.tr(&id), id, "{lang}: {id}");
                    }
                }
                let heading = format!("group-{}", def.group);
                assert_ne!(i18n.tr(&heading), heading, "{lang}: {heading}");
            }
        }
    }
}
