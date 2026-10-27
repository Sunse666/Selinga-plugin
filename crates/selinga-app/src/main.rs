#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui;
#[cfg(windows)]
use selinga_capture::ScreenCapture;
#[cfg(windows)]
use selinga_ocr::Ocr;
use selinga_pipeline::PipelineOutput;
use selinga_translate::Translator;
use std::sync::Arc;
#[cfg(windows)]
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

mod settings;
#[cfg(windows)]
mod plugin;
mod tray;
use settings::Settings;
#[cfg(windows)]
use tray::{Tray, TrayCommand};

const DEBOUNCE: Duration = Duration::from_millis(700);
const POLL_INTERVAL: Duration = Duration::from_millis(1000);
const EXCLUDE_INTERVAL: Duration = Duration::from_millis(500);
const HANDLE_MARGIN: f32 = 10.0;
fn mask_color() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(16, 16, 22, 230)
}
const TEXT_COLOR: egui::Color32 = egui::Color32::from_rgb(0xF2, 0xF2, 0xF5);
const SOURCE_COLOR: egui::Color32 = egui::Color32::from_rgb(0x9A, 0x9A, 0xA8);
const LINE_PAD: f32 = 3.0;
const TARGET_LANGS: [&str; 4] = ["zh-CN", "en-US", "ja-JP", "ko-KR"];
const MIN_SELECT: f32 = 80.0;
const DEFAULT_SIZE: [f32; 2] = [380.0, 240.0];
const BOX_SHOW_AT_FRAME: u32 = 3;
const BOX_HANDOFF_AT_FRAME: u32 = 8;
const PIPELINE_TIMEOUT: Duration = Duration::from_secs(25);
const AI_TIMEOUT: Duration = Duration::from_secs(120);
const SETTINGS_CHECK_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(windows)]
const LIVE_INTERVAL_ACTIVE: Duration = Duration::from_millis(100);
#[cfg(windows)]
const LIVE_INTERVAL_IDLE: Duration = Duration::from_millis(500);
#[cfg(windows)]
const LIVE_ACTIVE_WINDOW: Duration = Duration::from_secs(5);

fn build_translator(target: &str) -> Arc<dyn Translator> {
    let mut backends: Vec<Box<dyn Translator>> = vec![Box::new(
        selinga_translate::tencent::TransmartTranslator::new(target),
    )];
    backends.push(Box::new(selinga_translate::google::GoogleTranslator::new(
        target,
    )));
    backends.push(Box::new(
        selinga_translate::mymemory::MyMemoryTranslator::new("en-US", target),
    ));
    Arc::new(selinga_translate::CachedTranslator::new(
        selinga_translate::FallbackTranslator::new(backends),
        2048,
    ))
}

type ToolbarAction = Box<dyn Fn(&mut SelingaApp, &egui::Ui)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolbarIcon {
    Mode,
    Copy,
    Lang,
    Refresh,
    Ai,
    Pause,
    Resume,
}

#[cfg(windows)]
#[derive(Debug, Clone)]
struct FrameCache {
    hash: u64,
    output: PipelineOutput,
    ai: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayMode {
    Translation,
    Bilingual,
}

impl DisplayMode {
    fn toggle(self) -> Self {
        match self {
            DisplayMode::Translation => DisplayMode::Bilingual,
            DisplayMode::Bilingual => DisplayMode::Translation,
        }
    }
}

fn selection_region(marquee: egui::Rect, origin: (i32, i32), ppp: f32) -> selinga_capture::Region {
    selinga_capture::Region {
        x: origin.0 + (marquee.min.x * ppp).round() as i32,
        y: origin.1 + (marquee.min.y * ppp).round() as i32,
        w: (marquee.width() * ppp).round().max(1.0) as u32,
        h: (marquee.height() * ppp).round().max(1.0) as u32,
    }
}

fn region_to_logical(region: selinga_capture::Region, scale: f32) -> (egui::Pos2, egui::Vec2) {
    let scale = scale.max(0.1);
    (
        egui::pos2(region.x as f32 / scale, region.y as f32 / scale),
        egui::vec2(region.w as f32 / scale, region.h as f32 / scale),
    )
}

fn default_region(ppp: f32) -> selinga_capture::Region {
    let (pw, ph) = selinga_platform::primary_screen_px();
    let w = (DEFAULT_SIZE[0] * ppp).round().max(1.0) as u32;
    let h = (DEFAULT_SIZE[1] * ppp).round().max(1.0) as u32;
    selinga_capture::Region {
        x: ((pw as i32 - w as i32) / 2).max(0),
        y: ((ph as i32 - h as i32) / 2).max(0),
        w,
        h,
    }
}

fn display_mode_from(value: &str) -> DisplayMode {
    match value {
        "bilingual" => DisplayMode::Bilingual,
        _ => DisplayMode::Translation,
    }
}

fn log_event(message: &str) {
    use std::io::Write;
    let Some(dir) = Settings::path().and_then(|path| path.parent().map(|p| p.to_path_buf())) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("selinga.log");
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 1_000_000) {
        let _ = std::fs::rename(&path, dir.join("selinga.log.1"));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{} {message}", timestamp());
    }
}

fn timestamp() -> String {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::SystemInformation::GetLocalTime;
        let time = GetLocalTime();
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
        )
    }
    #[cfg(not(windows))]
    {
        String::from("----")
    }
}

fn settings_mtime() -> Option<std::time::SystemTime> {
    let path = Settings::path()?;
    std::fs::metadata(path).ok()?.modified().ok()
}

fn main() -> eframe::Result<()> {
    selinga_platform::enable_per_monitor_dpi();
    // 插件模式必须在单实例判定之前解析：第二份进程要靠它决定是「重新框选」还是「激活旧窗口」。
    let plugin = plugin::PluginMode::from_args(std::env::args().skip(1));
    if let Some(mode) = &plugin {
        if mode.action != plugin::ACTION_TRANSLATE {
            log_event(&format!(
                "starpie: unknown action {:?}, falling back to {}",
                mode.action,
                plugin::ACTION_TRANSLATE
            ));
        }
        log_event(&format!(
            "starpie plugin mode: v{} {}",
            env!("CARGO_PKG_VERSION"),
            mode.describe()
        ));
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log_event(&format!("panic: {info}"));
        default_hook(info);
    }));
    if !selinga_platform::single_instance() {
        log_event("another instance is running; signalled it and exited");
        return Ok(());
    }
    // 只有插件模式才建这个监听器：独立版重复启动仍走「激活旧窗口」的老行为，不改变用户预期。
    let reselect = if plugin.is_some() {
        selinga_platform::ReselectListener::create()
    } else {
        None
    };
    let (screen_w, screen_h) = selinga_platform::primary_screen_points();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Selinga")
        .with_taskbar(false)
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(true)
        .with_active(true)
        .with_visible(true)
        .with_inner_size([screen_w, screen_h])
        .with_position([-16000.0, -16000.0])
        .with_window_level(egui::WindowLevel::AlwaysOnTop);
    if let Some((rgba, width, height)) = app_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(egui::IconData {
            rgba,
            width,
            height,
        }));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Selinga",
        options,
        Box::new(move |cc| {
            selinga_platform::exclude_self_from_capture();
            install_cjk_fonts(&cc.egui_ctx);
            Ok(Box::new(SelingaApp::new(&cc.egui_ctx, plugin, reselect)))
        }),
    )
}

fn app_icon() -> Option<(Vec<u8>, u32, u32)> {
    let bytes = include_bytes!("../assets/selinga.png");
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes.as_slice()));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer).ok()?;
    buffer.truncate(info.buffer_size());
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    Some((buffer, info.width, info.height))
}

fn install_cjk_fonts(ctx: &egui::Context) {
    let candidates = [
        ("cjk-msyh", "C:\\Windows\\Fonts\\msyh.ttc"),
        ("cjk-msyh-ttf", "C:\\Windows\\Fonts\\msyh.ttf"),
        ("cjk-simhei", "C:\\Windows\\Fonts\\simhei.ttf"),
        ("cjk-simsun", "C:\\Windows\\Fonts\\simsun.ttc"),
    ];
    let mut fonts = egui::FontDefinitions::default();
    for (name, path) in candidates {
        match std::fs::read(path) {
            Ok(data) => {
                fonts.font_data.insert(
                    name.to_owned(),
                    std::sync::Arc::new(egui::FontData::from_owned(data)),
                );
                for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                    fonts
                        .families
                        .entry(family)
                        .or_default()
                        .push(name.to_owned());
                }
                break;
            }
            Err(_) => continue,
        }
    }
    ctx.set_fonts(fonts);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Select,
    Box,
}

fn box_viewport_id() -> egui::ViewportId {
    egui::ViewportId(egui::Id::new("selinga_box"))
}

struct SelingaApp {
    paused: bool,
    busy: bool,
    dragging: bool,
    resizing: Option<egui::ResizeDirection>,
    drag_anchor: Option<(egui::Pos2, egui::Rect)>,
    drag_rect: Option<egui::Rect>,
    last_rect: Option<egui::Rect>,
    stable_since: Option<Instant>,
    last_trigger: Option<Instant>,
    last_exclude: Instant,
    display_mode: DisplayMode,
    copied_at: Option<Instant>,
    result: Option<Result<PipelineOutput, String>>,
    translate_error: Option<String>,
    notice: Option<String>,
    pending_ai: bool,
    result_ai: bool,
    target_lang: usize,
    translator: Arc<std::sync::RwLock<Arc<dyn Translator>>>,
    vlm: settings::VlmSettings,
    hotkeys: settings::HotkeySettings,
    autostart: bool,
    hidden: bool,
    phase: Phase,
    /// 插件版：`Some` 表示由 StarPie 触发启动，托盘 / 全局热键 / 开机自启全部关闭。
    plugin: Option<plugin::PluginMode>,
    /// 已运行实例的「重新框选」信号。独立版为 `None`，行为与之前完全一致。
    reselect: Option<selinga_platform::ReselectListener>,
    /// 重新框选后「持续重施几何」的截止时刻。见 update() 里 Select 分支的说明。
    reselect_reassert: Option<Instant>,
    sel_phys: Option<selinga_capture::Region>,
    marquee: Option<(egui::Pos2, egui::Pos2)>,
    show_pending: bool,
    pending_frames: u32,
    show_after_frames: u32,
    box_show_pending: bool,
    box_pending_frames: u32,
    #[cfg(windows)]
    live_bg: Option<egui::TextureHandle>,
    #[cfg(windows)]
    select_bg: Option<egui::TextureHandle>,
    pipeline_gen: Arc<AtomicU64>,
    busy_since: Option<Instant>,
    settings_mtime: Option<std::time::SystemTime>,
    last_settings_check: Instant,
    #[cfg(windows)]
    live_fast_until: Instant,
    #[cfg(windows)]
    last_live_capture: Instant,
    t0: Instant,
    #[cfg(windows)]
    tray: Option<Tray>,
    #[cfg(windows)]
    cache: Arc<Mutex<Option<FrameCache>>>,
    #[cfg(windows)]
    worker: Option<Worker>,
}

#[cfg(windows)]
struct Worker {
    tx: std::sync::mpsc::Sender<WorkerMsg>,
    rx: std::sync::mpsc::Receiver<WorkerMsg>,
}

#[cfg(windows)]
enum WorkerMsg {
    Done {
        epoch: u64,
        ai: bool,
        output: PipelineOutput,
        translate_error: Option<String>,
    },
    Failed {
        epoch: u64,
        ai: bool,
        message: String,
    },
}

impl SelingaApp {
    fn new(
        ctx: &egui::Context,
        plugin: Option<plugin::PluginMode>,
        reselect: Option<selinga_platform::ReselectListener>,
    ) -> Self {
        #[cfg(windows)]
        let (tx, rx) = std::sync::mpsc::channel();
        let (saved, broken_settings) = Settings::load_or_recover();
        // 宿主显式给了语言就用宿主的；给了未知值则回落到设置，不静默改成默认语言。
        let target_lang = plugin
            .as_ref()
            .and_then(plugin::PluginMode::target_lang_index)
            .unwrap_or_else(|| {
                TARGET_LANGS
                    .iter()
                    .position(|l| *l == saved.target_lang)
                    .unwrap_or(0)
            });
        let display_mode = plugin
            .as_ref()
            .and_then(|mode| mode.display_mode)
            .unwrap_or_else(|| display_mode_from(&saved.display_mode));
        let settings_mtime = settings_mtime();
        // 插件版不建托盘，也就不注册那三个全局热键 —— 触发权归宿主，两套热键只会互相抢键。
        #[cfg(windows)]
        let (tray, tray_error, failed_hotkeys) = if plugin.is_some() {
            (None, None, Vec::new())
        } else {
            match Tray::new(&saved.hotkeys) {
                Ok(tray) => {
                    let failed = tray.failed.clone();
                    (Some(tray), None, failed)
                }
                Err(err) => (None, Some(err), Vec::new()),
            }
        };
        let app = Self {
            paused: false,
            busy: false,
            dragging: false,
            resizing: None,
            drag_anchor: None,
            drag_rect: None,
            last_rect: None,
            stable_since: None,
            last_trigger: None,
            last_exclude: Instant::now(),
            display_mode,
            copied_at: None,
            result: None,
            translate_error: None,
            notice: None,
            pending_ai: false,
            result_ai: false,
            target_lang,
            hidden: false,
            phase: Phase::Select,
            sel_phys: None,
            marquee: None,
            show_pending: true,
            pending_frames: 0,
            show_after_frames: 1,
            box_show_pending: false,
            box_pending_frames: 0,
            #[cfg(windows)]
            live_bg: None,
            #[cfg(windows)]
            select_bg: None,
            vlm: saved.vlm.clone(),
            hotkeys: saved.hotkeys.clone(),
            autostart: saved.autostart,
            translator: Arc::new(std::sync::RwLock::new(build_translator(
                TARGET_LANGS[target_lang],
            ))),
            #[cfg(windows)]
            tray,
            #[cfg(windows)]
            cache: Arc::new(Mutex::new(None)),
            #[cfg(windows)]
            worker: Some(Worker { tx, rx }),
            pipeline_gen: Arc::new(AtomicU64::new(0)),
            busy_since: None,
            settings_mtime,
            last_settings_check: Instant::now(),
            #[cfg(windows)]
            live_fast_until: Instant::now(),
            #[cfg(windows)]
            last_live_capture: Instant::now(),
            t0: Instant::now(),
            plugin,
            reselect,
            reselect_reassert: None,
        };
        #[cfg(windows)]
        let app = {
            let mut app = app;
            app.load_select_background(ctx);
            if let Some(err) = &tray_error {
                log_event(&format!("tray unavailable: {err}"));
                app.notice = Some(format!("托盘不可用：{err}"));
            }
            // 插件版不碰注册表自启项：那是独立版的意图，插件的启停由宿主管理。
            if app.plugin.is_none()
                && let Err(err) = selinga_platform::set_autostart(app.autostart)
            {
                log_event(&format!("autostart sync failed: {err}"));
            }
            if broken_settings.is_some() {
                log_event("settings unreadable; original moved to settings.toml.broken");
                app.notice = Some("配置无法解析，已备份为 settings.toml.broken".to_owned());
            }
            if !failed_hotkeys.is_empty() {
                let list = failed_hotkeys.join("、");
                log_event(&format!("hotkeys not registered: {list}"));
                app.notice = Some(format!("热键未注册：{list}"));
            }

            app
        };
        #[cfg(not(windows))]
        let _ = ctx;
        {
            log_event(&format!(
                "start: settings={}",
                Settings::path()
                    .map_or_else(|| "<none>".to_owned(), |path| path.display().to_string())
            ));
        }
        app
    }

    fn current_settings(&self) -> Settings {
        Settings {
            target_lang: TARGET_LANGS[self.target_lang].to_owned(),
            display_mode: match self.display_mode {
                DisplayMode::Translation => "translation".to_owned(),
                DisplayMode::Bilingual => "bilingual".to_owned(),
            },
            paused: false,
            vlm: self.vlm.clone(),
            hotkeys: self.hotkeys.clone(),
            autostart: self.autostart,
        }
    }

    fn persist_settings(&mut self) {
        self.current_settings().save();
        self.settings_mtime = settings_mtime();
    }

    fn generation(&self) -> u64 {
        self.pipeline_gen.load(Ordering::SeqCst)
    }

    fn bump_generation(&mut self) {
        self.pipeline_gen.fetch_add(1, Ordering::SeqCst);
        self.busy = false;
        self.busy_since = None;
        self.pending_ai = false;
        #[cfg(windows)]
        if let Ok(mut cache) = self.cache.lock() {
            *cache = None;
        }
    }

    fn rebuild_translator(&mut self) {
        let target = TARGET_LANGS[self.target_lang];
        if let Ok(mut guard) = self.translator.write() {
            *guard = build_translator(target);
        }
        self.bump_generation();
    }

    fn refresh_now(&mut self, ctx: &egui::Context) {
        if self.busy {
            return;
        }
        let Some(rect) = self.last_rect else {
            return;
        };
        let ppp = ctx.native_pixels_per_point().unwrap_or(1.0);
        self.trigger(ctx, rect, ppp, true);
    }

    fn open_settings_file(&mut self) {
        let Some(path) = Settings::path() else {
            return;
        };
        let has_vlm_section = std::fs::read_to_string(&path)
            .map(|text| text.contains("[vlm]"))
            .unwrap_or(false);
        if !has_vlm_section {
            self.persist_settings();
        }
        log_event(&format!("open config: {}", path.display()));
        if let Err(err) = selinga_platform::open_in_editor(&path) {
            log_event(&format!("open config failed: {err}"));
            self.notice = Some(format!("open config failed: {err}"));
        }
    }

    fn reload_settings_if_changed(&mut self, ctx: &egui::Context) {
        if self.last_settings_check.elapsed() < SETTINGS_CHECK_INTERVAL {
            return;
        }
        self.last_settings_check = Instant::now();
        let Some(modified) = settings_mtime() else {
            return;
        };
        if self.settings_mtime == Some(modified) {
            return;
        }
        self.settings_mtime = Some(modified);
        let Some(path) = Settings::path() else {
            return;
        };
        let Some(loaded) = Settings::load_from(&path) else {
            return;
        };
        if loaded == self.current_settings() {
            return;
        }
        self.apply_settings(loaded, ctx);
    }

    fn apply_settings(&mut self, loaded: Settings, ctx: &egui::Context) {
        let hotkeys_changed = loaded.hotkeys != self.hotkeys;
        let autostart_changed = loaded.autostart != self.autostart;
        if let Some(index) = TARGET_LANGS.iter().position(|l| *l == loaded.target_lang) {
            self.target_lang = index;
        }
        self.display_mode = display_mode_from(&loaded.display_mode);
        self.vlm = loaded.vlm;
        self.hotkeys = loaded.hotkeys;
        self.autostart = loaded.autostart;
        self.rebuild_translator();
        #[cfg(windows)]
        if autostart_changed && let Err(err) = selinga_platform::set_autostart(self.autostart) {
            log_event(&format!("autostart change failed: {err}"));
            self.notice = Some(format!("开机自启设置失败：{err}"));
        }
        #[cfg(windows)]
        if hotkeys_changed {
            self.tray = Tray::new(&self.hotkeys).ok();
        }
        #[cfg(windows)]
        if let Some(tray) = &self.tray {
            tray.set_hidden_label(self.hidden);
            tray.set_paused_label(self.paused);
        }
        log_event(&format!(
            "settings reloaded: lang={} mode={:?} paused={} hotkeys_changed={hotkeys_changed}",
            TARGET_LANGS[self.target_lang], self.display_mode, self.paused
        ));
        self.refresh_now(ctx);
    }

    #[cfg(windows)]
    fn note_activity(&mut self) {
        self.live_fast_until = Instant::now() + LIVE_ACTIVE_WINDOW;
    }

    fn enforce_timeout(&mut self) -> bool {
        let budget = if self.pending_ai {
            AI_TIMEOUT
        } else {
            PIPELINE_TIMEOUT
        };
        if self.busy && self.busy_since.is_some_and(|t| t.elapsed() >= budget) {
            let pending_ai = self.pending_ai;
            self.bump_generation();
            self.busy = false;
            self.busy_since = None;
            self.pending_ai = false;
            self.translate_error = None;
            self.result = Some(Err(format!(
                "{} timeout after {}s",
                if pending_ai { "AI" } else { "pipeline" },
                budget.as_secs()
            )));
            return true;
        }
        false
    }
}

impl eframe::App for SelingaApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if self.phase == Phase::Select {
            egui::Color32::TRANSPARENT.to_normalized_gamma_f32()
        } else {
            egui::Color32::from_rgba_unmultiplied(12, 12, 12, 180).to_normalized_gamma_f32()
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        if self.enforce_timeout() {
            ctx.request_repaint();
        }

        #[cfg(windows)]
        if self.last_exclude.elapsed() > EXCLUDE_INTERVAL {
            self.last_exclude = Instant::now();
            selinga_platform::exclude_self_from_capture();
        }

        // 宿主再次触发扇区：第二份进程只是给这个事件打了个信号，真正的动作在这里。
        #[cfg(windows)]
        if self
            .reselect
            .as_ref()
            .is_some_and(|listener| listener.signaled())
        {
            log_event(&format!(
                "starpie: reselect requested by host (phase={:?}, root={:?})",
                self.phase,
                selinga_platform::window_rect("Selinga")
            ));
            self.enter_select_mode(&ctx);
            // 框态时 ROOT 被收成 1×1 并挪到屏幕外（见 finish_select）。回到选区态时
            // 只做 SetWindowPos 不够：窗口未必重新参与合成，屏幕上就是「框没了、选区也不出来」。
            // 这里显式 ShowWindow + SetForegroundWindow，再持续几帧重施几何抵消旧尺寸。
            selinga_platform::show_window("Selinga");
            self.reselect_reassert = Some(Instant::now() + Duration::from_millis(600));
            log_event(&format!(
                "starpie: reselect applied (root={:?})",
                selinga_platform::window_rect("Selinga")
            ));
            ctx.request_repaint();
        }

        match self.phase {
            Phase::Select => {
                // 重新框选后的收尾：连续几帧把 ROOT 重新铺回虚拟桌面。
                // 单帧施一次在 DPI 切换与窗口刚被显示的情况下并不可靠。
                #[cfg(windows)]
                if let Some(deadline) = self.reselect_reassert {
                    if Instant::now() < deadline {
                        let (vx, vy, vw, vh) = selinga_platform::virtual_screen_px();
                        selinga_platform::set_window_geometry_px(
                            "Selinga",
                            vx,
                            vy,
                            vw as i32,
                            vh as i32,
                        );
                        ctx.request_repaint();
                    } else {
                        self.reselect_reassert = None;
                        log_event(&format!(
                            "starpie: reselect geometry settled (root={:?})",
                            selinga_platform::window_rect("Selinga")
                        ));
                    }
                }
                #[cfg(windows)]
                if self.select_bg.is_none() {
                    self.load_select_background(&ctx);
                }
                self.draw_select_overlay(ui, &ctx);
                #[cfg(windows)]
                self.drain_tray_select_phase();
                self.reload_settings_if_changed(&ctx);
                self.handle_pending_show(&ctx);
                if self.show_pending {
                    ctx.request_repaint();
                } else {
                    ctx.request_repaint_after(Duration::from_millis(50));
                }
            }
            Phase::Box => {
                #[cfg(windows)]
                if self.box_show_pending {
                    self.draw_frozen_background(ui);
                }
                self.draw_box_viewport(&ctx);
                if self.box_show_pending {
                    ctx.request_repaint();
                } else {
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
            }
        }
    }
}

impl SelingaApp {
    fn draw_box_viewport(&mut self, root_ctx: &egui::Context) {
        let Some(sel) = self.sel_phys else {
            return;
        };
        let scale =
            selinga_platform::monitor_scale_at(sel.x + sel.w as i32 / 2, sel.y + sel.h as i32 / 2);
        let (logical_pos, logical_size) = region_to_logical(sel, scale);
        let builder = egui::ViewportBuilder::default()
            .with_title("Selinga Box")
            .with_taskbar(false)
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(true)
            .with_active(true)
            .with_visible(true)
            .with_position([-16000.0, -16000.0])
            .with_inner_size(logical_size)
            .with_window_level(egui::WindowLevel::AlwaysOnTop);
        let root_ctx = root_ctx.clone();
        root_ctx.show_viewport_immediate(box_viewport_id(), builder, |ui, _class| {
            let box_ctx = ui.ctx().clone();

            if box_ctx.input(|i| i.viewport().close_requested()) {
                eprintln!("selinga: box close requested -> quit");
                root_ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
                return;
            }

            if self.box_show_pending {
                self.box_pending_frames += 1;
                let f = self.box_pending_frames;
                if f == 1 {
                    eprintln!("selinga: box viewport first frame");
                    selinga_platform::exclude_self_from_capture();
                    self.last_exclude = Instant::now();
                }
                if f == BOX_SHOW_AT_FRAME {
                    eprintln!(
                        "selinga: t={}ms box revealed",
                        self.t0.elapsed().as_millis()
                    );
                    let placed = selinga_platform::set_window_geometry_px(
                        "Selinga Box",
                        sel.x,
                        sel.y,
                        sel.w as i32,
                        sel.h as i32,
                    );
                    if !placed {
                        box_ctx
                            .send_viewport_cmd(egui::ViewportCommand::OuterPosition(logical_pos));
                    }
                    box_ctx.request_repaint();
                }
                if f >= BOX_HANDOFF_AT_FRAME {
                    eprintln!(
                        "selinga: t={}ms box handoff complete",
                        self.t0.elapsed().as_millis()
                    );
                    self.box_show_pending = false;
                    #[cfg(windows)]
                    {
                        self.select_bg = None;
                    }
                }
            }

            let outer = box_ctx.input(|i| i.viewport().outer_rect);
            let ppp = box_ctx.native_pixels_per_point().unwrap_or(1.0);

            #[cfg(windows)]
            if !self.dragging && self.resizing.is_none() {
                self.update_live_background(&box_ctx, ppp, outer);
            }

            if outer != self.last_rect {
                self.last_rect = outer;
                self.stable_since = Some(Instant::now());
                #[cfg(windows)]
                self.note_activity();
            }

            self.drain_worker(&box_ctx);
            self.drain_tray(&box_ctx);
            self.reload_settings_if_changed(&box_ctx);
            self.draw_box(ui, &box_ctx);

            let decision = trigger_decision(TriggerInput {
                pointer_idle: !box_ctx.input(|i| i.pointer.any_down()),
                paused: self.paused,
                hidden: self.hidden,
                busy: self.busy,
                dragging: self.dragging,
                resizing: self.resizing.is_some(),
                stable_for: self.stable_since.map(|t| t.elapsed()),
                since_trigger: self.last_trigger.map(|t| t.elapsed()),
            });
            if decision.is_some()
                && let Some(rect) = outer
            {
                self.trigger(&box_ctx, rect, ppp, false);
            }
        });
    }

    fn handle_pending_show(&mut self, ctx: &egui::Context) {
        if !self.show_pending {
            return;
        }
        self.pending_frames += 1;
        if self.pending_frames >= self.show_after_frames {
            self.show_pending = false;
            eprintln!(
                "selinga: t={}ms root revealed after {} frames",
                self.t0.elapsed().as_millis(),
                self.pending_frames
            );
            let (vx, vy, vw, vh) = selinga_platform::virtual_screen_px();
            if !selinga_platform::set_window_geometry_px("Selinga", vx, vy, vw as i32, vh as i32) {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(0.0, 0.0)));
            }
            ctx.request_repaint();
        }
    }

    #[cfg(windows)]
    fn capture_screen_texture(&self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        let (x, y, w, h) = selinga_platform::virtual_screen_px();
        let capture = selinga_capture::win32::GdiCapture::new().ok()?;
        let frame = capture
            .capture_region(selinga_capture::Region { x, y, w, h })
            .ok()?;
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.to_rgba(),
        );
        Some(ctx.load_texture("screen_bg", image, egui::TextureOptions::LINEAR))
    }

    #[cfg(windows)]
    fn load_select_background(&mut self, ctx: &egui::Context) {
        match self.capture_screen_texture(ctx) {
            Some(tex) => {
                let s = tex.size_vec2();
                eprintln!("selinga: bg loaded {}x{}", s.x as u32, s.y as u32);
                self.select_bg = Some(tex);
            }
            None => eprintln!("selinga: bg capture failed"),
        }
    }

    fn draw_frozen_background(&self, ui: &egui::Ui) {
        let rect = ui.max_rect();
        let painter = ui.painter();
        #[cfg(windows)]
        {
            if let Some(tex) = &self.select_bg {
                painter.image(
                    tex.id(),
                    rect,
                    egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else {
                painter.rect_filled(rect, 0.0, egui::Color32::from_black_alpha(70));
            }
        }
        #[cfg(not(windows))]
        painter.rect_filled(rect, 0.0, egui::Color32::from_black_alpha(70));
    }

    fn draw_select_overlay(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let rect = ui.max_rect();
        let painter = ui.painter().clone();

        self.draw_frozen_background(ui);

        let resp = ui.allocate_rect(rect, egui::Sense::click_and_drag());
        let pointer = ctx.input(|i| i.pointer.clone());

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) || resp.secondary_clicked() {
            log_event(&format!(
                "select cancelled (escape={} secondary={})",
                ctx.input(|i| i.key_pressed(egui::Key::Escape)),
                resp.secondary_clicked()
            ));
            self.finish_select(ctx, None);
            return;
        }

        if pointer.any_down() {
            ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
        }

        if pointer.primary_pressed() {
            self.marquee = pointer.interact_pos().map(|p| (p, p));
        } else if pointer.primary_down()
            && let (Some((_, cur)), Some(p)) = (&mut self.marquee, pointer.interact_pos())
        {
            *cur = p;
        }

        if let Some((a, b)) = self.marquee {
            let sel = egui::Rect::from_two_pos(a, b);
            painter.rect_filled(
                sel,
                0.0,
                egui::Color32::from_rgba_unmultiplied(0x53, 0x4A, 0xB7, 45),
            );
            painter.rect_stroke(
                sel,
                0.0,
                egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(0x8A, 0x81, 0xE8)),
                egui::StrokeKind::Inside,
            );
            let ppp = ctx.native_pixels_per_point().unwrap_or(1.0);
            let label = format!("{:.0} x {:.0}", sel.width() * ppp, sel.height() * ppp);
            let label_pos = egui::pos2(
                sel.left().clamp(rect.left() + 40.0, rect.right() - 40.0),
                (sel.top() - 12.0).max(rect.top() + 10.0),
            );
            painter.text(
                label_pos,
                egui::Align2::CENTER_BOTTOM,
                label,
                egui::FontId::proportional(13.0),
                TEXT_COLOR,
            );
        }

        if pointer.primary_released()
            && let Some((a, b)) = self.marquee.take()
        {
            log_event("select finished by drag");
            self.finish_select(ctx, Some(egui::Rect::from_two_pos(a, b)));
        }
    }

    fn finish_select(&mut self, ctx: &egui::Context, sel: Option<egui::Rect>) {
        self.marquee = None;
        let valid = sel.is_some_and(|r| r.width() >= MIN_SELECT && r.height() >= MIN_SELECT);
        let ppp = ctx.native_pixels_per_point().unwrap_or(1.0);
        let (vx, vy, _, _) = selinga_platform::virtual_screen_px();
        self.sel_phys = Some(if valid {
            selection_region(sel.unwrap_or(egui::Rect::ZERO), (vx, vy), ppp)
        } else {
            default_region(ppp)
        });
        self.phase = Phase::Box;
        log_event(&format!(
            "finish_select valid={valid} region={:?} t={}ms",
            self.sel_phys,
            self.t0.elapsed().as_millis()
        ));
        self.box_show_pending = true;
        self.box_pending_frames = 0;
        self.last_rect = None;
        self.stable_since = None;
        self.last_trigger = None;
        self.result = None;
        self.translate_error = None;
        self.notice = None;
        self.result_ai = false;
        self.bump_generation();
        ctx.send_viewport_cmd_to(
            egui::ViewportId::ROOT,
            egui::ViewportCommand::OuterPosition(egui::pos2(-16000.0, -16000.0)),
        );
        ctx.send_viewport_cmd_to(
            egui::ViewportId::ROOT,
            egui::ViewportCommand::InnerSize(egui::vec2(1.0, 1.0)),
        );
        #[cfg(windows)]
        {
            self.last_exclude = Instant::now();
            selinga_platform::exclude_self_from_capture();
        }
        eprintln!(
            "selinga: t={}ms root parked offscreen",
            self.t0.elapsed().as_millis()
        );
        ctx.request_repaint();
    }

    #[cfg(windows)]
    fn update_live_background(&mut self, ctx: &egui::Context, ppp: f32, outer: Option<egui::Rect>) {
        let Some(rect) = outer else {
            return;
        };
        let interval = if Instant::now() < self.live_fast_until {
            LIVE_INTERVAL_ACTIVE
        } else {
            LIVE_INTERVAL_IDLE
        };
        if self.last_live_capture.elapsed() < interval {
            return;
        }
        self.last_live_capture = Instant::now();
        let region = selinga_capture::Region {
            x: (rect.min.x * ppp).round() as i32,
            y: (rect.min.y * ppp).round() as i32,
            w: (rect.width() * ppp).round() as u32,
            h: (rect.height() * ppp).round() as u32,
        };
        let capture = match selinga_capture::win32::GdiCapture::new() {
            Ok(c) => c,
            Err(_) => return,
        };
        let Ok(frame) = capture.capture_region(region) else {
            return;
        };
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.to_rgba(),
        );
        if self.live_bg.is_none() {
            self.live_bg =
                Some(ctx.load_texture("live_bg", image.clone(), egui::TextureOptions::LINEAR));
            eprintln!("selinga: live bg {}x{}", frame.width, frame.height);
        }
        if let Some(tex) = self.live_bg.as_mut() {
            tex.set(image, egui::TextureOptions::LINEAR);
        }
    }

    fn draw_box(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let max_rect = ui.max_rect();
        let rect = match self.drag_rect {
            Some(r) => egui::Rect::from_min_size(egui::Pos2::ZERO, r.size()),
            None => max_rect,
        };
        let ppp = ctx.native_pixels_per_point().unwrap_or(1.0);
        let painter = ui.ctx().layer_painter(ui.layer_id());
        #[cfg(windows)]
        {
            if let Some(tex) = &self.select_bg {
                let ts = tex.size_vec2();
                let uv = match self.drag_rect {
                    Some(r) => egui::Rect::from_min_max(
                        egui::pos2(r.min.x * ppp / ts.x, r.min.y * ppp / ts.y),
                        egui::pos2(r.max.x * ppp / ts.x, r.max.y * ppp / ts.y),
                    ),
                    None => egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                };
                painter.image(tex.id(), rect, uv, egui::Color32::WHITE);
            } else if let Some(tex) = &self.live_bg {
                painter.image(
                    tex.id(),
                    rect,
                    egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
        }
        self.draw_overlay_results(ui, ppp);

        painter.rect_stroke(
            rect,
            6.0,
            egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(0x53, 0x4A, 0xB7)),
            egui::StrokeKind::Inside,
        );

        let resp = ui.allocate_rect(max_rect, egui::Sense::click_and_drag());
        let pointer = ctx.input(|i| i.pointer.clone());
        let pos = pointer.latest_pos();
        #[cfg(windows)]
        if resp.hovered() || pointer.any_down() {
            self.note_activity();
        }
        let resize_dir = resize_direction(rect, pos);
        let mut toolbar_hovered = false;
        if ui.rect_contains_pointer(rect) {
            toolbar_hovered = self.draw_toolbar(ui, rect);
        }
        self.draw_status_badge(ui, rect);
        if self.result_ai {
            ui.painter().text(
                egui::pos2(rect.right() - 8.0, rect.bottom() - 6.0),
                egui::Align2::RIGHT_BOTTOM,
                "AI",
                egui::FontId::proportional(10.0),
                SOURCE_COLOR,
            );
        }

        if !pointer.any_down() {
            if let Some(dir) = resize_dir {
                if !toolbar_hovered {
                    ctx.set_cursor_icon(resize_cursor(dir));
                }
            } else if resp.hovered() {
                ctx.set_cursor_icon(if toolbar_hovered {
                    egui::CursorIcon::PointingHand
                } else {
                    egui::CursorIcon::Grab
                });
            }
        }

        let outer = ctx.input(|i| i.viewport().outer_rect);
        let pointer_screen = pos.and_then(|p| outer.map(|o| o.min + p.to_vec2()));

        if pointer.primary_pressed() && !toolbar_hovered {
            if resize_dir.is_some() {
                self.resizing = resize_dir;
            } else if resize_dir.is_none() && resp.hovered() {
                self.dragging = true;
            }
            if self.dragging || self.resizing.is_some() {
                #[cfg(windows)]
                if self.select_bg.is_none() {
                    self.select_bg = self.capture_screen_texture(ctx);
                }
                if let (Some(ps), Some(o)) = (pointer_screen, outer) {
                    self.drag_anchor = Some((ps, o));
                }
            }
        }

        if (self.dragging || self.resizing.is_some())
            && pointer.any_down()
            && let (Some((grab, start)), Some(ps)) = (self.drag_anchor, pointer_screen)
        {
            let delta = ps - grab;
            let next = match self.resizing {
                Some(dir) => resize_rect(start, delta, dir),
                None => start.translate(delta),
            };
            let (screen_w, screen_h) = selinga_platform::primary_screen_points();
            let next = clamp_rect(next, screen_w, screen_h);
            let applied = selinga_platform::set_window_geometry_px(
                "Selinga Box",
                (next.min.x * ppp).round() as i32,
                (next.min.y * ppp).round() as i32,
                (next.width() * ppp).round() as i32,
                (next.height() * ppp).round() as i32,
            );
            if !applied {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(next.min));
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(next.size()));
            }
            self.drag_rect = Some(next);
            ctx.request_repaint();
        }

        if pointer.primary_released() {
            self.dragging = false;
            self.resizing = None;
            self.drag_anchor = None;
            self.drag_rect = None;
            #[cfg(windows)]
            {
                self.select_bg = None;
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn draw_toolbar(&mut self, ui: &mut egui::Ui, box_rect: egui::Rect) -> bool {
        use ToolbarIcon::{Ai, Copy, Lang, Mode, Pause, Refresh, Resume};

        let lang_code = TARGET_LANGS[self.target_lang]
            .split('-')
            .next()
            .unwrap_or("EN")
            .to_uppercase();
        let mode_tip = match self.display_mode {
            DisplayMode::Translation => "switch to bilingual",
            DisplayMode::Bilingual => "switch to translation only",
        };
        let ai_ready = self.vlm.config().is_some();
        let buttons: Vec<(ToolbarIcon, &str, ToolbarAction)> = vec![
            (
                Mode,
                mode_tip,
                Box::new(|app: &mut Self, ui: &egui::Ui| {
                    app.display_mode = app.display_mode.toggle();
                    app.persist_settings();
                    ui.ctx().request_repaint();
                }),
            ),
            (
                Copy,
                "copy result",
                Box::new(|app: &mut Self, ui: &egui::Ui| {
                    if let Some(Ok(output)) = &app.result {
                        let text = match app.display_mode {
                            DisplayMode::Translation => output.translated_text(),
                            DisplayMode::Bilingual => output.bilingual_text(),
                        };
                        ui.ctx().copy_text(text);
                        app.copied_at = Some(Instant::now());
                    }
                }),
            ),
            (
                Lang,
                "target language",
                Box::new(|app: &mut Self, ui: &egui::Ui| {
                    app.target_lang = (app.target_lang + 1) % TARGET_LANGS.len();
                    app.rebuild_translator();
                    app.persist_settings();
                    app.refresh_now(ui.ctx());
                }),
            ),
            (
                Refresh,
                "re-recognize",
                Box::new(|app: &mut Self, ui: &egui::Ui| {
                    app.refresh_now(ui.ctx());
                }),
            ),
            (
                Ai,
                if ai_ready {
                    "AI image recognition"
                } else {
                    "AI 未配置"
                },
                Box::new(|app: &mut Self, ui: &egui::Ui| {
                    app.trigger_ai(ui.ctx());
                }),
            ),
            (
                if self.paused { Resume } else { Pause },
                if self.paused { "resume" } else { "pause" },
                Box::new(|app: &mut Self, _ui: &egui::Ui| {
                    app.paused = !app.paused;
                    app.persist_settings();
                }),
            ),
        ];

        const ICON: f32 = 22.0;
        const GAP: f32 = 2.0;
        let n = buttons.len() as f32;
        let total_h = n * ICON + (n - 1.0) * GAP;
        let y0 = if total_h + 8.0 >= box_rect.height() {
            box_rect.top() + 4.0
        } else {
            box_rect.center().y - total_h * 0.5
        };
        let x_right = box_rect.right() - 4.0;
        let column_rect = egui::Rect::from_min_size(
            egui::pos2(x_right - ICON - 5.0, y0 - 5.0),
            egui::vec2(ICON + 10.0, total_h + 10.0),
        );
        let painter = ui.painter().clone();
        painter.rect_filled(column_rect, 8.0, egui::Color32::from_black_alpha(210));

        let pointer_pos = ui.input(|i| i.pointer.latest_pos());
        let mut any_hovered = false;
        for (idx, (icon, tip, action)) in buttons.iter().enumerate() {
            let btn_rect = egui::Rect::from_min_size(
                egui::pos2(x_right - ICON, y0 + idx as f32 * (ICON + GAP)),
                egui::vec2(ICON, ICON),
            );
            let hovered = pointer_pos.is_some_and(|p| btn_rect.contains(p));
            any_hovered |= hovered;
            let color = if hovered {
                TEXT_COLOR
            } else {
                TEXT_COLOR.gamma_multiply(0.72)
            };
            if hovered {
                painter.rect_filled(btn_rect, 6.0, egui::Color32::from_white_alpha(26));
            }
            let resp = ui.allocate_rect(btn_rect, egui::Sense::click());
            if resp.clicked() {
                action(self, ui);
            }
            if resp.hovered() {
                resp.on_hover_text(*tip);
            }
            let stroke = egui::Stroke::new(1.6_f32, color);
            match icon {
                ToolbarIcon::Mode => {
                    let (l, r, cy) = (
                        btn_rect.left() + 3.0,
                        btn_rect.right() - 3.0,
                        btn_rect.center().y,
                    );
                    painter.arrow(egui::pos2(l, cy - 3.5), egui::vec2(r - l, 0.0), stroke);
                    painter.arrow(egui::pos2(r, cy + 3.5), egui::vec2(l - r, 0.0), stroke);
                }
                ToolbarIcon::Copy => {
                    let back = egui::Rect::from_min_size(
                        egui::pos2(btn_rect.left() + 4.0, btn_rect.top() + 3.0),
                        egui::vec2(9.0, 9.0),
                    );
                    let front = egui::Rect::from_min_size(
                        egui::pos2(btn_rect.left() + 8.0, btn_rect.top() + 9.0),
                        egui::vec2(9.0, 9.0),
                    );
                    painter.rect_stroke(back, 2.0, stroke, egui::StrokeKind::Inside);
                    painter.rect_filled(front, 2.0, egui::Color32::from_rgb(0x22, 0x22, 0x2A));
                    painter.rect_stroke(front, 2.0, stroke, egui::StrokeKind::Inside);
                }
                ToolbarIcon::Lang => {
                    painter.text(
                        btn_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        lang_code.as_str(),
                        egui::FontId::proportional(9.5),
                        color,
                    );
                }
                ToolbarIcon::Refresh => {
                    let c = btn_rect.center();
                    painter.circle_stroke(c, 5.5, stroke);
                    let a = 45_f32.to_radians();
                    let (dx, dy) = (a.cos(), a.sin());
                    let edge = egui::pos2(c.x + 5.5 * dx, c.y + 5.5 * dy);
                    let tip = egui::pos2(edge.x - 3.0 * dy, edge.y + 3.0 * dx);
                    let base = egui::pos2(edge.x + 3.0 * dy, edge.y - 3.0 * dx);
                    let inner = egui::pos2(c.x + 1.5 * dx, c.y + 1.5 * dy);
                    painter.add(egui::Shape::convex_polygon(
                        vec![tip, base, inner],
                        color,
                        egui::Stroke::NONE,
                    ));
                }
                ToolbarIcon::Ai => {
                    let color = if ai_ready {
                        color
                    } else {
                        color.gamma_multiply(0.45)
                    };
                    painter.text(
                        btn_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "AI",
                        egui::FontId::proportional(9.5),
                        color,
                    );
                }
                ToolbarIcon::Pause => {
                    let c = btn_rect.center();
                    for x_off in [-4.5_f32, 1.5] {
                        painter.rect_filled(
                            egui::Rect::from_min_size(
                                egui::pos2(c.x + x_off, c.y - 4.5),
                                egui::vec2(3.0, 9.0),
                            ),
                            1.0,
                            color,
                        );
                    }
                }
                ToolbarIcon::Resume => {
                    let c = btn_rect.center();
                    painter.add(egui::Shape::convex_polygon(
                        vec![
                            egui::pos2(c.x - 3.5, c.y - 5.0),
                            egui::pos2(c.x - 3.5, c.y + 5.0),
                            egui::pos2(c.x + 5.0, c.y),
                        ],
                        color,
                        egui::Stroke::NONE,
                    ));
                }
            }
        }
        if self
            .copied_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
        {
            painter.text(
                egui::pos2(column_rect.left() - 6.0, column_rect.center().y),
                egui::Align2::RIGHT_CENTER,
                "copied!",
                egui::FontId::proportional(12.0),
                TEXT_COLOR,
            );
        }
        any_hovered
    }

    fn draw_status_badge(&mut self, ui: &mut egui::Ui, box_rect: egui::Rect) {
        const WARN_COLOR: egui::Color32 = egui::Color32::from_rgb(0xE2, 0xA1, 0x4B);
        let (text, color): (String, egui::Color32) = if self.busy {
            let label = if self.pending_ai {
                "AI image recognition..."
            } else {
                "recognizing..."
            };
            (label.to_owned(), SOURCE_COLOR)
        } else if self.paused {
            ("paused".to_owned(), SOURCE_COLOR)
        } else if let Some(notice) = &self.notice {
            let short: String = notice.chars().take(72).collect();
            (short, WARN_COLOR)
        } else if let Some(err) = &self.translate_error {
            let short: String = format!("translate: {}", err.chars().take(56).collect::<String>());
            (short, WARN_COLOR)
        } else if let Some(Err(err)) = &self.result {
            let short: String = err.chars().take(64).collect();
            (short, egui::Color32::from_rgb(0xE2, 0x4B, 0x4A))
        } else {
            return;
        };
        ui.painter().text(
            egui::pos2(box_rect.left() + 8.0, box_rect.top() + 8.0),
            egui::Align2::LEFT_TOP,
            text,
            egui::FontId::proportional(12.0),
            color,
        );
    }

    fn draw_overlay_results(&mut self, ui: &mut egui::Ui, ppp: f32) {
        let Some(Ok(output)) = &self.result else {
            return;
        };
        if output.lines.is_empty() {
            return;
        }
        let mode = self.display_mode;
        let painter = ui.painter();
        let ctx = ui.ctx().clone();
        for pair in &output.lines {
            let Some(bbox) = pair.rect else {
                continue;
            };
            if pair.translated.trim().is_empty() {
                continue;
            }
            let line = egui::Rect::from_min_size(
                egui::pos2(bbox.x / ppp, bbox.y / ppp),
                egui::vec2(bbox.w / ppp, bbox.h / ppp),
            );
            let mask = line.expand(LINE_PAD);
            let avail_w = mask.width() - 2.0 * LINE_PAD;
            let font_size = match mode {
                DisplayMode::Translation => {
                    fit_font_size(&ctx, &pair.translated, avail_w, line.height())
                }
                DisplayMode::Bilingual => {
                    fit_font_size(&ctx, &pair.translated, avail_w, line.height() * 0.5)
                }
            };
            painter.rect_filled(mask, 3.0, mask_color());
            let max_chars = (mask.width() / (font_size * 0.6)).max(4.0) as usize;
            match mode {
                DisplayMode::Translation => {
                    let translated = ellipsize(&pair.translated, max_chars);
                    painter.text(
                        mask.min + egui::vec2(LINE_PAD, LINE_PAD),
                        egui::Align2::LEFT_TOP,
                        &translated,
                        egui::FontId::proportional(font_size),
                        TEXT_COLOR,
                    );
                }
                DisplayMode::Bilingual => {
                    let source = ellipsize(&pair.source, max_chars);
                    painter.text(
                        mask.min + egui::vec2(LINE_PAD, LINE_PAD),
                        egui::Align2::LEFT_TOP,
                        &source,
                        egui::FontId::proportional(font_size * 0.62),
                        SOURCE_COLOR,
                    );
                    let translated = ellipsize(&pair.translated, max_chars);
                    painter.text(
                        mask.min + egui::vec2(LINE_PAD, LINE_PAD + mask.height() * 0.45),
                        egui::Align2::LEFT_TOP,
                        &translated,
                        egui::FontId::proportional(font_size),
                        TEXT_COLOR,
                    );
                }
            }
        }
    }

    #[cfg(windows)]
    fn trigger(&mut self, _ctx: &egui::Context, rect: egui::Rect, ppp: f32, force: bool) {
        self.spawn_worker(rect, ppp, force, false);
    }

    #[cfg(windows)]
    fn trigger_ai(&mut self, ctx: &egui::Context) {
        if self.busy {
            return;
        }
        if self.paused || self.hidden {
            let reason = if self.paused {
                "已暂停"
            } else {
                "方框已隐藏"
            };
            self.notice = Some(format!("{reason}：先恢复再试"));
            return;
        }
        if self.vlm.config().is_none() {
            self.notice = Some("AI 未配置：托盘 → 配置文件".to_owned());
            return;
        }
        let Some(rect) = self.last_rect else {
            return;
        };
        let ppp = ctx.native_pixels_per_point().unwrap_or(1.0);
        self.pending_ai = true;
        self.spawn_worker(rect, ppp, true, true);
    }

    #[cfg(windows)]
    fn spawn_worker(&mut self, rect: egui::Rect, ppp: f32, force: bool, ai: bool) {
        self.notice = None;
        self.busy = true;
        self.busy_since = Some(Instant::now());
        self.last_trigger = Some(Instant::now());
        self.stable_since = None;
        if force {
            self.note_activity();
        }
        let generation = self.generation();
        let gen_shared = Arc::clone(&self.pipeline_gen);

        let Some(worker) = &self.worker else {
            self.busy = false;
            self.busy_since = None;
            self.pending_ai = false;
            return;
        };
        let tx = worker.tx.clone();
        let cache = Arc::clone(&self.cache);
        let translator = Arc::clone(&self.translator);
        let ai_config = if ai { self.vlm.config() } else { None };
        let target_lang = TARGET_LANGS[self.target_lang].to_owned();
        let region = selinga_capture::Region {
            x: (rect.min.x * ppp).round() as i32,
            y: (rect.min.y * ppp).round() as i32,
            w: (rect.width() * ppp).round() as u32,
            h: (rect.height() * ppp).round() as u32,
        };

        std::thread::spawn(move || {
            let fail = |message: String| {
                log_event(&format!("pipeline failed (ai={ai}): {message}"));
                WorkerMsg::Failed {
                    epoch: generation,
                    ai,
                    message,
                }
            };
            let capture = match selinga_capture::win32::GdiCapture::new() {
                Ok(c) => c,
                Err(e) => return send(&tx, fail(e.to_string())),
            };
            let frame = match capture.capture_region(region) {
                Ok(f) => f,
                Err(e) => return send(&tx, fail(format!("capture: {e}"))),
            };
            let hash = frame_hash(&frame);

            if !ai
                && !force
                && let Some(cached) = cache.lock().ok().and_then(|c| c.clone())
                && cached.hash == hash
            {
                return send(
                    &tx,
                    WorkerMsg::Done {
                        epoch: generation,
                        ai: cached.ai,
                        output: cached.output,
                        translate_error: None,
                    },
                );
            }

            let (lines, translated, translate_error) = if ai {
                let Some(config) = ai_config else {
                    return send(&tx, fail("AI 未配置".to_owned()));
                };
                match ai_pass(&frame, &config, &target_lang) {
                    Ok((lines, translated)) => {
                        if lines.is_empty() {
                            return send(&tx, fail("AI 未识别到文字".to_owned()));
                        }
                        (lines, translated, None)
                    }
                    Err(e) => return send(&tx, fail(e)),
                }
            } else {
                let lines = match recognize_local(&frame) {
                    Ok(lines) => lines,
                    Err(e) => return send(&tx, fail(format!("ocr: {e}"))),
                };
                let sources: Vec<String> = lines.iter().map(|l| l.text.clone()).collect();
                let chain = match translator.read() {
                    Ok(g) => g.clone(),
                    Err(_) => return send(&tx, fail("translator lock poisoned".into())),
                };
                let (translated, translate_error) = match chain.translate_lines(&sources) {
                    Ok(t) => (t, None),
                    Err(e) => (sources.clone(), Some(e.to_string())),
                };
                (lines, translated, translate_error)
            };
            let output = selinga_pipeline::PipelineOutput {
                lines: lines
                    .iter()
                    .zip(translated)
                    .map(|(l, t)| selinga_pipeline::LinePair {
                        source: l.text.clone(),
                        translated: selinga_pipeline::normalize_translation(&t),
                        rect: l.rect,
                    })
                    .collect(),
            };
            if gen_shared.load(Ordering::SeqCst) == generation
                && let Ok(mut c) = cache.lock()
            {
                *c = Some(FrameCache {
                    hash,
                    output: output.clone(),
                    ai,
                });
            }
            send(
                &tx,
                WorkerMsg::Done {
                    epoch: generation,
                    ai,
                    output,
                    translate_error,
                },
            );
        });
    }

    #[cfg(not(windows))]
    fn trigger_ai(&mut self, _ctx: &egui::Context) {
        self.notice = Some("AI 识图 + 翻译仅在 Windows 可用".to_owned());
    }

    #[cfg(not(windows))]
    fn trigger(&mut self, _ctx: &egui::Context, _rect: egui::Rect, _ppp: f32, _force: bool) {
        self.busy = true;
        self.busy_since = Some(Instant::now());
        self.stable_since = None;
        self.last_trigger = Some(Instant::now());
        self.result = Some(Err(
            "pipeline not implemented on this platform yet".to_owned()
        ));
        self.busy = false;
        self.busy_since = None;
    }

    #[cfg(windows)]
    fn drain_tray(&mut self, ctx: &egui::Context) {
        let commands = match &self.tray {
            Some(tray) => tray.drain(),
            None => return,
        };
        for cmd in commands {
            match cmd {
                TrayCommand::ToggleHidden => {
                    self.hidden = !self.hidden;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(!self.hidden));
                    if let Some(tray) = &self.tray {
                        tray.set_hidden_label(self.hidden);
                    }
                }
                TrayCommand::TogglePause => {
                    self.paused = !self.paused;
                    self.persist_settings();
                    if let Some(tray) = &self.tray {
                        tray.set_paused_label(self.paused);
                    }
                }
                TrayCommand::Refresh => {
                    if !self.busy
                        && let Some(rect) = self.last_rect
                    {
                        let ppp = ctx.native_pixels_per_point().unwrap_or(1.0);
                        self.trigger(ctx, rect, ppp, true);
                    }
                }
                TrayCommand::Reselect => self.enter_select_mode(ctx),
                TrayCommand::OpenConfig => {
                    log_event("tray: OpenConfig");
                    self.open_settings_file();
                }
                TrayCommand::Quit => std::process::exit(0),
            }
        }
    }

    #[cfg(windows)]
    fn enter_select_mode(&mut self, ctx: &egui::Context) {
        self.phase = Phase::Select;
        self.sel_phys = None;
        self.marquee = None;
        self.hidden = false;
        self.last_rect = None;
        self.stable_since = None;
        self.last_trigger = None;
        self.result = None;
        self.translate_error = None;
        self.notice = None;
        self.result_ai = false;
        self.bump_generation();
        #[cfg(windows)]
        {
            self.select_bg = None;
            self.live_bg = None;
            if let Some(tray) = &self.tray {
                tray.set_hidden_label(false);
            }
        }
        ctx.send_viewport_cmd_to(box_viewport_id(), egui::ViewportCommand::Visible(false));
        let (vx, vy, vw, vh) = selinga_platform::virtual_screen_px();
        if !selinga_platform::set_window_geometry_px("Selinga", vx, vy, vw as i32, vh as i32) {
            let (sw, sh) = selinga_platform::primary_screen_points();
            ctx.send_viewport_cmd_to(
                egui::ViewportId::ROOT,
                egui::ViewportCommand::InnerSize(egui::vec2(sw, sh)),
            );
        }
        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
        #[cfg(windows)]
        {
            self.last_exclude = Instant::now();
            selinga_platform::exclude_self_from_capture();
        }
        self.show_pending = true;
        self.pending_frames = 0;
        self.show_after_frames = 2;
        eprintln!("selinga: reselect -> root fullscreen pending show");
    }

    #[cfg(windows)]
    fn drain_tray_select_phase(&mut self) {
        let commands = match &self.tray {
            Some(tray) => tray.drain(),
            None => return,
        };
        for cmd in commands {
            match cmd {
                TrayCommand::OpenConfig => {
                    log_event("tray: OpenConfig");
                    self.open_settings_file();
                }
                TrayCommand::Quit => std::process::exit(0),
                _ => {}
            }
        }
    }

    #[cfg(not(windows))]
    fn drain_tray(&mut self, _ctx: &egui::Context) {}

    #[cfg(windows)]
    fn drain_worker(&mut self, ctx: &egui::Context) {
        let Some(worker) = &self.worker else {
            return;
        };
        let current = self.generation();
        let mut done = false;
        while let Ok(msg) = worker.rx.try_recv() {
            let (epoch, ai, outcome) = match msg {
                WorkerMsg::Done {
                    epoch,
                    ai,
                    output,
                    translate_error,
                } => (epoch, ai, Ok((output, translate_error))),
                WorkerMsg::Failed { epoch, ai, message } => (epoch, ai, Err(message)),
            };
            if epoch != current {
                continue;
            }
            if ai {
                self.pending_ai = false;
            }
            match outcome {
                Ok((output, translate_error)) => {
                    self.result = Some(Ok(output));
                    self.translate_error = translate_error;
                    self.result_ai = ai;
                }
                Err(message) => {
                    self.result = Some(Err(message));
                    self.translate_error = None;
                }
            }
            done = true;
        }
        if done {
            self.busy = false;
            self.busy_since = None;
            ctx.request_repaint();
        }
    }

    #[cfg(not(windows))]
    fn drain_worker(&mut self, _ctx: &egui::Context) {}
}

#[cfg(windows)]
fn send(tx: &std::sync::mpsc::Sender<WorkerMsg>, msg: WorkerMsg) {
    let _ = tx.send(msg);
}

#[cfg(windows)]
fn recognize_local(frame: &selinga_capture::Frame) -> Result<Vec<selinga_ocr::OcrLine>, String> {
    let ocr = selinga_ocr::win32::WinOcr::new(None).map_err(|e| format!("ocr engine: {e}"))?;
    ocr.recognize(frame).map_err(|e| format!("ocr: {e}"))
}

#[cfg(windows)]
const AI_TARGET_LINE_PX: f32 = 20.0;

#[cfg(windows)]
fn ai_pass(
    frame: &selinga_capture::Frame,
    config: &selinga_translate::vlm::VlmConfig,
    target: &str,
) -> Result<(Vec<selinga_ocr::OcrLine>, Vec<String>), String> {
    let endpoint = selinga_ocr::vlm::VlmEndpoint {
        base_url: config.base_url.clone(),
        api_key: config.api_key.clone(),
        model: config.model.clone(),
    };
    let local = recognize_local(frame).unwrap_or_default();
    let scale = ai_scale_factor(median_line_height(&local), frame.width, frame.height);
    let enlarged = (scale > 1).then(|| frame.upscaled(scale));
    let input = enlarged.as_ref().unwrap_or(frame);

    let merged = selinga_ocr::vlm::VlmOcr::new(endpoint)
        .recognize_and_translate(input, target, local.len())
        .map_err(|e| format!("AI 识图: {e}"))?;
    if merged.is_empty() {
        return translate_local(&local, config, target);
    }
    let sources: Vec<String> = merged.iter().map(|line| line.text.clone()).collect();
    let mut translations: Vec<Option<String>> =
        merged.iter().map(|line| line.translated.clone()).collect();
    if translations.iter().any(Option::is_none) {
        let vlm = selinga_translate::vlm::VlmTranslator::new(config.clone(), target);
        match vlm.translate_lines(&sources) {
            Ok(filled) if filled.len() == sources.len() => {
                translations = filled.into_iter().map(Some).collect();
            }
            Ok(_) => {}
            Err(e) => return Err(format!("AI 翻译: {e}")),
        }
    }
    let lines: Vec<selinga_ocr::OcrLine> = merged
        .into_iter()
        .map(|line| selinga_ocr::OcrLine {
            text: line.text,
            rect: line.rect,
            words: Vec::new(),
        })
        .collect();
    let lines = merge_layout(lines, &local, frame.width as f32, frame.height as f32);
    let translated: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            translations
                .get(index)
                .and_then(|t| t.clone())
                .unwrap_or_else(|| line.text.clone())
        })
        .collect();
    Ok((lines, translated))
}

#[cfg(windows)]
fn translate_local(
    local: &[selinga_ocr::OcrLine],
    config: &selinga_translate::vlm::VlmConfig,
    target: &str,
) -> Result<(Vec<selinga_ocr::OcrLine>, Vec<String>), String> {
    if local.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let sources: Vec<String> = local.iter().map(|line| line.text.clone()).collect();
    let vlm = selinga_translate::vlm::VlmTranslator::new(config.clone(), target);
    let translated = vlm
        .translate_lines(&sources)
        .map_err(|e| format!("AI 翻译: {e}"))?;
    Ok((local.to_vec(), translated))
}

#[cfg(windows)]
fn median_line_height(lines: &[selinga_ocr::OcrLine]) -> Option<f32> {
    let mut heights: Vec<f32> = lines
        .iter()
        .filter_map(|line| line.rect)
        .map(|rect| rect.h)
        .filter(|h| *h > 0.5)
        .collect();
    if heights.is_empty() {
        return None;
    }
    heights.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(heights[heights.len() / 2])
}

#[cfg(windows)]
fn ai_scale_factor(median_line_px: Option<f32>, width: u32, height: u32) -> u32 {
    let longest = width.max(height).max(1);
    let limit = (selinga_ocr::vlm::MAX_SIDE / longest).max(1);
    let wanted = match median_line_px {
        Some(h) if h > 0.5 => (AI_TARGET_LINE_PX / h).ceil().clamp(1.0, 3.0) as u32,
        _ => {
            if longest < 600 {
                2
            } else {
                1
            }
        }
    };
    wanted.min(limit).max(1)
}

#[cfg(windows)]
fn merge_layout(
    vlm: Vec<selinga_ocr::OcrLine>,
    local: &[selinga_ocr::OcrLine],
    width: f32,
    height: f32,
) -> Vec<selinga_ocr::OcrLine> {
    let count = vlm.len();
    let boxes: Vec<selinga_ocr::Rect> = local.iter().filter_map(|line| line.rect).collect();
    let region = union_of(&boxes).unwrap_or(selinga_ocr::Rect {
        x: 0.0,
        y: 0.0,
        w: width,
        h: height,
    });
    let even = boxes.len() == count;
    let tolerance = region.h / count.max(1) as f32;
    let mut used = vec![false; boxes.len()];
    let mut out = Vec::with_capacity(count);
    for (index, mut line) in vlm.into_iter().enumerate() {
        if line.rect.is_none() {
            let borrowed = if even {
                Some(boxes[index])
            } else {
                match nearest_unused(&boxes, &used, region, index, count) {
                    Some((slot, distance)) if distance <= tolerance => {
                        used[slot] = true;
                        Some(boxes[slot])
                    }
                    _ => None,
                }
            };
            line.rect = Some(borrowed.unwrap_or_else(|| row_rect(region, index, count)));
        }
        out.push(line);
    }
    out
}

#[cfg(windows)]
fn union_of(rects: &[selinga_ocr::Rect]) -> Option<selinga_ocr::Rect> {
    let first = *rects.first()?;
    Some(rects[1..].iter().fold(first, |acc, r| {
        let x0 = acc.x.min(r.x);
        let y0 = acc.y.min(r.y);
        let x1 = (acc.x + acc.w).max(r.x + r.w);
        let y1 = (acc.y + acc.h).max(r.y + r.h);
        selinga_ocr::Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        }
    }))
}

#[cfg(windows)]
fn nearest_unused(
    boxes: &[selinga_ocr::Rect],
    used: &[bool],
    region: selinga_ocr::Rect,
    index: usize,
    count: usize,
) -> Option<(usize, f32)> {
    let expected = region.y + region.h * (index as f32 + 0.5) / count.max(1) as f32;
    boxes
        .iter()
        .enumerate()
        .filter(|(slot, _)| !used[*slot])
        .map(|(slot, rect)| (slot, (rect.y + rect.h / 2.0 - expected).abs()))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
}

#[cfg(windows)]
fn row_rect(region: selinga_ocr::Rect, index: usize, count: usize) -> selinga_ocr::Rect {
    let row_h = region.h / count.max(1) as f32;
    selinga_ocr::Rect {
        x: region.x,
        y: region.y + row_h * index as f32,
        w: region.w,
        h: row_h,
    }
}

#[cfg(windows)]
fn frame_hash(frame: &selinga_capture::Frame) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let step = (frame.bgra.len() / 8192).max(1);
    for byte in frame.bgra.iter().step_by(step) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn fit_font_size(ctx: &egui::Context, text: &str, max_w: f32, max_h: f32) -> f32 {
    let fits = |size: f32| {
        let galley = ctx.fonts_mut(|f| {
            f.layout_no_wrap(
                text.to_owned(),
                egui::FontId::proportional(size),
                egui::Color32::WHITE,
            )
        });
        let size = galley.size();
        size.x <= max_w && size.y <= max_h
    };
    let (mut lo, mut hi) = (8.0_f32, 32.0_f32.min(max_h.max(8.0)));
    if !fits(lo) {
        return lo;
    }
    if fits(hi) {
        return hi;
    }
    for _ in 0..6 {
        let mid = (lo + hi) / 2.0;
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn resize_direction(rect: egui::Rect, pos: Option<egui::Pos2>) -> Option<egui::ResizeDirection> {
    let pos = pos?;
    if !rect.expand(2.0).contains(pos) {
        return None;
    }
    let near_left = pos.x - rect.left() < HANDLE_MARGIN;
    let near_right = rect.right() - pos.x < HANDLE_MARGIN;
    let near_top = pos.y - rect.top() < HANDLE_MARGIN;
    let near_bottom = rect.bottom() - pos.y < HANDLE_MARGIN;
    let dir = match (near_left, near_right, near_top, near_bottom) {
        (true, false, false, false) => egui::ResizeDirection::West,
        (false, true, false, false) => egui::ResizeDirection::East,
        (false, false, true, false) => egui::ResizeDirection::North,
        (false, false, false, true) => egui::ResizeDirection::South,
        (true, false, true, false) => egui::ResizeDirection::NorthWest,
        (false, true, true, false) => egui::ResizeDirection::NorthEast,
        (true, false, false, true) => egui::ResizeDirection::SouthWest,
        (false, true, false, true) => egui::ResizeDirection::SouthEast,
        _ => return None,
    };
    if near_left || near_right || near_top || near_bottom {
        Some(dir)
    } else {
        None
    }
}

fn clamp_rect(rect: egui::Rect, screen_w: f32, screen_h: f32) -> egui::Rect {
    let width = rect.width().min(screen_w);
    let height = rect.height().min(screen_h);
    let x = rect.min.x.clamp(0.0, (screen_w - width).max(0.0));
    let y = rect.min.y.clamp(0.0, (screen_h - height).max(0.0));
    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(width, height))
}

fn resize_rect(start: egui::Rect, delta: egui::Vec2, dir: egui::ResizeDirection) -> egui::Rect {
    let mut r = start;
    let west = (start.left() + delta.x).min(start.right() - MIN_SELECT);
    let east = (start.right() + delta.x).max(start.left() + MIN_SELECT);
    let north = (start.top() + delta.y).min(start.bottom() - MIN_SELECT);
    let south = (start.bottom() + delta.y).max(start.top() + MIN_SELECT);
    match dir {
        egui::ResizeDirection::North => r.set_top(north),
        egui::ResizeDirection::South => r.set_bottom(south),
        egui::ResizeDirection::West => r.set_left(west),
        egui::ResizeDirection::East => r.set_right(east),
        egui::ResizeDirection::NorthWest => {
            r.set_top(north);
            r.set_left(west);
        }
        egui::ResizeDirection::NorthEast => {
            r.set_top(north);
            r.set_right(east);
        }
        egui::ResizeDirection::SouthWest => {
            r.set_bottom(south);
            r.set_left(west);
        }
        egui::ResizeDirection::SouthEast => {
            r.set_bottom(south);
            r.set_right(east);
        }
    }
    r
}

fn resize_cursor(dir: egui::ResizeDirection) -> egui::CursorIcon {
    match dir {
        egui::ResizeDirection::East | egui::ResizeDirection::West => {
            egui::CursorIcon::ResizeHorizontal
        }
        egui::ResizeDirection::North | egui::ResizeDirection::South => {
            egui::CursorIcon::ResizeVertical
        }
        egui::ResizeDirection::NorthWest | egui::ResizeDirection::SouthEast => {
            egui::CursorIcon::ResizeNwSe
        }
        egui::ResizeDirection::NorthEast | egui::ResizeDirection::SouthWest => {
            egui::CursorIcon::ResizeNeSw
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriggerKind {
    Debounced,
    Poll,
}

#[derive(Debug, Clone, Copy)]
struct TriggerInput {
    pointer_idle: bool,
    paused: bool,
    hidden: bool,
    busy: bool,
    dragging: bool,
    resizing: bool,
    stable_for: Option<Duration>,
    since_trigger: Option<Duration>,
}

fn trigger_decision(input: TriggerInput) -> Option<TriggerKind> {
    if !input.pointer_idle
        || input.paused
        || input.hidden
        || input.busy
        || input.dragging
        || input.resizing
    {
        return None;
    }
    match input.stable_for {
        Some(stable) => (stable >= DEBOUNCE).then_some(TriggerKind::Debounced),
        None => input
            .since_trigger
            .is_some_and(|d| d >= POLL_INTERVAL)
            .then_some(TriggerKind::Poll),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> TriggerInput {
        TriggerInput {
            pointer_idle: true,
            paused: false,
            hidden: false,
            busy: false,
            dragging: false,
            resizing: false,
            stable_for: None,
            since_trigger: None,
        }
    }

    #[test]
    fn settled_geometry_triggers_debounce() {
        let input = TriggerInput {
            stable_for: Some(DEBOUNCE + Duration::from_millis(1)),
            ..idle()
        };
        assert_eq!(trigger_decision(input), Some(TriggerKind::Debounced));
    }

    #[test]
    fn settling_geometry_waits_for_debounce() {
        let input = TriggerInput {
            stable_for: Some(DEBOUNCE - Duration::from_millis(1)),
            ..idle()
        };
        assert_eq!(trigger_decision(input), None);
    }

    #[test]
    fn static_box_polls_after_interval() {
        let input = TriggerInput {
            since_trigger: Some(POLL_INTERVAL),
            ..idle()
        };
        assert_eq!(trigger_decision(input), Some(TriggerKind::Poll));
    }

    #[test]
    fn static_box_waits_for_poll_interval() {
        let input = TriggerInput {
            since_trigger: Some(POLL_INTERVAL - Duration::from_millis(1)),
            ..idle()
        };
        assert_eq!(trigger_decision(input), None);
    }

    #[test]
    fn dragging_suppresses_both_channels() {
        let input = TriggerInput {
            dragging: true,
            stable_for: Some(DEBOUNCE * 4),
            since_trigger: Some(POLL_INTERVAL * 4),
            ..idle()
        };
        assert_eq!(trigger_decision(input), None);
    }

    #[test]
    fn resizing_busy_paused_hidden_and_pointer_all_suppress() {
        let flags = [
            TriggerInput {
                resizing: true,
                ..idle()
            },
            TriggerInput {
                busy: true,
                ..idle()
            },
            TriggerInput {
                paused: true,
                ..idle()
            },
            TriggerInput {
                hidden: true,
                ..idle()
            },
            TriggerInput {
                pointer_idle: false,
                ..idle()
            },
        ];
        for mut input in flags {
            input.stable_for = Some(DEBOUNCE * 4);
            assert_eq!(trigger_decision(input), None);
        }
    }

    #[test]
    fn never_triggered_box_stays_idle() {
        assert_eq!(trigger_decision(idle()), None);
    }

    #[test]
    fn embedded_icon_decodes_to_rgba() {
        let (rgba, width, height) = app_icon().expect("icon.png must decode");
        assert_eq!(width, 256);
        assert_eq!(height, 256);
        assert_eq!(rgba.len(), (width * height * 4) as usize);
    }

    #[test]
    fn selection_region_maps_points_to_physical() {
        let marquee = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(300.0, 200.0));
        let region = selection_region(marquee, (-1920, 0), 1.5);
        assert_eq!(region.x, -1770);
        assert_eq!(region.y, 75);
        assert_eq!(region.w, 450);
        assert_eq!(region.h, 300);
    }

    #[test]
    fn region_to_logical_uses_the_monitor_scale() {
        let region = selinga_capture::Region {
            x: 1920,
            y: 0,
            w: 400,
            h: 300,
        };
        let (pos, size) = region_to_logical(region, 2.0);
        assert_eq!(pos, egui::pos2(960.0, 0.0));
        assert_eq!(size, egui::vec2(200.0, 150.0));
    }

    #[test]
    fn selection_and_placement_round_trip() {
        let region = selinga_capture::Region {
            x: -500,
            y: 120,
            w: 640,
            h: 480,
        };
        let (pos, size) = region_to_logical(region, 1.25);
        let back = selection_region(egui::Rect::from_min_size(pos, size), (0, 0), 1.25);
        assert_eq!(back, region);
    }

    #[test]
    fn clamp_keeps_the_box_on_screen() {
        let clamped = clamp_rect(
            egui::Rect::from_min_size(egui::pos2(-50.0, -20.0), egui::vec2(200.0, 100.0)),
            1000.0,
            800.0,
        );
        assert_eq!(clamped.min.x, 0.0);
        assert_eq!(clamped.min.y, 0.0);

        let clamped = clamp_rect(
            egui::Rect::from_min_size(egui::pos2(950.0, 780.0), egui::vec2(200.0, 100.0)),
            1000.0,
            800.0,
        );
        assert_eq!(clamped.max.x, 1000.0);
        assert_eq!(clamped.max.y, 800.0);
    }

    #[test]
    fn clamp_shrinks_oversized_boxes() {
        let clamped = clamp_rect(
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1200.0, 900.0)),
            1000.0,
            800.0,
        );
        assert_eq!(clamped.width(), 1000.0);
        assert_eq!(clamped.height(), 800.0);
    }

    #[test]
    fn display_mode_strings_map_to_modes() {
        assert_eq!(display_mode_from("bilingual"), DisplayMode::Bilingual);
        assert_eq!(display_mode_from("translation"), DisplayMode::Translation);
        assert_eq!(display_mode_from("side-by-side"), DisplayMode::Translation);
    }

    #[cfg(windows)]
    mod ai_layout {
        use super::*;

        fn line(text: &str, rect: Option<selinga_ocr::Rect>) -> selinga_ocr::OcrLine {
            selinga_ocr::OcrLine {
                text: text.to_owned(),
                rect,
                words: Vec::new(),
            }
        }

        fn boxed(x: f32, y: f32, w: f32, h: f32) -> Option<selinga_ocr::Rect> {
            Some(selinga_ocr::Rect { x, y, w, h })
        }

        #[test]
        fn model_boxes_are_kept() {
            let vlm = vec![line("a", boxed(5.0, 5.0, 50.0, 12.0))];
            let local = vec![line("x", boxed(0.0, 0.0, 300.0, 100.0))];
            let merged = merge_layout(vlm, &local, 300.0, 200.0);
            assert_eq!(merged[0].rect.unwrap().w, 50.0);
        }

        #[test]
        fn text_only_reply_borrows_local_geometry() {
            let vlm = vec![line("a", None), line("b", None)];
            let local = vec![
                line("x", boxed(10.0, 10.0, 100.0, 20.0)),
                line("y", boxed(10.0, 40.0, 90.0, 18.0)),
            ];
            let merged = merge_layout(vlm, &local, 300.0, 200.0);
            assert_eq!(merged[0].rect.unwrap().h, 20.0);
            assert_eq!(merged[1].rect.unwrap().y, 40.0);
            assert_eq!(merged[1].rect.unwrap().w, 90.0);
        }

        #[test]
        fn mismatched_counts_still_get_ordered_rects() {
            let vlm = vec![line("a", None), line("b", None), line("c", None)];
            let local = vec![line("x", boxed(0.0, 0.0, 100.0, 20.0))];
            let merged = merge_layout(vlm, &local, 400.0, 300.0);
            let rects: Vec<_> = merged.iter().filter_map(|l| l.rect).collect();
            assert_eq!(rects.len(), 3);
            assert!(rects.windows(2).all(|pair| pair[0].y <= pair[1].y));
        }

        #[test]
        fn without_local_text_rows_span_the_crop() {
            let vlm = vec![line("a", None), line("b", None)];
            let merged = merge_layout(vlm, &[], 640.0, 400.0);
            assert_eq!(merged[0].rect.unwrap().w, 640.0);
            assert_eq!(merged[0].rect.unwrap().h, 200.0);
            assert_eq!(merged[1].rect.unwrap().y, 200.0);
        }

        #[test]
        fn small_glyphs_are_enlarged() {
            assert_eq!(ai_scale_factor(Some(20.0), 400, 200), 1);
            assert_eq!(ai_scale_factor(Some(10.0), 400, 200), 2);
            assert_eq!(ai_scale_factor(Some(5.0), 400, 200), 3);
        }

        #[test]
        fn enlargement_stays_under_the_model_ceiling() {
            assert_eq!(ai_scale_factor(Some(5.0), 2000, 400), 1);
            assert_eq!(ai_scale_factor(Some(5.0), 700, 300), 2);
        }

        #[test]
        fn unknown_text_size_uses_crop_size_as_the_hint() {
            assert_eq!(ai_scale_factor(None, 300, 200), 2);
            assert_eq!(ai_scale_factor(None, 900, 400), 1);
        }

        #[test]
        fn median_height_ignores_lines_without_boxes() {
            let lines = vec![
                line("a", None),
                line("b", boxed(0.0, 0.0, 10.0, 14.0)),
                line("c", boxed(0.0, 0.0, 10.0, 24.0)),
            ];
            assert_eq!(median_line_height(&lines), Some(24.0));
        }
    }
}
