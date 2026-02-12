#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{Datelike, Days, Local, LocalResult, NaiveDate, NaiveDateTime, TimeZone};
use csv::{ReaderBuilder, StringRecord};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke};
use rusqlite::{backup::Backup, params, Connection};
use serde_json::json;

#[cfg(target_os = "windows")]
use std::ffi::c_void;
#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
    BI_RGB, DIB_RGB_COLORS,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::OpenMutexW;
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::Shell::ExtractIconExW;
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

#[derive(Debug, Clone)]
struct Segment {
    start_ts: i64,
    end_ts: i64,
    is_idle: bool,
    app_name: String,
    process_path: Option<String>,
    title: Option<String>,
}

#[derive(Debug, Clone)]
struct SummaryRow {
    app_name: String,
    display_name: String,
    duration_secs: i64,
    process_path: Option<String>,
    is_idle: bool,
}

#[derive(Debug, Clone)]
struct ExportSegmentRow {
    start_ts: i64,
    end_ts: i64,
    is_idle: bool,
    app_name: String,
    process_path: Option<String>,
    title: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ImportStats {
    total_rows: usize,
    imported_rows: usize,
    skipped_rows: usize,
}

#[derive(Debug, Clone)]
struct ImportCsvColumns {
    title: Option<usize>,
    start_local: Option<usize>,
    end_local: Option<usize>,
    duration: Option<usize>,
    process: Option<usize>,
    start_ts: Option<usize>,
    end_ts: Option<usize>,
    is_idle: Option<usize>,
    app_name: Option<usize>,
    process_path: Option<usize>,
}

impl ImportCsvColumns {
    fn from_headers(headers: &StringRecord) -> Result<Self> {
        let columns = Self {
            title: find_csv_header_index(headers, &["title", "name"]),
            start_local: find_csv_header_index(headers, &["start", "startlocal"]),
            end_local: find_csv_header_index(headers, &["end", "endlocal"]),
            duration: find_csv_header_index(headers, &["duration", "durationsecs"]),
            process: find_csv_header_index(headers, &["process"]),
            start_ts: find_csv_header_index(headers, &["startts", "start_ts"]),
            end_ts: find_csv_header_index(headers, &["endts", "end_ts"]),
            is_idle: find_csv_header_index(headers, &["isidle", "is_idle"]),
            app_name: find_csv_header_index(headers, &["appname", "app_name"]),
            process_path: find_csv_header_index(headers, &["processpath", "process_path"]),
        };

        let has_time_columns = (columns.start_ts.is_some() && columns.end_ts.is_some())
            || (columns.start_local.is_some() && columns.end_local.is_some());
        if !has_time_columns {
            bail!(
                "CSV missing required time columns. Need Start/End or start_ts/end_ts."
            );
        }

        if columns.process.is_none() && columns.app_name.is_none() {
            bail!("CSV missing required app column. Need Process or app_name.");
        }

        Ok(columns)
    }
}

#[derive(Debug, Clone)]
struct ParsedImportRow {
    start_ts: i64,
    end_ts: i64,
    is_idle: bool,
    app_name: String,
    process_path: String,
    title: Option<String>,
}

#[derive(Debug, Clone)]
struct TimelineRenderSegment {
    start_ts: i64,
    end_ts: i64,
    is_idle: bool,
    app_name: String,
    process_path: Option<String>,
    title: Option<String>,
    multi_title: bool,
}

enum IconState {
    Pending,
    Loaded(egui::TextureHandle),
    Missing,
}

struct IconLoadResult {
    process_path: String,
    image: Option<egui::ColorImage>,
    dominant_color: Option<Color32>,
    display_name: Option<String>,
}

#[derive(Debug, Clone)]
struct CachedAppVisual {
    process_path: Option<String>,
    color: Color32,
    icon_size: Option<[usize; 2]>,
    icon_rgba: Option<Vec<u8>>,
    display_name: Option<String>,
}

struct ReloadRequest {
    request_id: u64,
    db_path: PathBuf,
    range_start: i64,
    range_end: i64,
}

struct ReloadPayload {
    segments: Vec<Segment>,
    summary_rows: Vec<SummaryRow>,
    summary_total_secs: i64,
}

struct ReloadResult {
    request_id: u64,
    payload: Result<ReloadPayload, String>,
}

#[derive(Debug, Clone, Copy)]
enum BackendStatusWorkerRequest {
    ProbeNow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangePreset {
    All,
    Day7,
    Day30,
    ThisWeek,
    ThisMonth,
    ThisQuarter,
    YearToDate,
}

impl RangePreset {
    fn short_label(self) -> &'static str {
        match self {
            Self::All => "ALL",
            Self::Day7 => "7D",
            Self::Day30 => "30D",
            Self::ThisWeek => "This Week",
            Self::ThisMonth => "This Month",
            Self::ThisQuarter => "This Quarter",
            Self::YearToDate => "YTD",
        }
    }

    fn ui_label(self, language: UiLanguage) -> &'static str {
        match language {
            UiLanguage::ZhCn => match self {
                Self::All => "\u{5168}\u{90E8}",
                Self::Day7 => "\u{6700}\u{8FD1}7\u{5929}",
                Self::Day30 => "\u{6700}\u{8FD1}30\u{5929}",
                Self::ThisWeek => "\u{672C}\u{5468}",
                Self::ThisMonth => "\u{672C}\u{6708}",
                Self::ThisQuarter => "\u{672C}\u{5B63}\u{5EA6}",
                Self::YearToDate => "\u{4ECA}\u{5E74}\u{81F3}\u{4ECA}",
            },
            UiLanguage::EnUs => match self {
                Self::All => "All",
                Self::Day7 => "Last 7 Days",
                Self::Day30 => "Last 30 Days",
                Self::ThisWeek => "This Week",
                Self::ThisMonth => "This Month",
                Self::ThisQuarter => "This Quarter",
                Self::YearToDate => "YTD",
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CustomRangeFocus {
    From,
    To,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiLanguage {
    ZhCn,
    EnUs,
}

impl UiLanguage {
    fn code(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::EnUs => "en-US",
        }
    }

    fn compact_label(self) -> &'static str {
        match self {
            Self::ZhCn => "\u{4E2D}\u{6587}",
            Self::EnUs => "EN",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        let normalized = code.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "zh" | "zh-cn" | "zh_hans" | "zh-hans" => Some(Self::ZhCn),
            "en" | "en-us" => Some(Self::EnUs),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportFormat {
    Csv,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendHealth {
    Running,
    Stopped,
}

#[derive(Debug, Clone)]
struct BackendStatus {
    health: BackendHealth,
    last_write_ts: Option<i64>,
    checked_ts: i64,
    detail: Option<String>,
}

impl BackendStatus {
    fn short_label_lang(&self, language: UiLanguage) -> &'static str {
        match (language, self.health) {
            (UiLanguage::ZhCn, BackendHealth::Running) => "\u{670D}\u{52A1}\u{8FD0}\u{884C}\u{4E2D}",
            (UiLanguage::ZhCn, BackendHealth::Stopped) => "\u{670D}\u{52A1}\u{672A}\u{8FD0}\u{884C}",
            (UiLanguage::EnUs, BackendHealth::Running) => "Tracking Active",
            (UiLanguage::EnUs, BackendHealth::Stopped) => "Service Not Running",
        }
    }

    fn color(&self) -> Color32 {
        match self.health {
            BackendHealth::Running => Color32::from_rgb(28, 136, 64),
            BackendHealth::Stopped => Color32::from_rgb(190, 56, 56),
        }
    }
}

struct TimelineApp {
    db_path: PathBuf,
    selected_date: NaiveDate,
    calendar_month: NaiveDate,
    range_preset: Option<RangePreset>,
    custom_range: Option<(NaiveDate, NaiveDate)>,
    custom_range_focus: CustomRangeFocus,
    custom_start_input: String,
    custom_end_input: String,
    summary_limit: Option<usize>,
    summary_limit_custom_input: String,
    timeline_view_range: Option<(i64, i64)>,
    segments: Vec<Segment>,
    summary_rows: Vec<SummaryRow>,
    summary_total_secs: i64,
    selected_app_keys: HashSet<String>,
    icon_cache: HashMap<String, IconState>,
    icon_color_cache: HashMap<String, Color32>,
    cached_app_visuals: HashMap<String, CachedAppVisual>,
    process_display_name_cache: HashMap<String, String>,
    app_color_cache: HashMap<String, Color32>,
    icon_request_tx: mpsc::Sender<String>,
    icon_result_rx: mpsc::Receiver<IconLoadResult>,
    reload_request_tx: mpsc::Sender<ReloadRequest>,
    reload_result_rx: mpsc::Receiver<ReloadResult>,
    backend_status_request_tx: mpsc::Sender<BackendStatusWorkerRequest>,
    backend_status_result_rx: mpsc::Receiver<BackendStatus>,
    next_reload_request_id: u64,
    pending_reload_request_id: Option<u64>,
    is_reloading: bool,
    pending_icon_refresh: bool,
    save_dir_override: Option<PathBuf>,
    save_dir_input: String,
    ui_language: UiLanguage,
    settings_path: PathBuf,
    export_format: ExportFormat,
    import_file_input: String,
    show_import_window: bool,
    show_export_window: bool,
    show_backup_window: bool,
    last_auto_refresh: Instant,
    backend_status: BackendStatus,
    error: Option<String>,
    info: Option<String>,
    info_expires_at: Option<Instant>,
    timeline_segments_cache: Arc<Vec<TimelineRenderSegment>>,
    timeline_cache_range: Option<(i64, i64)>,
    timeline_cache_dirty: bool,
}

const SCROLLBAR_SAFE_GUTTER: f32 = 16.0;
const MIN_TIMELINE_VIEW_SECS: i64 = 5 * 60;
const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(10);
const INFO_MESSAGE_TTL: Duration = Duration::from_secs(4);
const BACKEND_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(1);
const BACKEND_HEARTBEAT_GRACE_SECS: i64 = 180;
const TRACKER_DAEMON_MUTEX_NAME: &str = "Local\\LimeTraceBackendSingleton";
const APP_ICON_PNG: &[u8] = include_bytes!("../../../LimeTrace.png");
// Fixed timeline sizing. At 1280x720 startup, one-hour cell is close to golden ratio.
const TIMELINE_HEADER_HEIGHT: f32 = 24.0;
const TIMELINE_CHART_HEIGHT: f32 = 86.0;
const TIMELINE_FOOTER_HEIGHT: f32 = 28.0;
const TIMELINE_TOTAL_HEIGHT: f32 =
    TIMELINE_HEADER_HEIGHT + TIMELINE_CHART_HEIGHT + TIMELINE_FOOTER_HEIGHT;

impl TimelineApp {
    fn new(db_path: PathBuf) -> Self {
        let today = Local::now().date_naive();
        let (icon_request_tx, icon_result_rx) = spawn_icon_loader();
        let (reload_request_tx, reload_result_rx) = spawn_reload_worker();
        let (backend_status_request_tx, backend_status_result_rx) =
            spawn_backend_status_worker(db_path.clone());
        let default_save_dir = db_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let settings_path = default_save_dir.join("limetrace-settings.json");
        let ui_language = load_ui_language(&settings_path).unwrap_or_else(default_ui_language);
        let mut app = Self {
            db_path,
            selected_date: today,
            calendar_month: month_start(today),
            range_preset: None,
            custom_range: None,
            custom_range_focus: CustomRangeFocus::From,
            custom_start_input: today.format("%Y-%m-%d").to_string(),
            custom_end_input: today.format("%Y-%m-%d").to_string(),
            summary_limit: None,
            summary_limit_custom_input: "10".to_owned(),
            timeline_view_range: None,
            segments: Vec::new(),
            summary_rows: Vec::new(),
            summary_total_secs: 0,
            selected_app_keys: HashSet::new(),
            icon_cache: HashMap::new(),
            icon_color_cache: HashMap::new(),
            cached_app_visuals: HashMap::new(),
            process_display_name_cache: HashMap::new(),
            app_color_cache: HashMap::new(),
            icon_request_tx,
            icon_result_rx,
            reload_request_tx,
            reload_result_rx,
            backend_status_request_tx,
            backend_status_result_rx,
            next_reload_request_id: 0,
            pending_reload_request_id: None,
            is_reloading: false,
            pending_icon_refresh: false,
            save_dir_override: None,
            save_dir_input: default_save_dir.display().to_string(),
            ui_language,
            settings_path,
            export_format: ExportFormat::Csv,
            import_file_input: String::new(),
            show_import_window: false,
            show_export_window: false,
            show_backup_window: false,
            last_auto_refresh: Instant::now(),
            backend_status: BackendStatus {
                health: BackendHealth::Stopped,
                last_write_ts: None,
                checked_ts: unix_seconds_now(),
                detail: None,
            },
            error: None,
            info: None,
            info_expires_at: None,
            timeline_segments_cache: Arc::new(Vec::new()),
            timeline_cache_range: None,
            timeline_cache_dirty: true,
        };
        app.load_cached_app_visuals();
        app.reload();
        app.refresh_backend_status();
        app
    }

    fn set_info_message(&mut self, message: impl Into<String>) {
        self.error = None;
        self.info = Some(message.into());
        self.info_expires_at = Some(Instant::now() + INFO_MESSAGE_TTL);
    }

    fn clear_info_message(&mut self) {
        self.info = None;
        self.info_expires_at = None;
    }

    fn t(&self, key: &'static str) -> &'static str {
        tr(self.ui_language, key)
    }

    fn set_ui_language(&mut self, language: UiLanguage) {
        if self.ui_language == language {
            return;
        }
        self.ui_language = language;
        if let Err(err) = persist_ui_language(&self.settings_path, self.ui_language) {
            self.clear_info_message();
            self.error = Some(format!("failed to save language setting: {err:#}"));
        }
    }

    fn seed_app_color_cache_from_cached_visuals(&mut self) {
        for (app_key, visual) in &self.cached_app_visuals {
            self.app_color_cache.insert(app_key.clone(), visual.color);
            if let Some(path) = visual.process_path.as_deref() {
                self.icon_color_cache
                    .entry(path.to_owned())
                    .or_insert(visual.color);
                if let Some(name) = visual
                    .display_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    self.process_display_name_cache
                        .entry(path.to_owned())
                        .or_insert(name.to_owned());
                }
            }
        }
    }

    fn load_cached_app_visuals(&mut self) {
        let conn = match Connection::open(&self.db_path) {
            Ok(conn) => conn,
            Err(_) => return,
        };
        if conn.busy_timeout(Duration::from_millis(500)).is_err() {
            return;
        }
        if ensure_tracking_schema(&conn).is_err() {
            return;
        }
        if let Ok(cached) = load_cached_app_visuals_from_db(&conn) {
            self.cached_app_visuals = cached;
            self.seed_app_color_cache_from_cached_visuals();
        }
    }

    fn close_active_popup(ui: &mut egui::Ui) {
        ui.memory_mut(|mem| mem.close_popup());
    }

    fn invalidate_timeline_cache(&mut self) {
        self.timeline_cache_dirty = true;
    }

    fn reset_range_inputs_for_selected_date(&mut self) {
        let selected = self.selected_date.format("%Y-%m-%d").to_string();
        self.custom_start_input = selected.clone();
        self.custom_end_input = selected;
    }

    fn apply_range_change(&mut self) {
        self.selected_app_keys.clear();
        self.timeline_view_range = None;
        self.invalidate_timeline_cache();
        self.reload();
    }

    fn show_centered_window<F>(
        &mut self,
        ctx: &egui::Context,
        id: &'static str,
        title: &str,
        open: &mut bool,
        default_size: egui::Vec2,
        mut draw_content: F,
    ) where
        F: FnMut(&mut Self, &mut egui::Ui),
    {
        let screen_rect = ctx.screen_rect();
        let default_pos = Pos2::new(
            screen_rect.center().x - default_size.x * 0.5,
            screen_rect.center().y - default_size.y * 0.5,
        );
        egui::Window::new(title)
            .id(egui::Id::new(id))
            .open(open)
            .collapsible(false)
            .resizable(false)
            .default_size(default_size)
            .default_pos(default_pos)
            .show(ctx, |ui| {
                draw_content(self, ui);
            });
    }

    fn draw_save_path_action_row(&mut self, ui: &mut egui::Ui, action_key: &'static str) -> bool {
        let mut clicked = false;
        ui.horizontal(|ui| {
            ui.label(format!("{}:", self.t("path")));
            let path_width = (ui.available_width() - 64.0).max(140.0);
            ui.add_sized(
                [path_width, 22.0],
                egui::TextEdit::singleline(&mut self.save_dir_input),
            );
            clicked = ui.button(self.t(action_key)).clicked();
        });
        clicked
    }

    fn ensure_timeline_cache(&mut self, range_start: i64, range_end: i64) -> Arc<Vec<TimelineRenderSegment>> {
        let active_range = (range_start, range_end);
        if self.timeline_cache_dirty || self.timeline_cache_range != Some(active_range) {
            let timeline_filter_keys = self.effective_timeline_filter_keys();
            let rebuilt = build_timeline_segments(
                range_start,
                range_end,
                &self.segments,
                &timeline_filter_keys,
            );
            self.timeline_segments_cache = Arc::new(rebuilt);
            self.timeline_cache_range = Some(active_range);
            self.timeline_cache_dirty = false;
        }
        Arc::clone(&self.timeline_segments_cache)
    }

    fn visible_summary_count(&self) -> usize {
        match self.summary_limit {
            Some(limit) => limit.min(self.summary_rows.len()),
            None => self.summary_rows.len(),
        }
    }

    fn summary_limit_all_label(&self) -> &'static str {
        match self.ui_language {
            UiLanguage::ZhCn => "\u{5168}\u{90E8}",
            UiLanguage::EnUs => "All",
        }
    }

    fn apply_summary_limit_change(&mut self, previous_summary_limit: Option<usize>) {
        if self.summary_limit == previous_summary_limit {
            return;
        }

        let show_count = self.visible_summary_count();
        if show_count < self.summary_rows.len() {
            let allowed_keys: HashSet<String> = self
                .summary_rows
                .iter()
                .take(show_count)
                .map(|row| normalize_app_key(&row.app_name))
                .collect();
            self.selected_app_keys
                .retain(|selected| allowed_keys.contains(selected));
        }
        self.invalidate_timeline_cache();
    }

    fn effective_timeline_filter_keys(&self) -> HashSet<String> {
        let mut limit_keys: HashSet<String> = HashSet::new();
        let show_count = self.visible_summary_count();
        let limit_is_active = show_count < self.summary_rows.len();
        if limit_is_active {
            for row in self.summary_rows.iter().take(show_count) {
                limit_keys.insert(normalize_app_key(&row.app_name));
            }
        }

        if self.selected_app_keys.is_empty() {
            return limit_keys;
        }
        if !limit_is_active {
            return self.selected_app_keys.clone();
        }

        self.selected_app_keys
            .iter()
            .filter(|key| limit_keys.contains(*key))
            .cloned()
            .collect()
    }

    fn drain_reload_results(&mut self) {
        while let Ok(result) = self.reload_result_rx.try_recv() {
            if Some(result.request_id) != self.pending_reload_request_id {
                continue;
            }

            self.pending_reload_request_id = None;
            self.is_reloading = false;

            match result.payload {
                Ok(payload) => {
                    self.segments = payload.segments;
                    self.summary_rows = payload.summary_rows;
                    self.summary_total_secs = payload.summary_total_secs;
                    let valid_keys: HashSet<String> = self
                        .summary_rows
                        .iter()
                        .map(|row| normalize_app_key(&row.app_name))
                        .collect();
                    self.selected_app_keys
                        .retain(|selected| valid_keys.contains(selected));
                    self.app_color_cache.clear();
                    self.seed_app_color_cache_from_cached_visuals();
                    self.pending_icon_refresh = true;
                    self.error = None;
                    self.invalidate_timeline_cache();
                }
                Err(err) => {
                    self.error = Some(err);
                }
            }
        }
    }

    fn reload(&mut self) {
        self.last_auto_refresh = Instant::now();
        let Some((range_start, range_end)) = self.active_range_bounds() else {
            self.error = Some("failed to resolve active range".to_owned());
            return;
        };

        self.next_reload_request_id = self.next_reload_request_id.wrapping_add(1);
        let request_id = self.next_reload_request_id;
        self.pending_reload_request_id = Some(request_id);
        self.is_reloading = true;

        if self
            .reload_request_tx
            .send(ReloadRequest {
                request_id,
                db_path: self.db_path.clone(),
                range_start,
                range_end,
            })
            .is_err()
        {
            self.pending_reload_request_id = None;
            self.is_reloading = false;
            self.error = Some("reload worker unavailable".to_owned());
        }
    }

    fn set_selected_date(&mut self, date: NaiveDate) {
        self.selected_date = date;
        self.calendar_month = month_start(date);
        self.range_preset = None;
        self.custom_range = None;
        self.reset_range_inputs_for_selected_date();
        self.apply_range_change();
    }

    fn set_range_preset(&mut self, preset: RangePreset) {
        if self.range_preset != Some(preset) {
            self.range_preset = Some(preset);
            self.custom_range = None;
            self.reset_range_inputs_for_selected_date();
            self.apply_range_change();
        }
    }

    fn clear_range_preset(&mut self) {
        if self.range_preset.is_some() || self.custom_range.is_some() {
            self.range_preset = None;
            self.custom_range = None;
            self.reset_range_inputs_for_selected_date();
            self.apply_range_change();
        }
    }

    fn set_custom_range(&mut self, start: NaiveDate, end: NaiveDate) {
        self.range_preset = None;
        self.custom_range = Some((start, end));
        self.custom_start_input = start.format("%Y-%m-%d").to_string();
        self.custom_end_input = end.format("%Y-%m-%d").to_string();
        self.apply_range_change();
    }

    fn activate_custom_range(&mut self) {
        let (start, end) = self
            .active_range_dates()
            .unwrap_or((self.selected_date, self.selected_date));
        self.custom_range_focus = CustomRangeFocus::From;
        self.calendar_month = month_start(start);
        if self.range_preset.is_some() || self.custom_range.is_none() {
            self.set_custom_range(start, end);
        }
    }

    fn active_range_bounds(&self) -> Option<(i64, i64)> {
        if let Some((start_date, end_date)) = self.custom_range {
            let end_exclusive = end_date.checked_add_days(Days::new(1))?;
            return date_range_bounds(start_date, end_exclusive);
        }
        if let Some(preset) = self.range_preset {
            return range_bounds_for_preset(self.selected_date, preset);
        }
        let end_exclusive = self.selected_date.checked_add_days(Days::new(1))?;
        date_range_bounds(self.selected_date, end_exclusive)
    }

    fn active_range_dates(&self) -> Option<(NaiveDate, NaiveDate)> {
        if let Some((start, end)) = self.custom_range {
            return Some((start, end));
        }
        if let Some(preset) = self.range_preset {
            return range_dates_for_preset(self.selected_date, preset);
        }
        Some((self.selected_date, self.selected_date))
    }

    fn shift_day(&mut self, offset_days: i64) {
        let shifted = if offset_days >= 0 {
            self.selected_date
                .checked_add_days(Days::new(offset_days as u64))
        } else {
            self.selected_date
                .checked_sub_days(Days::new((-offset_days) as u64))
        };

        if let Some(new_date) = shifted {
            self.set_selected_date(new_date);
        }
    }

    fn shift_calendar_month(&mut self, offset_months: i32) {
        if let Some(next) = add_months(self.calendar_month, offset_months) {
            self.calendar_month = next;
        }
    }

    fn draw_monthly_calendar(&mut self, ui: &mut egui::Ui, selected_date: NaiveDate) -> Option<NaiveDate> {
        ui.set_min_width(250.0);
        ui.horizontal(|ui| {
            if ui.button("<").clicked() {
                self.shift_calendar_month(-1);
            }
            ui.add_space(6.0);
            ui.label(self.calendar_month.format("%Y-%m").to_string());
            ui.add_space(6.0);
            if ui.button(">").clicked() {
                self.shift_calendar_month(1);
            }
        });
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            for name in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"] {
                ui.add_sized(
                    [32.0, 18.0],
                    egui::Label::new(egui::RichText::new(name).small()),
                );
            }
        });

        let first_weekday = self.calendar_month.weekday().num_days_from_monday() as usize;
        let total_days = days_in_month(self.calendar_month);
        let today = Local::now().date_naive();
        let mut day: u32 = 1;
        let mut picked_date: Option<NaiveDate> = None;

        for row in 0..6 {
            ui.horizontal(|ui| {
                for col in 0..7 {
                    let cell = row * 7 + col;
                    if cell < first_weekday || day > total_days {
                        ui.add_sized([32.0, 24.0], egui::Label::new(""));
                        continue;
                    }

                    let Some(date) = NaiveDate::from_ymd_opt(
                        self.calendar_month.year(),
                        self.calendar_month.month(),
                        day,
                    ) else {
                        day += 1;
                        continue;
                    };

                    let mut text = egui::RichText::new(day.to_string());
                    let mut button = egui::Button::new(text.clone())
                        .min_size(egui::vec2(32.0, 24.0))
                        .frame(false);

                    if date == selected_date {
                        let selection = ui.visuals().selection;
                        text = text.color(selection.stroke.color);
                        button = egui::Button::new(text)
                            .min_size(egui::vec2(32.0, 24.0))
                            .fill(selection.bg_fill)
                            .stroke(Stroke::NONE);
                    } else if date == today {
                        button = button
                            .frame(true)
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::new(1.0, Color32::from_rgb(80, 130, 210)));
                    }

                    let response = ui.add(button);
                    if response.hovered() && date != selected_date {
                        let hover_rect = response.rect.shrink2(egui::vec2(1.0, 1.0));
                        ui.painter().rect_filled(
                            hover_rect,
                            4.0,
                            Color32::from_rgba_unmultiplied(100, 100, 100, 108),
                        );
                    }

                    if response.clicked() {
                        picked_date = Some(date);
                    }
                    day += 1;
                }
            });
            if day > total_days {
                break;
            }
        }

        picked_date
    }

    fn draw_date_picker(&mut self, ui: &mut egui::Ui) {
        if let Some(date) = self.draw_monthly_calendar(ui, self.selected_date) {
            self.set_selected_date(date);
            Self::close_active_popup(ui);
        }
    }

    fn draw_custom_range_picker(&mut self, ui: &mut egui::Ui) {
        let (start, end) = self
            .active_range_dates()
            .unwrap_or((self.selected_date, self.selected_date));

        ui.set_min_width(280.0);
        ui.add_space(4.0);

        let focus_date = match self.custom_range_focus {
            CustomRangeFocus::From => start,
            CustomRangeFocus::To => end,
        };
        if let Some(picked_date) = self.draw_monthly_calendar(ui, focus_date) {
            let (mut new_start, mut new_end) = (start, end);
            match self.custom_range_focus {
                CustomRangeFocus::From => {
                    new_start = picked_date;
                    if new_end < new_start {
                        new_end = new_start;
                    }
                }
                CustomRangeFocus::To => {
                    new_end = picked_date;
                    if new_end < new_start {
                        new_start = new_end;
                    }
                }
            }
            if new_start != start || new_end != end {
                self.set_custom_range(new_start, new_end);
            }
        }

    }

    fn draw_range_picker(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(80.0);
        let day_selected = self.range_preset.is_none() && self.custom_range.is_none();
        if ui
            .selectable_label(day_selected, self.t("single_day"))
            .clicked()
        {
            self.clear_range_preset();
            Self::close_active_popup(ui);
            return;
        }

        let custom_selected = self.custom_range.is_some();
        if ui
            .selectable_label(custom_selected, self.t("custom"))
            .clicked()
        {
            self.activate_custom_range();
            Self::close_active_popup(ui);
            return;
        }

        ui.separator();

        for preset in [
            RangePreset::All,
            RangePreset::Day7,
            RangePreset::Day30,
            RangePreset::ThisWeek,
            RangePreset::ThisMonth,
            RangePreset::ThisQuarter,
            RangePreset::YearToDate,
        ] {
            let selected = self.range_preset == Some(preset);
            if ui
                .selectable_label(selected, preset.ui_label(self.ui_language))
                .clicked()
            {
                self.set_range_preset(preset);
                Self::close_active_popup(ui);
            }
        }
    }

    fn draw_summary_rows(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
    ) {
        if self.summary_rows.is_empty() {
            ui.label(self.t("no_data"));
            return;
        }

        let total_secs = self.summary_total_secs;
        let show_count = self.visible_summary_count();

        for row_idx in 0..show_count {
            if let Some(row) = self.summary_rows.get(row_idx).cloned() {
                self.draw_summary_row(ctx, ui, row_idx, &row, total_secs);
            }
        }
    }

    fn draw_summary_row(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        _row_idx: usize,
        row: &SummaryRow,
        total_secs: i64,
    ) {
        let app_key = normalize_app_key(&row.app_name);
        let is_selected = self.selected_app_keys.contains(&app_key);
        let dark_mode = ui.visuals().dark_mode;
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), Sense::click());
        if is_selected {
            let selected_fill = if response.hovered() {
                if dark_mode {
                    Color32::from_rgb(60, 84, 122)
                } else {
                    Color32::from_rgb(203, 219, 242)
                }
            } else {
                if dark_mode {
                    Color32::from_rgb(49, 72, 107)
                } else {
                    Color32::from_rgb(216, 228, 246)
                }
            };
            ui.painter().rect_filled(rect, 4.0, selected_fill);
        } else if response.hovered() {
            let hover_fill = if dark_mode {
                Color32::from_rgb(48, 48, 48)
            } else {
                Color32::from_rgb(230, 230, 230)
            };
            ui.painter().rect_filled(rect, 4.0, hover_fill);
        }
        if response.clicked() {
            let before = self.selected_app_keys.clone();
            let ctrl_pressed = ui.input(|i| i.modifiers.ctrl || i.modifiers.command);
            if ctrl_pressed {
                if is_selected {
                    self.selected_app_keys.remove(&app_key);
                } else {
                    self.selected_app_keys.insert(app_key);
                }
            } else {
                // Single-click keeps single-select behavior, but allows deselecting
                // when the currently selected row is clicked again.
                let should_deselect_all = is_selected && self.selected_app_keys.len() == 1;
                self.selected_app_keys.clear();
                if !should_deselect_all {
                    self.selected_app_keys.insert(app_key);
                }
            }
            if self.selected_app_keys != before {
                self.invalidate_timeline_cache();
            }
        }

        let ratio = if total_secs > 0 {
            (row.duration_secs as f32 / total_secs as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let percent_text = format!("{:>5.1}%", ratio * 100.0);
        let duration_text = format_duration(row.duration_secs);

        let mut content_rect = rect.shrink2(egui::vec2(6.0, 3.0));
        content_rect.max.x = (content_rect.max.x - SCROLLBAR_SAFE_GUTTER)
            .max(content_rect.min.x + 1.0);
        let gap = 8.0;
        let row_width = content_rect.width().max(1.0);
        let duration_width = 88.0;
        let fixed_right = duration_width + gap * 2.0;
        let available_left = (row_width - fixed_right).max(40.0);
        let mut name_width = (available_left * 0.50).clamp(90.0, 620.0);
        let mut bar_width = (available_left - name_width).max(52.0);
        if name_width + bar_width > available_left {
            bar_width = (available_left - name_width).max(40.0);
            name_width = (available_left - bar_width).max(70.0);
        }

        let mut x = content_rect.left();
        let y = content_rect.top();
        let h = content_rect.height();
        let name_rect = Rect::from_min_size(Pos2::new(x, y), egui::vec2(name_width, h));
        x += name_width + gap;
        let bar_rect = Rect::from_min_size(Pos2::new(x, y), egui::vec2(bar_width, h));
        x += bar_width + gap;
        let duration_rect = Rect::from_min_size(Pos2::new(x, y), egui::vec2(duration_width, h));

        let painter = ui.painter();
        let text_color = ui.visuals().text_color();

        let icon_rect = Rect::from_center_size(
            Pos2::new(name_rect.left() + 10.0, name_rect.center().y),
            egui::vec2(16.0, 16.0),
        );
        let row_color =
            self.display_color_for(row.is_idle, &row.app_name, row.process_path.as_deref());
        if let Some(texture_id) = self.icon_texture_id(ctx, row) {
            painter.image(
                texture_id,
                icon_rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        } else {
            draw_fallback_icon(painter, icon_rect, row_color);
        }

        let name_text_rect = Rect::from_min_max(
            Pos2::new(icon_rect.right() + 4.0, name_rect.top()),
            Pos2::new(name_rect.right(), name_rect.bottom()),
        );
        let name_font = ui
            .style()
            .text_styles
            .get(&egui::TextStyle::Body)
            .cloned()
            .unwrap_or_else(|| FontId::proportional(18.0));
        let display_name = self.display_name_for_summary_row(row);
        painter
            .with_clip_rect(name_text_rect)
            .text(
                Pos2::new(name_text_rect.left(), name_text_rect.center().y),
                Align2::LEFT_CENTER,
                display_name,
                name_font,
                text_color,
            );

        let bar_shape = Rect::from_center_size(
            Pos2::new(bar_rect.center().x, bar_rect.center().y),
            egui::vec2(bar_rect.width(), 18.0),
        );
        let bar_bg = if dark_mode {
            Color32::from_rgb(68, 68, 68)
        } else {
            Color32::from_rgb(240, 240, 240)
        };
        painter.rect_filled(bar_shape, 9.0, bar_bg);
        let fill_color = row_color;
        if ratio > 0.0 {
            let fill_w = (bar_shape.width() * ratio).clamp(0.0, bar_shape.width());
            let fill_rect = Rect::from_min_max(
                bar_shape.min,
                Pos2::new(bar_shape.left() + fill_w, bar_shape.bottom()),
            );
            painter.rect_filled(fill_rect, 9.0, fill_color);
        }

        let percent_color = if ratio >= 0.50 {
            Color32::WHITE
        } else if dark_mode {
            Color32::from_rgb(210, 210, 210)
        } else {
            Color32::from_rgb(72, 72, 72)
        };
        painter.text(
            bar_shape.center(),
            Align2::CENTER_CENTER,
            &percent_text,
            FontId::monospace(13.0),
            percent_color,
        );

        painter
            .with_clip_rect(duration_rect)
            .text(
                Pos2::new(duration_rect.right(), duration_rect.center().y),
                Align2::RIGHT_CENTER,
                &duration_text,
                FontId::monospace(13.0),
                text_color,
            );
    }

    fn icon_texture_id(&mut self, ctx: &egui::Context, row: &SummaryRow) -> Option<egui::TextureId> {
        if row.is_idle {
            return None;
        }
        let app_key = normalize_app_key(&row.app_name);

        if let Some(process_path) = row.process_path.as_deref().filter(|value| !value.is_empty()) {
            self.ensure_icon_cached(process_path);
            if let Some(color) = self.icon_color_cache.get(process_path).copied() {
                self.app_color_cache.entry(app_key.clone()).or_insert(color);
            }

            if let Some(IconState::Loaded(texture)) = self.icon_cache.get(process_path) {
                return Some(texture.id());
            }
        }

        self.cached_icon_texture_id_for_app(ctx, &app_key)
    }

    fn cached_icon_texture_id_for_app(
        &mut self,
        ctx: &egui::Context,
        app_key: &str,
    ) -> Option<egui::TextureId> {
        let cache_key = format!("cache::app::{app_key}");
        if let Some(state) = self.icon_cache.get(&cache_key) {
            return match state {
                IconState::Loaded(texture) => Some(texture.id()),
                IconState::Pending | IconState::Missing => None,
            };
        }

        let Some(cached) = self.cached_app_visuals.get(app_key) else {
            return None;
        };
        let Some([width, height]) = cached.icon_size else {
            self.icon_cache.insert(cache_key, IconState::Missing);
            return None;
        };
        let Some(icon_rgba) = cached.icon_rgba.as_deref() else {
            self.icon_cache.insert(cache_key, IconState::Missing);
            return None;
        };
        let Some(image) = decode_cached_icon_image(width, height, icon_rgba) else {
            self.icon_cache.insert(cache_key, IconState::Missing);
            return None;
        };

        let texture = ctx.load_texture(
            format!("icon-cache:{app_key}"),
            image,
            egui::TextureOptions::LINEAR,
        );
        let texture_id = texture.id();
        self.icon_cache.insert(cache_key, IconState::Loaded(texture));
        Some(texture_id)
    }

    fn ensure_icon_cached(&mut self, process_path: &str) {
        if process_path.is_empty() {
            return;
        }
        if !matches!(
            self.icon_cache.get(process_path),
            None | Some(IconState::Pending)
        ) {
            return;
        }

        self.icon_cache
            .entry(process_path.to_owned())
            .or_insert(IconState::Pending);
        if self.icon_request_tx.send(process_path.to_owned()).is_err() {
            self.icon_cache
                .insert(process_path.to_owned(), IconState::Missing);
        }
    }

    fn refresh_app_color_cache(&mut self) {
        let mut unique_paths: HashSet<String> = HashSet::new();
        let mut app_to_path: HashMap<String, String> = HashMap::new();

        for seg in &self.segments {
            if seg.is_idle {
                continue;
            }
            let Some(path) = seg.process_path.as_deref() else {
                continue;
            };
            if path.is_empty() {
                continue;
            }
            let app_key = normalize_app_key(&seg.app_name);
            let path_owned = path.to_owned();
            unique_paths.insert(path_owned.clone());
            app_to_path.entry(app_key).or_insert(path_owned);
        }

        for process_path in unique_paths {
            self.ensure_icon_cached(&process_path);
        }

        for (app_key, process_path) in app_to_path {
            if let Some(color) = self.icon_color_cache.get(&process_path).copied() {
                self.app_color_cache.entry(app_key).or_insert(color);
            }
        }
    }

    fn drain_icon_results(&mut self, ctx: &egui::Context) {
        let mut has_update = false;
        let cache_conn = Connection::open(&self.db_path).ok();
        if let Some(conn) = cache_conn.as_ref() {
            let _ = conn.busy_timeout(Duration::from_millis(500));
            let _ = ensure_tracking_schema(conn);
        }

        while let Ok(result) = self.icon_result_rx.try_recv() {
            let IconLoadResult {
                process_path,
                image,
                dominant_color,
                display_name,
            } = result;
            let dominant_color = dominant_color.or_else(|| image.as_ref().and_then(dominant_color_from_icon));

            if let Some(color) = dominant_color {
                self.icon_color_cache.insert(process_path.clone(), color);
            }
            if let Some(display_name) = display_name.as_deref() {
                self.process_display_name_cache
                    .insert(process_path.clone(), display_name.to_owned());
            }

            if let Some(conn) = cache_conn.as_ref() {
                self.persist_app_visual_cache(
                    conn,
                    &process_path,
                    image.as_ref(),
                    dominant_color,
                    display_name.as_deref(),
                );
            }

            if let Some(image) = image {
                let texture = ctx.load_texture(
                    format!("icon:{}", process_path),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                self.icon_cache.insert(process_path, IconState::Loaded(texture));
            } else {
                self.icon_cache.insert(process_path, IconState::Missing);
            }
            has_update = true;
        }

        if has_update {
            self.refresh_app_color_cache();
            ctx.request_repaint();
        }
    }

    fn display_name_for_summary_row<'a>(&'a self, row: &'a SummaryRow) -> &'a str {
        if let Some(path) = row.process_path.as_deref() {
            if let Some(display_name) = self.process_display_name_cache.get(path) {
                let trimmed = display_name.trim();
                if !trimmed.is_empty() {
                    return trimmed;
                }
            }
        }
        let app_key = normalize_app_key(&row.app_name);
        if let Some(cached) = self.cached_app_visuals.get(&app_key) {
            if let Some(display_name) = cached.display_name.as_deref() {
                let trimmed = display_name.trim();
                if !trimmed.is_empty() {
                    return trimmed;
                }
            }
        }
        &row.display_name
    }

    fn app_keys_for_process_path(&self, process_path: &str) -> HashSet<String> {
        let mut keys = HashSet::new();
        for row in &self.summary_rows {
            if row
                .process_path
                .as_deref()
                .map(str::trim)
                .is_some_and(|path| path.eq_ignore_ascii_case(process_path))
            {
                keys.insert(normalize_app_key(&row.app_name));
            }
        }
        if keys.is_empty() {
            for seg in &self.segments {
                if seg
                    .process_path
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|path| path.eq_ignore_ascii_case(process_path))
                {
                    keys.insert(normalize_app_key(&seg.app_name));
                }
            }
        }
        keys
    }

    fn persist_app_visual_cache(
        &mut self,
        conn: &Connection,
        process_path: &str,
        image: Option<&egui::ColorImage>,
        dominant_color: Option<Color32>,
        display_name: Option<&str>,
    ) {
        let app_keys = self.app_keys_for_process_path(process_path);
        if app_keys.is_empty() {
            return;
        }

        let serialized_icon = image.map(encode_cached_icon_image);
        let display_name = display_name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned);

        for app_key in app_keys {
            let mut cached = self
                .cached_app_visuals
                .get(&app_key)
                .cloned()
                .unwrap_or(CachedAppVisual {
                    process_path: None,
                    color: color_for_app(false, &app_key),
                    icon_size: None,
                    icon_rgba: None,
                    display_name: None,
                });

            cached.process_path = Some(process_path.to_owned());
            if let Some(color) = dominant_color {
                cached.color = color;
                self.app_color_cache.insert(app_key.clone(), color);
            }
            if let Some((icon_size, icon_rgba)) = serialized_icon.as_ref() {
                cached.icon_size = Some(*icon_size);
                cached.icon_rgba = Some(icon_rgba.clone());
            }
            if let Some(display_name) = display_name.as_ref() {
                cached.display_name = Some(display_name.clone());
            }

            if upsert_cached_app_visual(conn, &app_key, &cached).is_ok() {
                self.cached_app_visuals.insert(app_key.clone(), cached);
            }
        }
    }

    fn display_color_for(
        &self,
        is_idle: bool,
        app_name: &str,
        process_path: Option<&str>,
    ) -> Color32 {
        display_color_from_maps(
            &self.icon_color_cache,
            &self.app_color_cache,
            is_idle,
            app_name,
            process_path,
        )
    }

    fn refresh_backend_status(&mut self) {
        if self
            .backend_status_request_tx
            .send(BackendStatusWorkerRequest::ProbeNow)
            .is_err()
        {
            self.backend_status = BackendStatus {
                health: BackendHealth::Stopped,
                last_write_ts: None,
                checked_ts: unix_seconds_now(),
                detail: None,
            };
        }
    }

    fn drain_backend_status_results(&mut self) {
        let mut latest_status: Option<BackendStatus> = None;
        while let Ok(status) = self.backend_status_result_rx.try_recv() {
            latest_status = Some(status);
        }
        if let Some(status) = latest_status {
            self.backend_status = status;
        }
    }

    fn data_root_dir(&self) -> PathBuf {
        self.db_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn output_root_dir(&self) -> PathBuf {
        self.save_dir_override
            .clone()
            .unwrap_or_else(|| self.data_root_dir())
    }

    fn export_output_path(&self, extension: &str) -> Result<PathBuf> {
        let export_dir = self.output_root_dir().join("exports");
        fs::create_dir_all(&export_dir)
            .with_context(|| format!("failed to create export directory: {}", export_dir.display()))?;

        let filename = format!(
            "export_{}_{}.{}",
            Local::now().format("%Y%m%d_%H%M%S"),
            self.current_range_tag(),
            extension
        );
        Ok(export_dir.join(filename))
    }

    fn parse_import_file_path(&self) -> Result<PathBuf> {
        let trimmed = self.import_file_input.trim();
        if trimmed.is_empty() {
            bail!("CSV file path cannot be empty");
        }

        let csv_path = PathBuf::from(trimmed);
        if !csv_path.exists() {
            bail!("CSV file does not exist: {}", csv_path.display());
        }
        if !csv_path.is_file() {
            bail!("CSV path is not a file: {}", csv_path.display());
        }
        Ok(csv_path)
    }

    fn import_csv_file(&self, csv_path: &PathBuf) -> Result<ImportStats> {
        let mut conn = Connection::open(&self.db_path)
            .with_context(|| format!("failed to open database: {}", self.db_path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))
            .context("failed to set busy timeout")?;
        ensure_tracking_schema(&conn)?;

        let mut reader = ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_path(csv_path)
            .with_context(|| format!("failed to open CSV file: {}", csv_path.display()))?;

        let headers = reader
            .headers()
            .with_context(|| format!("failed to read CSV headers: {}", csv_path.display()))?
            .clone();
        let columns = ImportCsvColumns::from_headers(&headers)?;

        let tx = conn
            .transaction()
            .context("failed to open import transaction")?;

        let mut app_cache: HashMap<(String, String), i64> = HashMap::new();
        let mut title_cache: HashMap<String, i64> = HashMap::new();
        let mut stats = ImportStats::default();

        for (row_idx, row_result) in reader.records().enumerate() {
            stats.total_rows += 1;
            let row = match row_result {
                Ok(row) => row,
                Err(err) => {
                    stats.skipped_rows += 1;
                    eprintln!("CSV row {} parse error: {err}", row_idx + 2);
                    continue;
                }
            };

            let parsed = match parse_import_csv_row(&row, &columns) {
                Some(parsed) => parsed,
                None => {
                    stats.skipped_rows += 1;
                    continue;
                }
            };

            let app_id = upsert_app_in_tx(
                &tx,
                &mut app_cache,
                &parsed.app_name,
                &parsed.process_path,
            )?;
            let title_id = if let Some(title) = parsed.title.as_deref() {
                Some(upsert_title_in_tx(&tx, &mut title_cache, title)?)
            } else {
                None
            };

            tx.execute(
                "\
                INSERT INTO segments (
                  start_ts,
                  end_ts,
                  app_id,
                  title_id,
                  is_idle,
                  pid,
                  pid_create_time
                )
                VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL)",
                params![
                    parsed.start_ts,
                    parsed.end_ts,
                    app_id,
                    title_id,
                    if parsed.is_idle { 1_i64 } else { 0_i64 },
                ],
            )
            .context("failed to insert imported segment")?;
            stats.imported_rows += 1;
        }

        tx.commit()
            .context("failed to commit CSV import transaction")?;

        if stats.total_rows == 0 {
            bail!("CSV has no data rows");
        }
        if stats.imported_rows == 0 {
            bail!("CSV contains no valid rows");
        }

        Ok(stats)
    }

    fn apply_custom_save_dir(&mut self) -> Result<PathBuf> {
        let trimmed = self.save_dir_input.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("save path cannot be empty"));
        }
        let dir = PathBuf::from(trimmed);
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create save directory: {}", dir.display()))?;
        self.save_dir_override = Some(dir.clone());
        self.save_dir_input = dir.display().to_string();
        Ok(dir)
    }

    fn apply_custom_save_dir_or_report_error(&mut self) -> bool {
        if let Err(err) = self.apply_custom_save_dir() {
            self.clear_info_message();
            self.error = Some(format!("save path invalid: {err:#}"));
            return false;
        }
        true
    }

    fn current_range_tag(&self) -> String {
        if let Some((start, end)) = self.custom_range {
            return format!(
                "custom_{}_{}",
                start.format("%Y%m%d"),
                end.format("%Y%m%d")
            );
        }
        if let Some(preset) = self.range_preset {
            return preset
                .short_label()
                .to_ascii_lowercase()
                .replace(' ', "_");
        }
        "single_day".to_owned()
    }

    fn collect_export_rows_for_active_range(&self) -> Vec<ExportSegmentRow> {
        let Some((range_start, range_end)) = self.active_range_bounds() else {
            return Vec::new();
        };

        let mut rows = Vec::new();
        for seg in &self.segments {
            let start = seg.start_ts.max(range_start);
            let end = seg.end_ts.min(range_end);
            if end <= start {
                continue;
            }
            rows.push(ExportSegmentRow {
                start_ts: start,
                end_ts: end,
                is_idle: seg.is_idle,
                app_name: seg.app_name.clone(),
                process_path: seg.process_path.clone(),
                title: seg.title.clone(),
            });
        }
        rows
    }

    fn export_current_range_csv(&self) -> Result<PathBuf> {
        let output_path = self.export_output_path("csv")?;

        let file = File::create(&output_path)
            .with_context(|| format!("failed to create export file: {}", output_path.display()))?;
        let mut writer = BufWriter::new(file);
        writeln!(writer, "\"Title\",\"Start\",\"End\",\"Duration\",\"Process\"")
            .context("failed to write CSV header")?;

        let mut process_name_lookup_cache: HashMap<String, String> = HashMap::new();

        for row in self.collect_export_rows_for_active_range() {
            let start_text = format_local_datetime(row.start_ts);
            let end_text = format_local_datetime(row.end_ts);
            let duration_text = format_duration(row.end_ts.saturating_sub(row.start_ts));
            let title_text = row
                .title
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| display_app_name(&row.app_name, row.is_idle));
            let process_text = resolve_export_process_name(
                row.is_idle,
                &row.app_name,
                row.process_path.as_deref(),
                &self.process_display_name_cache,
                &mut process_name_lookup_cache,
            );

            writeln!(
                writer,
                "{},{},{},{},{}",
                csv_escape(&title_text),
                csv_escape(&start_text),
                csv_escape(&end_text),
                csv_escape(&duration_text),
                csv_escape(&process_text),
            )
            .context("failed to write CSV row")?;
        }
        writer.flush().context("failed to flush CSV writer")?;
        Ok(output_path)
    }

    fn export_current_range_json(&self) -> Result<PathBuf> {
        let output_path = self.export_output_path("json")?;

        let rows = self.collect_export_rows_for_active_range();
        let items: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|row| {
                let duration_secs = row.end_ts.saturating_sub(row.start_ts);
                json!({
                    "start_ts": row.start_ts,
                    "end_ts": row.end_ts,
                    "start_local": format_local_datetime(row.start_ts),
                    "end_local": format_local_datetime(row.end_ts),
                    "duration_secs": duration_secs,
                    "is_idle": row.is_idle,
                    "app_name": row.app_name,
                    "process_path": row.process_path,
                    "title": row.title
                })
            })
            .collect();

        let file = File::create(&output_path)
            .with_context(|| format!("failed to create export file: {}", output_path.display()))?;
        let writer = BufWriter::new(file);
        serde_json::to_writer_pretty(writer, &items).context("failed to write JSON export")?;
        Ok(output_path)
    }

    fn backup_database(&self) -> Result<PathBuf> {
        let backup_dir = self.output_root_dir().join("backups");
        fs::create_dir_all(&backup_dir)
            .with_context(|| format!("failed to create backup directory: {}", backup_dir.display()))?;

        let filename = format!("tracker_{}.db", Local::now().format("%Y%m%d_%H%M%S"));
        let output_path = backup_dir.join(filename);

        let source = Connection::open(&self.db_path)
            .with_context(|| format!("failed to open source database: {}", self.db_path.display()))?;
        let mut destination = Connection::open(&output_path)
            .with_context(|| format!("failed to create backup file: {}", output_path.display()))?;

        let backup = Backup::new(&source, &mut destination).context("failed to initialize SQLite backup")?;
        backup
            .run_to_completion(128, Duration::from_millis(20), None)
            .context("failed to complete SQLite backup")?;

        Ok(output_path)
    }

    fn draw_export_window_content(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(320.0);
        ui.horizontal(|ui| {
            ui.label(format!("{}:", self.t("format")));
            ui.selectable_value(&mut self.export_format, ExportFormat::Csv, "CSV");
            ui.selectable_value(&mut self.export_format, ExportFormat::Json, "JSON");
        });
        if self.range_preset == Some(RangePreset::All) {
            let data_line = match self.ui_language {
                UiLanguage::ZhCn => "\u{6570}\u{636E}: \u{5168}\u{90E8}\u{65E5}\u{671F}".to_owned(),
                UiLanguage::EnUs => "Data: all dates".to_owned(),
            };
            ui.label(data_line);
        } else if let Some((start, end)) = self.active_range_dates() {
            let data_line = match self.ui_language {
                UiLanguage::ZhCn => format!("\u{6570}\u{636E}: {} ~ {}\u{FF08}\u{5F53}\u{524D}\u{8303}\u{56F4}\u{FF09}", start.format("%Y-%m-%d"), end.format("%Y-%m-%d")),
                UiLanguage::EnUs => format!(
                    "Data: {} ~ {} (Current Range)",
                    start.format("%Y-%m-%d"),
                    end.format("%Y-%m-%d")
                ),
            };
            ui.label(data_line);
        }

        if self.draw_save_path_action_row(ui, "export") {
            if !self.apply_custom_save_dir_or_report_error() {
                return;
            }
            let save_result = match self.export_format {
                ExportFormat::Csv => self
                    .export_current_range_csv()
                    .map(|path| ("CSV", path))
                    .map_err(|err| format!("CSV export failed: {err:#}")),
                ExportFormat::Json => self
                    .export_current_range_json()
                    .map(|path| ("JSON", path))
                    .map_err(|err| format!("JSON export failed: {err:#}")),
            };

            match save_result {
                Ok((kind, path)) => {
                    self.set_info_message(format!("{kind} saved: {}", path.display()));
                    eprintln!("{kind} export saved: {}", path.display());
                }
                Err(err) => {
                    self.clear_info_message();
                    self.error = Some(err);
                }
            }
        }
    }

    fn draw_backup_window_content(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(320.0);
        ui.label(match self.ui_language {
            UiLanguage::ZhCn => "\u{6570}\u{636E}: \u{5168}\u{91CF}\u{6570}\u{636E}\u{5E93}\u{FF08}\u{5168}\u{90E8}\u{65E5}\u{671F}\u{FF09}".to_owned(),
            UiLanguage::EnUs => "Data: full database (all dates)".to_owned(),
        });
        if self.draw_save_path_action_row(ui, "backup") {
            if !self.apply_custom_save_dir_or_report_error() {
                return;
            }

            match self.backup_database() {
                Ok(path) => {
                    self.set_info_message(format!("Backup saved: {}", path.display()));
                    eprintln!("Database backup saved: {}", path.display());
                }
                Err(err) => {
                    self.clear_info_message();
                    self.error = Some(format!("database backup failed: {err:#}"));
                }
            }
        }
    }

    fn draw_import_window_content(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(320.0);
        ui.label(match self.ui_language {
            UiLanguage::ZhCn => "\u{6570}\u{636E}: CSV \u{6587}\u{4EF6}",
            UiLanguage::EnUs => "Data: CSV file",
        });

        let mut clicked = false;
        ui.horizontal(|ui| {
            ui.label(format!("{}:", self.t("path")));
            let hint = match self.ui_language {
                UiLanguage::ZhCn => "CSV \u{6587}\u{4EF6}\u{8DEF}\u{5F84}",
                UiLanguage::EnUs => "CSV file path",
            };
            let path_width = (ui.available_width() - 64.0).max(140.0);
            ui.add_sized(
                [path_width, 22.0],
                egui::TextEdit::singleline(&mut self.import_file_input).hint_text(hint),
            );
            clicked = ui.button(self.t("import")).clicked();
        });

        if !clicked {
            return;
        }

        let csv_path = match self.parse_import_file_path() {
            Ok(path) => path,
            Err(err) => {
                self.clear_info_message();
                self.error = Some(format!("CSV import failed: {err:#}"));
                return;
            }
        };

        match self.import_csv_file(&csv_path) {
            Ok(stats) => {
                let message = match self.ui_language {
                    UiLanguage::ZhCn => format!(
                        "\u{5BFC}\u{5165}\u{5B8C}\u{6210}\u{FF1A}\u{6210}\u{529F} {} \u{6761}\u{FF0C}\u{8DF3}\u{8FC7} {} \u{6761}",
                        stats.imported_rows, stats.skipped_rows
                    ),
                    UiLanguage::EnUs => format!(
                        "Import completed: {} rows imported, {} rows skipped",
                        stats.imported_rows, stats.skipped_rows
                    ),
                };
                self.set_info_message(message);
                self.reload();
                self.refresh_backend_status();
                eprintln!(
                    "CSV import completed: {} rows imported, {} rows skipped ({})",
                    stats.imported_rows,
                    stats.skipped_rows,
                    csv_path.display()
                );
            }
            Err(err) => {
                self.clear_info_message();
                self.error = Some(format!("CSV import failed: {err:#}"));
            }
        }
    }

    fn draw_backend_status_indicator(&self, ui: &mut egui::Ui) {
        let text = match (self.ui_language, self.backend_status.health) {
            (UiLanguage::ZhCn, BackendHealth::Running) => "\u{670D}\u{52A1}\u{8FD0}\u{884C}\u{4E2D}".to_owned(),
            (UiLanguage::EnUs, BackendHealth::Running) => "Tracking Active".to_owned(),
            (UiLanguage::ZhCn, BackendHealth::Stopped) => {
                "\u{670D}\u{52A1}\u{672A}\u{8FD0}\u{884C}".to_owned()
            }
            (UiLanguage::EnUs, BackendHealth::Stopped) => "Service Not Running".to_owned(),
        };
        let response = ui.label(egui::RichText::new(text).strong().color(self.backend_status.color()));
        let status = &self.backend_status;
        response.on_hover_ui(|ui| {
            ui.label(format!(
                "{}: {}",
                self.t("status"),
                status.short_label_lang(self.ui_language)
            ));
            ui.label(format!("{}: {}", self.t("checked"), format_hms(status.checked_ts)));
            if let Some(last_write_ts) = status.last_write_ts {
                ui.label(format!("{}: {}", self.t("last_write"), format_hms(last_write_ts)));
            } else {
                ui.label(format!("{}: --", self.t("last_write")));
            }
            if let Some(detail) = &status.detail {
                ui.separator();
                ui.label(detail);
            }
        });
    }
}

impl eframe::App for TimelineApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(expires_at) = self.info_expires_at {
            if Instant::now() >= expires_at {
                self.clear_info_message();
            } else if let Some(remaining) = expires_at.checked_duration_since(Instant::now()) {
                ctx.request_repaint_after(remaining);
            }
        }

        ctx.request_repaint_after(BACKEND_STATUS_POLL_INTERVAL);
        self.drain_reload_results();
        self.drain_backend_status_results();
        if self.last_auto_refresh.elapsed() >= AUTO_REFRESH_INTERVAL
            && self.pending_reload_request_id.is_none()
        {
            self.reload();
        }

        self.drain_icon_results(ctx);
        if self.pending_icon_refresh {
            self.refresh_app_color_cache();
            self.pending_icon_refresh = false;
        }
        let active_range = self.active_range_bounds();

        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let is_single_day_mode = self.range_preset.is_none() && self.custom_range.is_none();
                let is_all_range_mode = self.range_preset == Some(RangePreset::All);
                ui.label(self.t("range"));
                let range_label = if self.custom_range.is_some() {
                    self.t("custom").to_owned()
                } else {
                    self.range_preset
                        .map(|preset| preset.ui_label(self.ui_language).to_owned())
                        .unwrap_or_else(|| self.t("single_day").to_owned())
                };
                let range_button = ui.button(range_label);
                let range_popup_id = ui.make_persistent_id("range_popup");
                if range_button.clicked() {
                    ui.memory_mut(|mem| mem.toggle_popup(range_popup_id));
                }
                egui::popup::popup_below_widget(
                    ui,
                    range_popup_id,
                    &range_button,
                    egui::popup::PopupCloseBehavior::CloseOnClickOutside,
                    |ui| self.draw_range_picker(ui),
                );

                if is_single_day_mode {
                    ui.separator();
                    if ui.button("<").clicked() {
                        self.shift_day(-1);
                    }
                    if ui.button(self.t("today")).clicked() {
                        let today = Local::now().date_naive();
                        self.set_selected_date(today);
                    }
                    if ui.button(">").clicked() {
                        self.shift_day(1);
                    }

                    ui.separator();
                    ui.label(self.t("date"));
                    let date_button = ui.button(self.selected_date.format("%Y-%m-%d").to_string());
                    let date_popup_id = ui.make_persistent_id("date_popup");
                    if date_button.clicked() {
                        ui.memory_mut(|mem| mem.toggle_popup(date_popup_id));
                    }
                    egui::popup::popup_below_widget(
                        ui,
                        date_popup_id,
                        &date_button,
                        egui::popup::PopupCloseBehavior::CloseOnClickOutside,
                        |ui| self.draw_date_picker(ui),
                    );
                }

                if !is_single_day_mode && !is_all_range_mode {
                    let (range_start_date, range_end_date) = self
                        .active_range_dates()
                        .unwrap_or((self.selected_date, self.selected_date));
                    let from_label = range_start_date.format("%Y-%m-%d").to_string();
                    let to_label = range_end_date.format("%Y-%m-%d").to_string();
                    ui.separator();
                    ui.label(self.t("from"));
                    let from_button = ui.button(from_label);
                    let from_popup_id = ui.make_persistent_id("from_popup");
                    if from_button.clicked() {
                        self.calendar_month = month_start(range_start_date);
                        if self.custom_range_focus != CustomRangeFocus::From {
                            self.custom_range_focus = CustomRangeFocus::From;
                        }
                        ui.memory_mut(|mem| mem.toggle_popup(from_popup_id));
                    }
                    egui::popup::popup_below_widget(
                        ui,
                        from_popup_id,
                        &from_button,
                        egui::popup::PopupCloseBehavior::CloseOnClickOutside,
                        |ui| self.draw_custom_range_picker(ui),
                    );
                    ui.label(self.t("to"));
                    let to_button = ui.button(to_label);
                    let to_popup_id = ui.make_persistent_id("to_popup");
                    if to_button.clicked() {
                        self.calendar_month = month_start(range_end_date);
                        if self.custom_range_focus != CustomRangeFocus::To {
                            self.custom_range_focus = CustomRangeFocus::To;
                        }
                        ui.memory_mut(|mem| mem.toggle_popup(to_popup_id));
                    }
                    egui::popup::popup_below_widget(
                        ui,
                        to_popup_id,
                        &to_button,
                        egui::popup::PopupCloseBehavior::CloseOnClickOutside,
                        |ui| self.draw_custom_range_picker(ui),
                    );
                }

                ui.separator();
                ui.label(items_caption(self.ui_language));
                let previous_summary_limit = self.summary_limit;
                let summary_limit_selected_text = self
                    .summary_limit
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| self.summary_limit_all_label().to_owned());
                egui::ComboBox::from_id_salt("summary_limit")
                    .selected_text(summary_limit_selected_text)
                    .width(68.0)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show_ui(ui, |ui| {
                        let all_selected = self.summary_limit.is_none();
                        if ui
                            .selectable_label(all_selected, self.summary_limit_all_label())
                            .clicked()
                        {
                            self.summary_limit = None;
                            self.summary_limit_custom_input.clear();
                            ui.close_menu();
                        }

                        for limit in [5_usize, 10, 15, 20, 25, 30] {
                            let selected = self.summary_limit == Some(limit);
                            if ui.selectable_label(selected, format!("{limit}")).clicked() {
                                self.summary_limit = Some(limit);
                                self.summary_limit_custom_input = limit.to_string();
                                ui.close_menu();
                            }
                        }

                        ui.separator();
                        let hint = match self.ui_language {
                            UiLanguage::ZhCn => "\u{6570}\u{91CF}",
                            UiLanguage::EnUs => "count",
                        };
                        let response = ui.add_sized(
                            [52.0, 22.0],
                            egui::TextEdit::singleline(&mut self.summary_limit_custom_input)
                                .hint_text(hint),
                        );
                        if response.changed() {
                            self.summary_limit_custom_input
                                .retain(|ch| ch.is_ascii_digit());
                            if let Ok(value) = self.summary_limit_custom_input.parse::<usize>() {
                                if value > 0 {
                                    self.summary_limit = Some(value);
                                }
                            }
                        }
                    });
                self.apply_summary_limit_change(previous_summary_limit);
                ui.separator();
                if ui.button(self.t("refresh")).clicked() {
                    self.reload();
                    self.refresh_backend_status();
                }
                ui.separator();

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut selected_language = self.ui_language;
                    egui::ComboBox::from_id_salt("ui_language")
                        .selected_text(selected_language.compact_label())
                        .width(60.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut selected_language,
                                UiLanguage::ZhCn,
                                UiLanguage::ZhCn.compact_label(),
                            );
                            ui.selectable_value(
                                &mut selected_language,
                                UiLanguage::EnUs,
                                UiLanguage::EnUs.compact_label(),
                            );
                        });
                    if selected_language != self.ui_language {
                        self.set_ui_language(selected_language);
                    }
                    ui.label("\u{1F310}");
                    ui.separator();
                    self.draw_backend_status_indicator(ui);
                });
            });
            ui.add_space(1.0);

            if let Some(err) = &self.error {
                ui.colored_label(Color32::from_rgb(180, 30, 30), err);
            } else if let Some(info) = self.info.clone() {
                ui.horizontal(|ui| {
                    ui.colored_label(Color32::from_rgb(24, 120, 56), info);
                    if ui.small_button("x").on_hover_text("Dismiss").clicked() {
                        self.clear_info_message();
                    }
                });
            }
        });

        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), Sense::hover());
            let content_rect = rect.shrink2(egui::vec2(6.0, 3.0));
            let text_color = ui.visuals().text_color();

            let total_x = (content_rect.right() - SCROLLBAR_SAFE_GUTTER).max(content_rect.left());
            let left_max_x = (total_x - 130.0).max(content_rect.left() + 120.0);
            let actions_left = (content_rect.left() - 6.0).max(rect.left() + 1.0);
            let actions_rect = Rect::from_min_max(
                Pos2::new(actions_left, content_rect.top()),
                Pos2::new(left_max_x, content_rect.bottom()),
            );
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(actions_rect), |ui| {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let import_button = ui.button(self.t("import"));
                    if import_button.clicked() {
                        self.show_import_window = !self.show_import_window;
                    }
                    let export_button = ui.button(self.t("export"));
                    if export_button.clicked() {
                        self.show_export_window = !self.show_export_window;
                    }
                    let backup_button = ui.button(self.t("backup"));
                    if backup_button.clicked() {
                        self.show_backup_window = !self.show_backup_window;
                    }
                });
            });

            let painter = ui.painter();
            painter.text(
                Pos2::new(total_x, content_rect.center().y),
                Align2::RIGHT_CENTER,
                format!("{}: {}", self.t("total"), format_duration(self.summary_total_secs)),
                FontId::monospace(13.0),
                text_color,
            );

            let help_rect = Rect::from_center_size(
                Pos2::new(content_rect.right() - 5.0, content_rect.center().y),
                egui::vec2(SCROLLBAR_SAFE_GUTTER + 6.0, content_rect.height()),
            );
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(help_rect), |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().button_padding = egui::vec2(2.0, 0.0);
                    let help_button = ui.button("?");
                    let help_popup_id = ui.make_persistent_id("help_popup");
                    if help_button.clicked() {
                        ui.memory_mut(|mem| mem.toggle_popup(help_popup_id));
                    }
                    egui::popup::popup_above_or_below_widget(
                        ui,
                        help_popup_id,
                        &help_button,
                        egui::AboveOrBelow::Above,
                        egui::popup::PopupCloseBehavior::CloseOnClickOutside,
                        |ui| draw_help_menu_content(ui, self.ui_language),
                    );
                });
            });
        });

        if self.show_import_window {
            let mut open = self.show_import_window;
            let import_title = self.t("import");
            self.show_centered_window(
                ctx,
                "import_window",
                import_title,
                &mut open,
                egui::vec2(380.0, 170.0),
                |app, ui| app.draw_import_window_content(ui),
            );
            self.show_import_window = open;
        }

        if self.show_export_window {
            let mut open = self.show_export_window;
            let export_title = self.t("export");
            self.show_centered_window(
                ctx,
                "export_window",
                export_title,
                &mut open,
                egui::vec2(380.0, 210.0),
                |app, ui| app.draw_export_window_content(ui),
            );
            self.show_export_window = open;
        }

        if self.show_backup_window {
            let mut open = self.show_backup_window;
            let backup_title = self.t("backup");
            self.show_centered_window(
                ctx,
                "backup_window",
                backup_title,
                &mut open,
                egui::vec2(380.0, 170.0),
                |app, ui| app.draw_backup_window_content(ui),
            );
            self.show_backup_window = open;
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some((range_start, range_end)) = active_range else {
                ui.colored_label(
                    Color32::from_rgb(180, 30, 30),
                    "failed to resolve active range",
                );
                return;
            };

            if self.range_preset.is_none() && self.custom_range.is_none() {
                ui.heading(self.t("timeline"));
                ui.add_space(8.0);
                let timeline_segments = self.ensure_timeline_cache(range_start, range_end);
                let view_range = &mut self.timeline_view_range;
                let icon_colors = &self.icon_color_cache;
                let app_colors = &self.app_color_cache;
                draw_timeline(
                    ui,
                    range_start,
                    range_end,
                    timeline_segments.as_slice(),
                    view_range,
                    icon_colors,
                    app_colors,
                    self.summary_rows.as_slice(),
                    &self.process_display_name_cache,
                    self.ui_language,
                );
                ui.add_space(8.0);
            } else {
                self.timeline_view_range = None;
            }

            draw_section_header(ui, self.t("top_apps"));
            ui.add_space(6.0);

            egui::ScrollArea::vertical()
                .id_salt("top_apps_scroll")
                .auto_shrink([false, false])
                .max_height(ui.available_height().max(0.0))
                .show(ui, |ui| {
                    self.draw_summary_rows(ctx, ui);
                });
        });
    }
}

fn spawn_icon_loader() -> (mpsc::Sender<String>, mpsc::Receiver<IconLoadResult>) {
    let (request_tx, request_rx) = mpsc::channel::<String>();
    let (result_tx, result_rx) = mpsc::channel::<IconLoadResult>();

    std::thread::spawn(move || {
        while let Ok(process_path) = request_rx.recv() {
            let image = load_app_icon_image(&process_path);
            let dominant_color = image.as_ref().and_then(dominant_color_from_icon);
            let display_name = load_app_file_description(&process_path);
            let _ = result_tx.send(IconLoadResult {
                process_path,
                image,
                dominant_color,
                display_name,
            });
        }
    });

    (request_tx, result_rx)
}

fn spawn_reload_worker() -> (mpsc::Sender<ReloadRequest>, mpsc::Receiver<ReloadResult>) {
    let (request_tx, request_rx) = mpsc::channel::<ReloadRequest>();
    let (result_tx, result_rx) = mpsc::channel::<ReloadResult>();

    std::thread::spawn(move || {
        while let Ok(mut request) = request_rx.recv() {
            while let Ok(next_request) = request_rx.try_recv() {
                request = next_request;
            }

            let payload: std::result::Result<ReloadPayload, String> =
                match load_segments_for_range(&request.db_path, request.range_start, request.range_end) {
                    Ok(segments) => {
                        let summary_rows =
                            build_summary_rows(request.range_start, request.range_end, &segments);
                        let summary_total_secs = summary_rows
                            .iter()
                            .map(|row| row.duration_secs.max(0))
                            .sum();
                        Ok(ReloadPayload {
                            segments,
                            summary_rows,
                            summary_total_secs,
                        })
                    }
                    Err(err) => Err(format!("failed to load segments: {err:#}")),
                };

            if result_tx
                .send(ReloadResult {
                    request_id: request.request_id,
                    payload,
                })
                .is_err()
            {
                break;
            }
        }
    });

    (request_tx, result_rx)
}

fn spawn_backend_status_worker(
    db_path: PathBuf,
) -> (
    mpsc::Sender<BackendStatusWorkerRequest>,
    mpsc::Receiver<BackendStatus>,
) {
    let (request_tx, request_rx) = mpsc::channel::<BackendStatusWorkerRequest>();
    let (result_tx, result_rx) = mpsc::channel::<BackendStatus>();

    std::thread::spawn(move || {
        loop {
            match request_rx.recv_timeout(BACKEND_STATUS_POLL_INTERVAL) {
                Ok(BackendStatusWorkerRequest::ProbeNow)
                | Err(mpsc::RecvTimeoutError::Timeout) => {
                    while request_rx.try_recv().is_ok() {}
                    let status = match probe_backend_status(&db_path) {
                        Ok(status) => status,
                        Err(_err) => BackendStatus {
                            health: BackendHealth::Stopped,
                            last_write_ts: None,
                            checked_ts: unix_seconds_now(),
                            detail: None,
                        },
                    };
                    if result_tx.send(status).is_err() {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    (request_tx, result_rx)
}

fn draw_section_header(ui: &mut egui::Ui, title: &str) {
    ui.horizontal(|ui| {
        ui.heading(title);
        ui.add_space(10.0);

        let line_width = ui.available_width().max(0.0);
        if line_width <= 0.0 {
            return;
        }

        let line_height = ui.text_style_height(&egui::TextStyle::Heading).max(18.0);
        let (line_rect, _) =
            ui.allocate_exact_size(egui::vec2(line_width, line_height), Sense::hover());
        let y = line_rect.center().y + 1.0;
        let line_color = if ui.visuals().dark_mode {
            Color32::from_rgb(76, 76, 76)
        } else {
            Color32::from_rgb(206, 206, 201)
        };
        ui.painter().line_segment(
            [Pos2::new(line_rect.left(), y), Pos2::new(line_rect.right(), y)],
            Stroke::new(1.0, line_color),
        );
    });
}

fn draw_help_menu_content(ui: &mut egui::Ui, language: UiLanguage) {
    ui.set_min_width(240.0);
    ui.label(tr(language, "help.timeline"));
    ui.label(tr(language, "help.zoom"));
    ui.label(tr(language, "help.pan"));
    ui.label(tr(language, "help.reset"));
}

fn build_summary_rows(range_start: i64, range_end: i64, segments: &[Segment]) -> Vec<SummaryRow> {
    if range_end <= range_start {
        return Vec::new();
    }

    let mut totals: HashMap<String, SummaryRow> = HashMap::new();
    let mut display_name_by_path: HashMap<String, Option<String>> = HashMap::new();
    for seg in segments {
        if should_hide_summary_app(&seg.app_name, seg.is_idle, seg.process_path.as_deref()) {
            continue;
        }

        let clipped_start = seg.start_ts.max(range_start);
        let clipped_end = seg.end_ts.min(range_end);
        if clipped_end <= clipped_start {
            continue;
        }

        let duration = clipped_end - clipped_start;
        let display_name = resolve_summary_display_name(seg, &mut display_name_by_path);
        let key = normalize_summary_group_key(&display_name);

        let entry = totals.entry(key).or_insert_with(|| SummaryRow {
            app_name: seg.app_name.clone(),
            display_name,
            duration_secs: 0,
            process_path: seg.process_path.clone(),
            is_idle: seg.is_idle,
        });

        entry.duration_secs += duration;
        if should_prefer_process_path(entry.process_path.as_deref(), seg.process_path.as_deref()) {
            entry.process_path = seg.process_path.clone();
            entry.app_name = seg.app_name.clone();
            entry.is_idle = seg.is_idle;
        }
    }

    let mut rows: Vec<SummaryRow> = totals.into_values().collect();
    rows.sort_by(|a, b| {
        b.duration_secs
            .cmp(&a.duration_secs)
            .then_with(|| a.display_name.cmp(&b.display_name))
            .then_with(|| a.app_name.cmp(&b.app_name))
            .then_with(|| {
                a.process_path
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.process_path.as_deref().unwrap_or(""))
            })
            .then_with(|| a.is_idle.cmp(&b.is_idle))
    });
    rows
}

fn resolve_summary_display_name(
    seg: &Segment,
    display_name_by_path: &mut HashMap<String, Option<String>>,
) -> String {
    if let Some(path) = seg
        .process_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|path| !is_synthetic_import_path(path))
    {
        let cached = display_name_by_path
            .entry(path.to_owned())
            .or_insert_with(|| load_app_file_description(path));
        if let Some(name) = cached.as_deref() {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
    }

    display_app_name(&seg.app_name, seg.is_idle)
}

fn normalize_summary_group_key(display_name: &str) -> String {
    let mut key = String::with_capacity(display_name.len());
    let mut prev_is_space = false;
    for ch in display_name.trim().chars() {
        if ch.is_whitespace() {
            if !prev_is_space {
                key.push(' ');
                prev_is_space = true;
            }
            continue;
        }
        prev_is_space = false;
        key.push(ch.to_ascii_lowercase());
    }
    key
}

fn should_prefer_process_path(current: Option<&str>, incoming: Option<&str>) -> bool {
    let incoming = incoming
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let current = current
        .map(str::trim)
        .filter(|value| !value.is_empty());

    match (current, incoming) {
        (None, Some(_)) => true,
        (Some(cur), Some(next)) => is_synthetic_import_path(cur) && !is_synthetic_import_path(next),
        _ => false,
    }
}

fn is_synthetic_import_path(path: &str) -> bool {
    let path = path.trim();
    path.starts_with("<import:") && path.ends_with('>')
}

fn draw_timeline(
    ui: &mut egui::Ui,
    range_start: i64,
    range_end: i64,
    timeline_segments: &[TimelineRenderSegment],
    view_range: &mut Option<(i64, i64)>,
    icon_colors: &HashMap<String, Color32>,
    app_colors: &HashMap<String, Color32>,
    summary_rows: &[SummaryRow],
    process_display_name_cache: &HashMap<String, String>,
    language: UiLanguage,
) {
    if range_end <= range_start {
        ui.colored_label(Color32::from_rgb(180, 30, 30), "unable to resolve active range");
        return;
    }
    let dark_mode = ui.visuals().dark_mode;
    let panel_bg = if dark_mode {
        Color32::from_rgb(32, 34, 36)
    } else {
        Color32::from_rgb(247, 247, 244)
    };
    let chart_bg = if dark_mode {
        Color32::from_rgb(28, 30, 32)
    } else {
        Color32::from_rgb(247, 247, 244)
    };
    let chart_border = if dark_mode {
        Color32::from_rgb(86, 86, 86)
    } else {
        Color32::from_rgb(210, 210, 205)
    };
    let label_color = if dark_mode {
        Color32::from_rgb(190, 190, 190)
    } else {
        Color32::from_rgb(92, 92, 86)
    };
    let major_grid = if dark_mode {
        Color32::from_rgb(96, 96, 96)
    } else {
        Color32::from_rgb(170, 170, 165)
    };
    let minor_grid = if dark_mode {
        Color32::from_rgb(62, 62, 62)
    } else {
        Color32::from_rgb(218, 218, 213)
    };

    let day_span_secs = (range_end - range_start).max(1);
    let (mut view_start, mut view_end) = sanitize_view_range(*view_range, range_start, range_end);

    let available_width = ui.available_width().max(120.0);
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(available_width, TIMELINE_TOTAL_HEIGHT),
        Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, panel_bg);

    let chart_rect = Rect::from_min_max(
        Pos2::new(rect.left() + 4.0, rect.top() + TIMELINE_HEADER_HEIGHT),
        Pos2::new(
            (rect.right() - 4.0).max(rect.left() + 60.0),
            rect.bottom() - TIMELINE_FOOTER_HEIGHT,
        ),
    );
    painter.rect_filled(chart_rect, 0.0, chart_bg);
    painter.rect_stroke(
        chart_rect,
        0.0,
        Stroke::new(1.0, chart_border),
    );

    let chart_hover = ui.interact(
        chart_rect,
        ui.id().with("timeline_chart_hover"),
        Sense::click_and_drag(),
    );

    let mut view_changed = false;
    if chart_hover.double_clicked() {
        view_start = range_start;
        view_end = range_end;
        view_changed = true;
    }

    if chart_hover.hovered() {
        let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll_y.abs() > f32::EPSILON {
            let zoom_factor = if scroll_y > 0.0 { 0.92 } else { 1.08 };
            let hover_x = chart_hover
                .hover_pos()
                .map(|p| p.x)
                .unwrap_or(chart_rect.center().x);
            let anchor_ratio =
                ((hover_x - chart_rect.left()) / chart_rect.width().max(1.0)).clamp(0.0, 1.0);
            let current_span = (view_end - view_start).max(1);
            let anchor_ts = view_start + (anchor_ratio * current_span as f32) as i64;
            let mut new_span = ((current_span as f32) * zoom_factor).round() as i64;
            new_span = new_span.clamp(MIN_TIMELINE_VIEW_SECS, day_span_secs);
            let new_start = anchor_ts - (anchor_ratio * new_span as f32) as i64;
            let (clamped_start, clamped_end) =
                clamp_view_span(new_start, new_span, range_start, range_end);
            view_start = clamped_start;
            view_end = clamped_end;
            view_changed = true;
        }
    }

    if chart_hover.dragged() {
        let delta_x = ui.input(|i| i.pointer.delta().x);
        if delta_x.abs() > f32::EPSILON {
            let span = (view_end - view_start).max(1);
            let shift_secs = (-(delta_x / chart_rect.width().max(1.0)) * span as f32).round() as i64;
            if shift_secs != 0 {
                let (clamped_start, clamped_end) =
                    clamp_view_span(view_start + shift_secs, span, range_start, range_end);
                view_start = clamped_start;
                view_end = clamped_end;
                view_changed = true;
            }
        }
    }

    if view_changed {
        if view_start <= range_start && view_end >= range_end {
            *view_range = None;
        } else {
            *view_range = Some((view_start, view_end));
        }
    }
    let view_span = (view_end - view_start).max(1) as f32;
    let (visible_start_idx, visible_end_idx) =
        visible_timeline_segment_bounds(timeline_segments, view_start, view_end);
    let visible_segments = &timeline_segments[visible_start_idx..visible_end_idx];
    let label_y = rect.top() + 4.0;
    let label_font = FontId::monospace(10.0);
    let is_full_day_view =
        (range_end - range_start) == 24 * 3600 && view_start == range_start && view_end == range_end;

    let left_edge_label = if is_full_day_view {
        "00".to_owned()
    } else {
        format_tick_label(view_start, range_end - range_start)
    };
    let right_edge_label = if is_full_day_view {
        "24".to_owned()
    } else {
        format_tick_label(view_end, range_end - range_start)
    };

    painter.text(
        Pos2::new(chart_rect.left() + 2.0, label_y),
        Align2::LEFT_TOP,
        left_edge_label,
        label_font.clone(),
        label_color,
    );
    painter.text(
        Pos2::new(chart_rect.right() - 2.0, label_y),
        Align2::RIGHT_TOP,
        right_edge_label,
        label_font.clone(),
        label_color,
    );

    let grid_step = choose_grid_step_seconds(range_end - range_start);
    let first_tick = align_timestamp_to_step(range_start, grid_step);
    let mut tick = first_tick;
    while tick <= range_end {
        if tick >= view_start && tick <= view_end {
            let ratio = ((tick - view_start) as f32 / view_span).clamp(0.0, 1.0);
            let x = chart_rect.left() + ratio * chart_rect.width();
            let is_major = is_major_tick(tick, grid_step);
            let color = if is_major { major_grid } else { minor_grid };
            painter.line_segment(
                [Pos2::new(x, chart_rect.top()), Pos2::new(x, chart_rect.bottom())],
                Stroke::new(1.0, color),
            );

            if is_major {
                // Endpoint labels are drawn separately on left/right edges.
                if tick > view_start && tick < view_end {
                    let label = format_tick_label(tick, range_end - range_start);
                    painter.text(
                        Pos2::new(x, label_y),
                        Align2::CENTER_TOP,
                        label,
                        label_font.clone(),
                        label_color,
                    );
                }
            }
        }
        tick = tick.saturating_add(grid_step);
    }

    for seg in visible_segments {
        if should_hide_in_visualization(&seg.app_name, seg.is_idle, seg.process_path.as_deref()) {
            continue;
        }
        let seg_start = seg.start_ts.max(view_start);
        let seg_end = seg.end_ts.min(view_end);
        if seg_end <= seg_start {
            continue;
        }
        let start_ratio = ((seg_start - view_start) as f32 / view_span).clamp(0.0, 1.0);
        let end_ratio = ((seg_end - view_start) as f32 / view_span).clamp(0.0, 1.0);
        let x0 = chart_rect.left() + start_ratio * chart_rect.width();
        let x1 = chart_rect.left() + end_ratio * chart_rect.width();

        let seg_rect = Rect::from_min_max(
            Pos2::new(x0, chart_rect.top()),
            Pos2::new((x1).max(x0 + 1.0), chart_rect.bottom()),
        );

        let color = display_color_from_maps(
            icon_colors,
            app_colors,
            seg.is_idle,
            &seg.app_name,
            seg.process_path.as_deref(),
        );
        painter.rect_filled(seg_rect, 2.0, color);
    }

    if let Some(seg) = find_hovered_timeline_segment(
        chart_hover.hover_pos(),
        chart_rect,
        view_start,
        view_end,
        visible_segments,
    ) {
        let duration = seg.end_ts.saturating_sub(seg.start_ts);
        let app_label = resolve_timeline_app_label(seg, summary_rows, process_display_name_cache);
        egui::show_tooltip_at_pointer(
            ui.ctx(),
            ui.layer_id(),
            ui.id().with("timeline_hover_tooltip"),
            |ui| {
                let app_label_color = if ui.visuals().dark_mode {
                    ui.visuals().text_color()
                } else {
                    Color32::BLACK
                };
                ui.label(
                    egui::RichText::new(format!(
                        "{}: {}",
                        timeline_tip_text(language, "app"),
                        app_label
                    ))
                    .strong()
                    .color(app_label_color),
                );
                if seg.multi_title {
                    ui.label(format!(
                        "{}: {}",
                        timeline_tip_text(language, "title"),
                        timeline_tip_text(language, "multi_title")
                    ));
                } else if let Some(title) = &seg.title {
                    ui.label(format!(
                        "{}: {}",
                        timeline_tip_text(language, "title"),
                        title
                    ));
                }
                ui.label(format!(
                    "{}: {}",
                    timeline_tip_text(language, "duration"),
                    format_duration(duration)
                ));
                ui.label(format!(
                    "{}: {} - {}",
                    timeline_tip_text(language, "range"),
                    format_hms(seg.start_ts),
                    format_hms(seg.end_ts)
                ));
            },
        );
    }

}

fn visible_timeline_segment_bounds(
    segments: &[TimelineRenderSegment],
    view_start: i64,
    view_end: i64,
) -> (usize, usize) {
    if segments.is_empty() || view_end <= view_start {
        return (0, 0);
    }
    let start = segments.partition_point(|seg| seg.end_ts <= view_start);
    let end = segments.partition_point(|seg| seg.start_ts < view_end);
    (start.min(end), end)
}

fn find_hovered_timeline_segment<'a>(
    hover_pos: Option<Pos2>,
    chart_rect: Rect,
    day_start: i64,
    day_end: i64,
    segments: &'a [TimelineRenderSegment],
) -> Option<&'a TimelineRenderSegment> {
    let hover_pos = hover_pos?;
    if !chart_rect.contains(hover_pos) {
        return None;
    }
    if day_end <= day_start || segments.is_empty() {
        return None;
    }

    let day_span_secs = (day_end - day_start).max(1);
    let ratio = ((hover_pos.x - chart_rect.left()) / chart_rect.width().max(1.0)).clamp(0.0, 1.0);
    let hover_ts = (day_start + (ratio * day_span_secs as f32) as i64).min(day_end.saturating_sub(1));
    let idx = segments.partition_point(|seg| seg.end_ts <= hover_ts);
    let seg = segments.get(idx)?;
    if hover_ts < seg.start_ts || hover_ts >= seg.end_ts {
        return None;
    }
    if should_hide_in_visualization(&seg.app_name, seg.is_idle, seg.process_path.as_deref()) {
        return None;
    }
    Some(seg)
}

fn build_timeline_segments(
    day_start: i64,
    day_end: i64,
    segments: &[Segment],
    selected_app_keys: &HashSet<String>,
) -> Vec<TimelineRenderSegment> {
    const MERGE_GAP_TOLERANCE_SECS: i64 = 1;

    let mut merged: Vec<TimelineRenderSegment> = Vec::new();

    for seg in segments {
        if !selected_app_keys.is_empty()
            && !selected_app_keys.contains(&normalize_app_key(&seg.app_name))
        {
            continue;
        }

        let clipped_start = seg.start_ts.max(day_start);
        let clipped_end = seg.end_ts.min(day_end);
        if clipped_end <= clipped_start {
            continue;
        }

        if let Some(last) = merged.last_mut() {
            if last.is_idle == seg.is_idle
                && last.app_name == seg.app_name
                && last.process_path == seg.process_path
                && clipped_start <= last.end_ts.saturating_add(MERGE_GAP_TOLERANCE_SECS)
            {
                if clipped_end > last.end_ts {
                    last.end_ts = clipped_end;
                }
                if !same_title(&last.title, &seg.title) {
                    last.multi_title = true;
                }
                continue;
            }
        }

        merged.push(TimelineRenderSegment {
            start_ts: clipped_start,
            end_ts: clipped_end,
            is_idle: seg.is_idle,
            app_name: seg.app_name.clone(),
            process_path: seg.process_path.clone(),
            title: seg.title.clone(),
            multi_title: false,
        });
    }

    merged
}

fn same_title(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x == y,
        (None, None) => true,
        _ => false,
    }
}

fn draw_fallback_icon(painter: &egui::Painter, rect: Rect, fill: Color32) {
    painter.rect_filled(rect, 3.0, fill);
}

fn display_app_name(raw_name: &str, is_idle: bool) -> String {
    if is_idle {
        return "IDLE".to_owned();
    }

    let trimmed = raw_name.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("UNKNOWN") {
        return "UNKNOWN".to_owned();
    }

    strip_exe_suffix(trimmed).to_owned()
}

fn strip_exe_suffix(name: &str) -> &str {
    if name.to_ascii_lowercase().ends_with(".exe") && name.len() > 4 {
        &name[..name.len() - 4]
    } else {
        name
    }
}

fn normalize_app_key(app_name: &str) -> String {
    strip_exe_suffix(app_name.trim()).to_ascii_lowercase()
}

fn display_color_from_maps(
    icon_colors: &HashMap<String, Color32>,
    app_colors: &HashMap<String, Color32>,
    is_idle: bool,
    app_name: &str,
    process_path: Option<&str>,
) -> Color32 {
    if should_hide_in_visualization(app_name, is_idle, process_path) {
        return Color32::TRANSPARENT;
    }
    let icon_color = process_path
        .and_then(|path| icon_colors.get(path).copied())
        .or_else(|| app_colors.get(&normalize_app_key(app_name)).copied());

    if let Some(icon_color) = icon_color {
        return icon_color;
    }
    color_for_app(false, app_name)
}

fn is_system_level_app(app_name: &str, process_path: Option<&str>) -> bool {
    let app = normalize_app_key(app_name);
    if app == "explorer" {
        return false;
    }
    const SYSTEM_APPS: [&str; 11] = [
        "searchhost",
        "shellexperiencehost",
        "startmenuexperiencehost",
        "applicationframehost",
        "runtimebroker",
        "textinputhost",
        "taskhostw",
        "sihost",
        "lockapp",
        "dwm",
        "ctfmon",
    ];
    if SYSTEM_APPS.contains(&app.as_str()) {
        return true;
    }

    let Some(path) = process_path else {
        return false;
    };
    let normalized = path.trim().replace('/', "\\").to_ascii_lowercase();
    normalized.starts_with(r"c:\windows\") || normalized.starts_with(r"\\?\c:\windows\")
}

fn should_hide_in_visualization(app_name: &str, is_idle: bool, process_path: Option<&str>) -> bool {
    is_idle || is_system_level_app(app_name, process_path)
}

fn sanitize_view_range(
    view_range: Option<(i64, i64)>,
    day_start: i64,
    day_end: i64,
) -> (i64, i64) {
    let day_span = (day_end - day_start).max(1);
    if let Some((start, end)) = view_range {
        let span = (end - start).clamp(MIN_TIMELINE_VIEW_SECS, day_span);
        return clamp_view_span(start, span, day_start, day_end);
    }
    (day_start, day_end)
}

fn clamp_view_span(start: i64, span: i64, day_start: i64, day_end: i64) -> (i64, i64) {
    let day_span = (day_end - day_start).max(1);
    let span = span.clamp(MIN_TIMELINE_VIEW_SECS, day_span);
    let max_start = day_end - span;
    let clamped_start = start.clamp(day_start, max_start);
    (clamped_start, clamped_start + span)
}

fn dominant_color_from_icon(image: &egui::ColorImage) -> Option<Color32> {
    let mut bins: HashMap<u16, (u32, u64, u64, u64)> = HashMap::new();

    for pixel in &image.pixels {
        if pixel.a() < 24 {
            continue;
        }

        let r = pixel.r() as u32;
        let g = pixel.g() as u32;
        let b = pixel.b() as u32;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let delta = max - min;

        // Ignore near-white icon background pixels.
        if max > 236 && delta < 10 {
            continue;
        }

        let key = (((r as u16) >> 3) << 10) | (((g as u16) >> 3) << 5) | ((b as u16) >> 3);
        let entry = bins.entry(key).or_insert((0, 0, 0, 0));
        entry.0 += 1;
        entry.1 += r as u64;
        entry.2 += g as u64;
        entry.3 += b as u64;
    }

    let (_, (count, sum_r, sum_g, sum_b)) = bins.into_iter().max_by_key(|(_, data)| data.0)?;
    let count = count.max(1) as u64;
    let mut r = (sum_r / count) as u8;
    let mut g = (sum_g / count) as u8;
    let mut b = (sum_b / count) as u8;

    // Keep colors readable on light UI background.
    let luma = (u32::from(r) * 2126 + u32::from(g) * 7152 + u32::from(b) * 722) / 10_000;
    if luma < 50 {
        r = r.saturating_add(18);
        g = g.saturating_add(18);
        b = b.saturating_add(18);
    }

    Some(Color32::from_rgb(r, g, b))
}

fn should_hide_summary_app(app_name: &str, is_idle: bool, process_path: Option<&str>) -> bool {
    should_hide_in_visualization(app_name, is_idle, process_path)
}

fn month_start(date: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(date.year(), date.month(), 1).unwrap_or(date)
}

fn add_months(month_start: NaiveDate, offset_months: i32) -> Option<NaiveDate> {
    let total_months = i64::from(month_start.year()) * 12
        + i64::from(month_start.month0())
        + i64::from(offset_months);
    let year = i32::try_from(total_months.div_euclid(12)).ok()?;
    let month0 = total_months.rem_euclid(12) as u32;
    NaiveDate::from_ymd_opt(year, month0 + 1, 1)
}

fn days_in_month(month_start: NaiveDate) -> u32 {
    if let Some(next_month) = add_months(month_start, 1) {
        if let Some(last_day) = next_month.checked_sub_days(Days::new(1)) {
            return last_day.day();
        }
    }
    31
}

fn quarter_start(date: NaiveDate) -> Option<NaiveDate> {
    let quarter_month = (date.month0() / 3) * 3 + 1;
    NaiveDate::from_ymd_opt(date.year(), quarter_month, 1)
}

fn date_range_bounds(start_date: NaiveDate, end_exclusive_date: NaiveDate) -> Option<(i64, i64)> {
    let start = local_midnight_ts(start_date)?;
    let end = local_midnight_ts(end_exclusive_date)?;
    Some((start, end))
}

fn rolling_range_bounds(anchor_date: NaiveDate, days: u64) -> Option<(i64, i64)> {
    if days == 0 {
        return None;
    }
    let start_date = anchor_date.checked_sub_days(Days::new(days.saturating_sub(1)))?;
    let end_exclusive = anchor_date.checked_add_days(Days::new(1))?;
    date_range_bounds(start_date, end_exclusive)
}

fn range_dates_for_preset(anchor_date: NaiveDate, preset: RangePreset) -> Option<(NaiveDate, NaiveDate)> {
    match preset {
        RangePreset::All => None,
        RangePreset::Day7 => {
            let start = anchor_date.checked_sub_days(Days::new(6))?;
            Some((start, anchor_date))
        }
        RangePreset::Day30 => {
            let start = anchor_date.checked_sub_days(Days::new(29))?;
            Some((start, anchor_date))
        }
        RangePreset::ThisWeek => {
            let start = anchor_date
                .checked_sub_days(Days::new(anchor_date.weekday().num_days_from_monday() as u64))?;
            let end = start.checked_add_days(Days::new(6))?;
            Some((start, end))
        }
        RangePreset::ThisMonth => {
            let start = month_start(anchor_date);
            let end = add_months(start, 1)?.checked_sub_days(Days::new(1))?;
            Some((start, end))
        }
        RangePreset::ThisQuarter => {
            let start = quarter_start(anchor_date)?;
            let end = add_months(start, 3)?.checked_sub_days(Days::new(1))?;
            Some((start, end))
        }
        RangePreset::YearToDate => {
            let start = NaiveDate::from_ymd_opt(anchor_date.year(), 1, 1)?;
            Some((start, anchor_date))
        }
    }
}

fn range_bounds_for_preset(anchor_date: NaiveDate, preset: RangePreset) -> Option<(i64, i64)> {
    match preset {
        RangePreset::All => {
            let end_exclusive = Local::now().date_naive().checked_add_days(Days::new(1))?;
            let end_ts = local_midnight_ts(end_exclusive)?;
            Some((0, end_ts))
        }
        RangePreset::Day7 => rolling_range_bounds(anchor_date, 7),
        RangePreset::Day30 => rolling_range_bounds(anchor_date, 30),
        RangePreset::ThisWeek => {
            let week_start = anchor_date
                .checked_sub_days(Days::new(anchor_date.weekday().num_days_from_monday() as u64))?;
            let week_end = week_start.checked_add_days(Days::new(7))?;
            date_range_bounds(week_start, week_end)
        }
        RangePreset::ThisMonth => {
            let start = month_start(anchor_date);
            let end = add_months(start, 1)?;
            date_range_bounds(start, end)
        }
        RangePreset::ThisQuarter => {
            let start = quarter_start(anchor_date)?;
            let end = add_months(start, 3)?;
            date_range_bounds(start, end)
        }
        RangePreset::YearToDate => {
            let start = NaiveDate::from_ymd_opt(anchor_date.year(), 1, 1)?;
            let end_exclusive = anchor_date.checked_add_days(Days::new(1))?;
            date_range_bounds(start, end_exclusive)
        }
    }
}

fn choose_grid_step_seconds(range_span: i64) -> i64 {
    const HOUR: i64 = 3600;
    const DAY: i64 = 24 * HOUR;

    if range_span <= 2 * DAY {
        HOUR
    } else if range_span <= 7 * DAY {
        6 * HOUR
    } else if range_span <= 31 * DAY {
        DAY
    } else if range_span <= 120 * DAY {
        3 * DAY
    } else {
        7 * DAY
    }
}

fn align_timestamp_to_step(ts: i64, step_secs: i64) -> i64 {
    if step_secs <= 0 {
        return ts;
    }
    ts.div_euclid(step_secs) * step_secs
}

fn is_major_tick(_ts: i64, _step_secs: i64) -> bool {
    true
}

fn format_tick_label(ts: i64, range_span: i64) -> String {
    if let Some(dt) = Local.timestamp_opt(ts, 0).single() {
        let day_span = 24 * 3600;
        if range_span <= 2 * day_span {
            return dt.format("%H").to_string();
        }
        if range_span <= 120 * day_span {
            return dt.format("%m-%d").to_string();
        }
        return dt.format("%Y-%m").to_string();
    }
    "--".to_owned()
}

fn probe_backend_status(db_path: &PathBuf) -> Result<BackendStatus> {
    let checked_ts = unix_seconds_now();
    let daemon_running = is_tracker_daemon_running();
    let last_write_ts = load_latest_segment_end_ts(db_path)?;
    let heartbeat_recent = last_write_ts
        .map(|ts| checked_ts.saturating_sub(ts) <= BACKEND_HEARTBEAT_GRACE_SECS)
        .unwrap_or(false);

    let (health, detail) = if daemon_running && (heartbeat_recent || last_write_ts.is_none()) {
        (BackendHealth::Running, None)
    } else {
        (BackendHealth::Stopped, None)
    };

    Ok(BackendStatus {
        health,
        last_write_ts,
        checked_ts,
        detail,
    })
}

fn load_latest_segment_end_ts(db_path: &PathBuf) -> Result<Option<i64>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed to open database: {}", db_path.display()))?;
    let latest_end_ts = conn
        .query_row("SELECT MAX(end_ts) FROM segments", [], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .context("failed to query latest segment timestamp")?;
    Ok(latest_end_ts)
}

#[cfg(target_os = "windows")]
fn is_tracker_daemon_running() -> bool {
    const MUTEX_SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
    let mutex_name: Vec<u16> = TRACKER_DAEMON_MUTEX_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle: HANDLE =
        unsafe { OpenMutexW(MUTEX_SYNCHRONIZE_ACCESS, 0, mutex_name.as_ptr()) };
    if handle.is_null() {
        return false;
    }
    unsafe {
        CloseHandle(handle);
    }
    true
}

#[cfg(not(target_os = "windows"))]
fn is_tracker_daemon_running() -> bool {
    false
}

fn load_segments_for_range(db_path: &PathBuf, range_start: i64, range_end: i64) -> Result<Vec<Segment>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed to open database: {}", db_path.display()))?;

    let mut stmt = conn.prepare(
        "\
        SELECT
          s.start_ts,
          s.end_ts,
          s.is_idle,
          a.exe_name,
          a.process_path,
          t.title
        FROM segments s
        LEFT JOIN apps a ON a.id = s.app_id
        LEFT JOIN titles t ON t.id = s.title_id
        WHERE s.end_ts > ?1
          AND s.start_ts < ?2
        ORDER BY s.start_ts ASC",
    )?;

    let mut rows = stmt.query(params![range_start, range_end])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let is_idle: i64 = row.get(2)?;
        let app_name: Option<String> = row.get(3)?;
        let process_path: Option<String> = row.get(4)?;
        let title: Option<String> = row.get(5)?;

        result.push(Segment {
            start_ts: row.get(0)?,
            end_ts: row.get(1)?,
            is_idle: is_idle != 0,
            app_name: app_name.unwrap_or_else(|| "UNKNOWN".to_owned()),
            process_path,
            title,
        });
    }
    Ok(result)
}

fn local_midnight_ts(date: NaiveDate) -> Option<i64> {
    match Local.with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0) {
        LocalResult::Single(dt) => Some(dt.timestamp()),
        LocalResult::Ambiguous(a, b) => Some(a.timestamp().min(b.timestamp())),
        LocalResult::None => None,
    }
}

fn format_duration(seconds: i64) -> String {
    let secs = seconds.max(0);
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

fn format_hms(unix_ts: i64) -> String {
    if let Some(dt) = Local.timestamp_opt(unix_ts, 0).single() {
        return dt.format("%H:%M:%S").to_string();
    }
    "--:--:--".to_owned()
}

fn format_local_datetime(unix_ts: i64) -> String {
    if let Some(dt) = Local.timestamp_opt(unix_ts, 0).single() {
        return dt.format("%Y-%m-%d %H:%M:%S").to_string();
    }
    "--".to_owned()
}

fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn ensure_tracking_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "\
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS apps (
          id INTEGER PRIMARY KEY,
          exe_name TEXT NOT NULL,
          process_path TEXT NOT NULL,
          UNIQUE(exe_name, process_path)
        );

        CREATE TABLE IF NOT EXISTS titles (
          id INTEGER PRIMARY KEY,
          title TEXT NOT NULL UNIQUE
        );

        CREATE TABLE IF NOT EXISTS segments (
          id INTEGER PRIMARY KEY,
          start_ts INTEGER NOT NULL,
          end_ts INTEGER NOT NULL CHECK (end_ts >= start_ts),
          app_id INTEGER,
          title_id INTEGER,
          is_idle INTEGER NOT NULL DEFAULT 0,
          pid INTEGER,
          pid_create_time INTEGER,
          FOREIGN KEY(app_id) REFERENCES apps(id),
          FOREIGN KEY(title_id) REFERENCES titles(id)
        );

        CREATE INDEX IF NOT EXISTS idx_segments_start ON segments(start_ts);
        CREATE INDEX IF NOT EXISTS idx_segments_app_start ON segments(app_id, start_ts);
        CREATE INDEX IF NOT EXISTS idx_segments_idle_start ON segments(is_idle, start_ts);

        CREATE TABLE IF NOT EXISTS app_visual_cache (
          app_key TEXT PRIMARY KEY,
          process_path TEXT,
          color_rgba INTEGER NOT NULL,
          icon_width INTEGER,
          icon_height INTEGER,
          icon_rgba BLOB,
          display_name TEXT,
          updated_ts INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_app_visual_cache_process_path
          ON app_visual_cache(process_path);",
    )
    .context("failed to ensure tracking schema")?;
    Ok(())
}

fn encode_cached_icon_image(image: &egui::ColorImage) -> ([usize; 2], Vec<u8>) {
    let mut rgba = Vec::with_capacity(image.pixels.len() * 4);
    for pixel in &image.pixels {
        rgba.push(pixel.r());
        rgba.push(pixel.g());
        rgba.push(pixel.b());
        rgba.push(pixel.a());
    }
    (image.size, rgba)
}

fn decode_cached_icon_image(width: usize, height: usize, rgba: &[u8]) -> Option<egui::ColorImage> {
    let expected_len = width.checked_mul(height)?.checked_mul(4)?;
    if width == 0 || height == 0 || rgba.len() != expected_len {
        return None;
    }
    Some(egui::ColorImage::from_rgba_unmultiplied([width, height], rgba))
}

fn encode_color32_rgba(color: Color32) -> i64 {
    ((i64::from(color.r())) << 24)
        | ((i64::from(color.g())) << 16)
        | ((i64::from(color.b())) << 8)
        | i64::from(color.a())
}

fn decode_color32_rgba(value: i64) -> Color32 {
    let r = ((value >> 24) & 0xFF) as u8;
    let g = ((value >> 16) & 0xFF) as u8;
    let b = ((value >> 8) & 0xFF) as u8;
    let a = (value & 0xFF) as u8;
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

fn load_cached_app_visuals_from_db(conn: &Connection) -> Result<HashMap<String, CachedAppVisual>> {
    let mut stmt = conn.prepare(
        "\
        SELECT
          app_key,
          process_path,
          color_rgba,
          icon_width,
          icon_height,
          icon_rgba,
          display_name
        FROM app_visual_cache",
    )?;

    let mut rows = stmt.query([])?;
    let mut result = HashMap::new();
    while let Some(row) = rows.next()? {
        let app_key: String = row.get(0)?;
        let process_path: Option<String> = row.get(1)?;
        let color_rgba: i64 = row.get(2)?;
        let icon_width: Option<i64> = row.get(3)?;
        let icon_height: Option<i64> = row.get(4)?;
        let icon_rgba: Option<Vec<u8>> = row.get(5)?;
        let display_name: Option<String> = row.get(6)?;

        let icon_size = match (icon_width, icon_height, icon_rgba.as_ref()) {
            (Some(w), Some(h), Some(buf))
                if w > 0
                    && h > 0
                    && (w as usize)
                        .checked_mul(h as usize)
                        .and_then(|pixels| pixels.checked_mul(4))
                        .is_some_and(|expected| expected == buf.len()) =>
            {
                Some([w as usize, h as usize])
            }
            _ => None,
        };

        result.insert(
            app_key,
            CachedAppVisual {
                process_path,
                color: decode_color32_rgba(color_rgba),
                icon_size,
                icon_rgba,
                display_name,
            },
        );
    }

    Ok(result)
}

fn upsert_cached_app_visual(conn: &Connection, app_key: &str, visual: &CachedAppVisual) -> Result<()> {
    let (icon_width, icon_height, icon_rgba) = match (visual.icon_size, visual.icon_rgba.as_ref()) {
        (Some([w, h]), Some(buf)) => (Some(w as i64), Some(h as i64), Some(buf.as_slice())),
        _ => (None, None, None),
    };

    conn.execute(
        "\
        INSERT INTO app_visual_cache (
          app_key,
          process_path,
          color_rgba,
          icon_width,
          icon_height,
          icon_rgba,
          display_name,
          updated_ts
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ON CONFLICT(app_key) DO UPDATE SET
          process_path = excluded.process_path,
          color_rgba = excluded.color_rgba,
          icon_width = excluded.icon_width,
          icon_height = excluded.icon_height,
          icon_rgba = excluded.icon_rgba,
          display_name = excluded.display_name,
          updated_ts = excluded.updated_ts",
        params![
            app_key,
            visual.process_path.as_deref(),
            encode_color32_rgba(visual.color),
            icon_width,
            icon_height,
            icon_rgba,
            visual.display_name.as_deref(),
            unix_seconds_now(),
        ],
    )
    .context("failed to upsert app visual cache")?;
    Ok(())
}

fn find_csv_header_index(headers: &StringRecord, aliases: &[&str]) -> Option<usize> {
    let mut normalized_aliases = HashSet::with_capacity(aliases.len());
    for alias in aliases {
        normalized_aliases.insert(normalize_csv_header_key(alias));
    }

    headers.iter().position(|header| {
        let normalized = normalize_csv_header_key(header);
        normalized_aliases.contains(&normalized)
    })
}

fn normalize_csv_header_key(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_lowercase());
        } else if ch.is_alphanumeric() {
            normalized.push(ch);
        }
    }
    normalized
}

fn csv_record_text<'a>(record: &'a StringRecord, idx: Option<usize>) -> Option<&'a str> {
    idx.and_then(|i| record.get(i))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn parse_import_csv_row(record: &StringRecord, columns: &ImportCsvColumns) -> Option<ParsedImportRow> {
    let start_ts = csv_record_text(record, columns.start_ts)
        .and_then(parse_unix_seconds)
        .or_else(|| csv_record_text(record, columns.start_local).and_then(parse_local_datetime_to_unix));

    let mut end_ts = csv_record_text(record, columns.end_ts)
        .and_then(parse_unix_seconds)
        .or_else(|| csv_record_text(record, columns.end_local).and_then(parse_local_datetime_to_unix));

    let duration_secs = csv_record_text(record, columns.duration).and_then(parse_duration_to_seconds);

    if end_ts.is_none() {
        if let (Some(start), Some(duration)) = (start_ts, duration_secs) {
            end_ts = Some(start.saturating_add(duration));
        }
    }

    let start_ts = start_ts?;
    let mut end_ts = end_ts?;
    if end_ts <= start_ts {
        if let Some(duration) = duration_secs {
            end_ts = start_ts.saturating_add(duration);
        }
    }
    if end_ts <= start_ts {
        return None;
    }

    let process_text = csv_record_text(record, columns.process)
        .or_else(|| csv_record_text(record, columns.app_name))?;
    let app_name_text = csv_record_text(record, columns.app_name).unwrap_or(process_text);
    let is_idle = csv_record_text(record, columns.is_idle)
        .and_then(parse_idle_flag)
        .unwrap_or_else(|| infer_idle_from_text(process_text) || infer_idle_from_text(app_name_text));

    let app_name = if is_idle {
        "IDLE".to_owned()
    } else {
        app_name_text.to_owned()
    };

    let process_path = csv_record_text(record, columns.process_path)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| synthetic_import_process_path(process_text));

    let title = csv_record_text(record, columns.title).map(ToOwned::to_owned);

    Some(ParsedImportRow {
        start_ts,
        end_ts,
        is_idle,
        app_name,
        process_path,
        title,
    })
}

fn parse_unix_seconds(value: &str) -> Option<i64> {
    value.trim().parse::<i64>().ok()
}

fn parse_local_datetime_to_unix(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    const FORMATS: [&str; 8] = [
        "%Y-%m-%d %H:%M:%S",
        "%Y/%m/%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y/%m/%d %H:%M",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y/%m/%d %H:%M:%S%.f",
    ];

    for format in FORMATS {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, format) {
            return match Local.from_local_datetime(&naive) {
                LocalResult::Single(dt) => Some(dt.timestamp()),
                LocalResult::Ambiguous(a, b) => Some(a.timestamp().min(b.timestamp())),
                LocalResult::None => None,
            };
        }
    }

    None
}

fn parse_duration_to_seconds(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    if let Ok(seconds) = value.parse::<i64>() {
        return (seconds > 0).then_some(seconds);
    }

    let parts: Vec<&str> = value.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }

    let mut values = Vec::with_capacity(parts.len());
    for part in parts {
        let parsed = part.trim().parse::<i64>().ok()?;
        if parsed < 0 {
            return None;
        }
        values.push(parsed);
    }

    let seconds = if values.len() == 3 {
        values[0]
            .saturating_mul(3600)
            .saturating_add(values[1].saturating_mul(60))
            .saturating_add(values[2])
    } else {
        values[0].saturating_mul(60).saturating_add(values[1])
    };

    (seconds > 0).then_some(seconds)
}

fn parse_idle_flag(value: &str) -> Option<bool> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "y" => Some(true),
        "0" | "false" | "no" | "n" => Some(false),
        _ => None,
    }
}

fn infer_idle_from_text(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    normalized == "idle" || normalized == "idling" || normalized == "afk" || value.trim() == "\u{7A7A}\u{95F2}"
}

fn synthetic_import_process_path(process: &str) -> String {
    let cleaned = process
        .trim()
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>();
    if cleaned.is_empty() {
        "<import:UNKNOWN>".to_owned()
    } else {
        format!("<import:{}>", cleaned)
    }
}

fn upsert_app_in_tx(
    tx: &rusqlite::Transaction<'_>,
    cache: &mut HashMap<(String, String), i64>,
    exe_name: &str,
    process_path: &str,
) -> Result<i64> {
    let key = (exe_name.to_owned(), process_path.to_owned());
    if let Some(id) = cache.get(&key) {
        return Ok(*id);
    }

    tx.execute(
        "\
        INSERT INTO apps (exe_name, process_path)
        VALUES (?1, ?2)
        ON CONFLICT(exe_name, process_path) DO NOTHING",
        params![exe_name, process_path],
    )
    .context("failed to upsert imported app")?;

    let app_id = tx
        .query_row(
            "SELECT id FROM apps WHERE exe_name = ?1 AND process_path = ?2",
            params![exe_name, process_path],
            |row| row.get::<_, i64>(0),
        )
        .context("failed to resolve imported app id")?;

    cache.insert(key, app_id);
    Ok(app_id)
}

fn upsert_title_in_tx(
    tx: &rusqlite::Transaction<'_>,
    cache: &mut HashMap<String, i64>,
    title: &str,
) -> Result<i64> {
    if let Some(id) = cache.get(title) {
        return Ok(*id);
    }

    tx.execute(
        "\
        INSERT INTO titles (title)
        VALUES (?1)
        ON CONFLICT(title) DO NOTHING",
        params![title],
    )
    .context("failed to upsert imported title")?;

    let title_id = tx
        .query_row(
            "SELECT id FROM titles WHERE title = ?1",
            params![title],
            |row| row.get::<_, i64>(0),
        )
        .context("failed to resolve imported title id")?;

    cache.insert(title.to_owned(), title_id);
    Ok(title_id)
}

fn unix_seconds_now() -> i64 {
    Local::now().timestamp()
}

fn resolve_timeline_app_label(
    seg: &TimelineRenderSegment,
    summary_rows: &[SummaryRow],
    process_display_name_cache: &HashMap<String, String>,
) -> String {
    if let Some(row) = summary_rows.iter().find(|row| row.app_name == seg.app_name) {
        if let Some(path) = row.process_path.as_deref() {
            if let Some(display_name) = process_display_name_cache.get(path) {
                let trimmed = display_name.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_owned();
                }
            }
        }
        let trimmed = row.display_name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }

    if let Some(path) = seg.process_path.as_deref() {
        if let Some(display_name) = process_display_name_cache.get(path) {
            let trimmed = display_name.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
    }

    display_app_name(&seg.app_name, seg.is_idle)
}

fn resolve_export_process_name(
    is_idle: bool,
    app_name: &str,
    process_path: Option<&str>,
    process_display_name_cache: &HashMap<String, String>,
    lookup_cache: &mut HashMap<String, String>,
) -> String {
    if let Some(path) = process_path.map(str::trim).filter(|value| !value.is_empty()) {
        if let Some(cached_name) = process_display_name_cache.get(path) {
            let trimmed = cached_name.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }

        if let Some(cached_name) = lookup_cache.get(path) {
            return cached_name.clone();
        }

        if let Some(description) = load_app_file_description(path) {
            let trimmed = description.trim();
            if !trimmed.is_empty() {
                let resolved = trimmed.to_owned();
                lookup_cache.insert(path.to_owned(), resolved.clone());
                return resolved;
            }
        }
    }

    display_app_name(app_name, is_idle)
}

fn timeline_tip_text(language: UiLanguage, key: &'static str) -> &'static str {
    match language {
        UiLanguage::ZhCn => match key {
            "app" => "\u{5E94}\u{7528}",
            "title" => "\u{6807}\u{9898}",
            "duration" => "\u{65F6}\u{957F}",
            "range" => "\u{533A}\u{95F4}",
            "multi_title" => "\u{591A}\u{4E2A}\u{6807}\u{9898}",
            _ => key,
        },
        UiLanguage::EnUs => match key {
            "app" => "App",
            "title" => "Title",
            "duration" => "Duration",
            "range" => "Range",
            "multi_title" => "(multiple titles)",
            _ => key,
        },
    }
}

fn items_caption(language: UiLanguage) -> &'static str {
    match language {
        UiLanguage::ZhCn => "\u{6570}\u{91CF}",
        UiLanguage::EnUs => "Show",
    }
}

fn default_ui_language() -> UiLanguage {
    let locale = env::var("LANG").unwrap_or_default().to_ascii_lowercase();
    if locale.starts_with("zh") {
        UiLanguage::ZhCn
    } else {
        UiLanguage::EnUs
    }
}

fn load_ui_language(settings_path: &PathBuf) -> Option<UiLanguage> {
    let content = fs::read_to_string(settings_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    let language_code = value.get("language")?.as_str()?;
    UiLanguage::from_code(language_code)
}

fn persist_ui_language(settings_path: &PathBuf, language: UiLanguage) -> Result<()> {
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create settings directory: {}", parent.display()))?;
    }
    let payload = json!({
        "language": language.code()
    });
    let text = serde_json::to_string_pretty(&payload).context("failed to serialize UI settings")?;
    fs::write(settings_path, text)
        .with_context(|| format!("failed to write UI settings: {}", settings_path.display()))?;
    Ok(())
}

fn tr(language: UiLanguage, key: &'static str) -> &'static str {
    match language {
        UiLanguage::ZhCn => match key {
            "lang" => "\u{8BED}\u{8A00}",
            "range" => "\u{8303}\u{56F4}",
            "date" => "\u{65E5}\u{671F}",
            "from" => "\u{5F00}\u{59CB}",
            "to" => "\u{7ED3}\u{675F}",
            "items" => "\u{6761}\u{76EE}",
            "refresh" => "\u{5237}\u{65B0}",
            "today" => "\u{4ECA}\u{5929}",
            "single_day" => "\u{5355}\u{65E5}",
            "custom" => "\u{81EA}\u{5B9A}\u{4E49}",
            "rolling" => "\u{6EDA}\u{52A8}",
            "calendar" => "\u{65E5}\u{5386}",
            "timeline" => "\u{65F6}\u{95F4}\u{8F74}",
            "top_apps" => "\u{5E94}\u{7528}\u{6392}\u{884C}",
            "total" => "\u{603B}\u{8BA1}",
            "import" => "\u{5BFC}\u{5165}",
            "export" => "\u{5BFC}\u{51FA}",
            "backup" => "\u{5907}\u{4EFD}",
            "format" => "\u{683C}\u{5F0F}",
            "path" => "\u{8DEF}\u{5F84}",
            "no_data" => "\u{5F53}\u{524D}\u{8303}\u{56F4}\u{6CA1}\u{6709}\u{5E94}\u{7528}\u{6570}\u{636E}\u{3002}",
            "status" => "\u{72B6}\u{6001}",
            "checked" => "\u{68C0}\u{67E5}\u{65F6}\u{95F4}",
            "last_write" => "\u{6700}\u{8FD1}\u{5199}\u{5165}",
            "backend" => "LimeTrace Backend",
            "running" => "\u{8FD0}\u{884C}\u{4E2D}",
            "stopped" => "\u{672A}\u{8FD0}\u{884C}",
            "unknown" => "\u{672A}\u{77E5}",
            "help.timeline" => "\u{65F6}\u{95F4}\u{8F74}",
            "help.zoom" => "- \u{6EDA}\u{8F6E}\u{FF1A}\u{7F29}\u{653E}",
            "help.pan" => "- \u{62D6}\u{62FD}\u{FF1A}\u{5E73}\u{79FB}",
            "help.reset" => "- \u{53CC}\u{51FB}\u{FF1A}\u{91CD}\u{7F6E}\u{89C6}\u{56FE}",
            _ => key,
        },
        UiLanguage::EnUs => match key {
            "lang" => "Lang",
            "range" => "Range",
            "date" => "Date",
            "from" => "From",
            "to" => "To",
            "items" => "Items",
            "refresh" => "Refresh",
            "today" => "Today",
            "single_day" => "Single Day",
            "custom" => "Custom",
            "rolling" => "Rolling",
            "calendar" => "Calendar",
            "timeline" => "Timeline",
            "top_apps" => "Top Apps",
            "total" => "Total",
            "import" => "Import",
            "export" => "Export",
            "backup" => "Backup",
            "format" => "Format",
            "path" => "Path",
            "no_data" => "No app data for the selected range.",
            "status" => "Status",
            "checked" => "Checked",
            "last_write" => "Last segment write",
            "backend" => "LimeTrace Backend",
            "running" => "Running",
            "stopped" => "Stopped",
            "unknown" => "Unknown",
            "help.timeline" => "Timeline",
            "help.zoom" => "- Mouse Wheel: Zoom",
            "help.pan" => "- Drag: Pan",
            "help.reset" => "- Double-click: Reset View",
            _ => key,
        },
    }
}

fn color_for_app(is_idle: bool, app_name: &str) -> Color32 {
    if is_idle {
        return Color32::from_rgb(158, 162, 168);
    }

    let mut hash: u64 = 1469598103934665603;
    for b in app_name.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }

    const PALETTE: [Color32; 12] = [
        Color32::from_rgb(33, 102, 172),
        Color32::from_rgb(178, 24, 43),
        Color32::from_rgb(65, 171, 93),
        Color32::from_rgb(217, 95, 14),
        Color32::from_rgb(117, 107, 177),
        Color32::from_rgb(230, 171, 2),
        Color32::from_rgb(27, 158, 119),
        Color32::from_rgb(102, 166, 30),
        Color32::from_rgb(231, 41, 138),
        Color32::from_rgb(166, 118, 29),
        Color32::from_rgb(102, 102, 102),
        Color32::from_rgb(1, 133, 113),
    ];
    PALETTE[(hash as usize) % PALETTE.len()]
}

fn configure_chinese_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    if let Some(font_bytes) = load_noto_sans_font_bytes() {
        fonts.font_data.insert(
            "noto_sans".to_owned(),
            egui::FontData::from_owned(font_bytes).into(),
        );
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            family.insert(0, "noto_sans".to_owned());
        }
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
            family.insert(0, "noto_sans".to_owned());
        }
        ctx.set_fonts(fonts);
        return;
    }

    let Some(font_bytes) = load_chinese_font_bytes() else {
        return;
    };
    fonts.font_data.insert(
        "zh_cn".to_owned(),
        egui::FontData::from_owned(font_bytes).into(),
    );
    if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        family.insert(0, "zh_cn".to_owned());
    }
    if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
        family.insert(0, "zh_cn".to_owned());
    }
    ctx.set_fonts(fonts);
}

fn configure_interaction_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.interaction.tooltip_delay = 0.0;
    });
}

fn load_noto_sans_font_bytes() -> Option<Vec<u8>> {
    let mut candidates = vec![
        PathBuf::from("assets").join("fonts").join("NotoSans-Regular.ttf"),
        PathBuf::from("assets")
            .join("fonts")
            .join("NotoSansSC-Regular.otf"),
        PathBuf::from("assets")
            .join("fonts")
            .join("NotoSansCJKsc-Regular.otf"),
    ];

    if let Some(windir) = env::var_os("WINDIR") {
        let font_dir = PathBuf::from(windir).join("Fonts");
        candidates.push(font_dir.join("NotoSans-Regular.ttf"));
        candidates.push(font_dir.join("NotoSansSC-Regular.otf"));
        candidates.push(font_dir.join("NotoSansCJKsc-Regular.otf"));
        candidates.push(font_dir.join("NotoSansCJK-Regular.ttc"));
    }

    candidates.push(PathBuf::from(r"C:\Windows\Fonts\NotoSans-Regular.ttf"));
    candidates.push(PathBuf::from(r"C:\Windows\Fonts\NotoSansSC-Regular.otf"));
    candidates.push(PathBuf::from(r"C:\Windows\Fonts\NotoSansCJKsc-Regular.otf"));
    candidates.push(PathBuf::from(r"C:\Windows\Fonts\NotoSansCJK-Regular.ttc"));

    for path in candidates {
        if let Ok(bytes) = std::fs::read(&path) {
            return Some(bytes);
        }
    }
    None
}

fn load_chinese_font_bytes() -> Option<Vec<u8>> {
    let mut candidates = Vec::new();

    if let Some(windir) = env::var_os("WINDIR") {
        let font_dir = PathBuf::from(windir).join("Fonts");
        candidates.push(font_dir.join("msyh.ttc"));
        candidates.push(font_dir.join("msyh.ttf"));
        candidates.push(font_dir.join("simhei.ttf"));
        candidates.push(font_dir.join("simsun.ttc"));
    }

    candidates.push(PathBuf::from(r"C:\Windows\Fonts\msyh.ttc"));
    candidates.push(PathBuf::from(r"C:\Windows\Fonts\msyh.ttf"));
    candidates.push(PathBuf::from(r"C:\Windows\Fonts\simhei.ttf"));
    candidates.push(PathBuf::from(r"C:\Windows\Fonts\simsun.ttc"));

    for path in candidates {
        if let Ok(bytes) = std::fs::read(&path) {
            return Some(bytes);
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn load_app_file_description(process_path: &str) -> Option<String> {
    if process_path.trim().is_empty() {
        return None;
    }

    let path_wide = to_wide_null(process_path);
    let mut handle: u32 = 0;
    let version_size = unsafe { GetFileVersionInfoSizeW(path_wide.as_ptr(), &mut handle) };
    if version_size == 0 {
        return None;
    }

    let mut version_data = vec![0_u8; version_size as usize];
    let ok = unsafe {
        GetFileVersionInfoW(
            path_wide.as_ptr(),
            0,
            version_size,
            version_data.as_mut_ptr() as *mut c_void,
        )
    };
    if ok == 0 {
        return None;
    }

    let mut translation_pairs = load_version_translation_pairs(&version_data);
    // Common fallback locales when translation table is missing or incomplete.
    translation_pairs.push((0x0409, 0x04B0));
    translation_pairs.push((0x0804, 0x04B0));
    translation_pairs.sort_unstable();
    translation_pairs.dedup();

    for (lang, codepage) in translation_pairs {
        let query = format!(r"\StringFileInfo\{lang:04x}{codepage:04x}\FileDescription");
        if let Some(description) = query_version_string_value(&version_data, &query) {
            let trimmed = description.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }

    None
}

#[cfg(target_os = "windows")]
fn load_version_translation_pairs(version_data: &[u8]) -> Vec<(u16, u16)> {
    let query = to_wide_null(r"\VarFileInfo\Translation");
    let mut value_ptr: *mut c_void = std::ptr::null_mut();
    let mut value_len: u32 = 0;
    let ok = unsafe {
        VerQueryValueW(
            version_data.as_ptr() as *const c_void,
            query.as_ptr(),
            &mut value_ptr,
            &mut value_len,
        )
    };
    if ok == 0 || value_ptr.is_null() || value_len < 4 {
        return Vec::new();
    }

    let word_count = (value_len as usize) / 2;
    if word_count < 2 {
        return Vec::new();
    }
    let words = unsafe { std::slice::from_raw_parts(value_ptr as *const u16, word_count) };
    let pair_count = words.len() / 2;
    let mut pairs = Vec::with_capacity(pair_count);
    for i in 0..pair_count {
        pairs.push((words[i * 2], words[i * 2 + 1]));
    }
    pairs
}

#[cfg(target_os = "windows")]
fn query_version_string_value(version_data: &[u8], query: &str) -> Option<String> {
    let query_wide = to_wide_null(query);
    let mut value_ptr: *mut c_void = std::ptr::null_mut();
    let mut value_len: u32 = 0;
    let ok = unsafe {
        VerQueryValueW(
            version_data.as_ptr() as *const c_void,
            query_wide.as_ptr(),
            &mut value_ptr,
            &mut value_len,
        )
    };
    if ok == 0 || value_ptr.is_null() || value_len == 0 {
        return None;
    }

    let wide = unsafe { std::slice::from_raw_parts(value_ptr as *const u16, value_len as usize) };
    let end = wide.iter().position(|&ch| ch == 0).unwrap_or(wide.len());
    if end == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&wide[..end]))
}

#[cfg(not(target_os = "windows"))]
fn load_app_file_description(_process_path: &str) -> Option<String> {
    None
}

#[cfg(target_os = "windows")]
fn to_wide_null(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(target_os = "windows")]
fn load_app_icon_image(process_path: &str) -> Option<egui::ColorImage> {
    let mut wide: Vec<u16> = process_path.encode_utf16().collect();
    wide.push(0);

    let mut large_icons: [HICON; 1] = [std::ptr::null_mut(); 1];
    let mut small_icons: [HICON; 1] = [std::ptr::null_mut(); 1];

    let extracted = unsafe {
        ExtractIconExW(
            wide.as_ptr(),
            0,
            large_icons.as_mut_ptr(),
            small_icons.as_mut_ptr(),
            1,
        )
    };

    if extracted == 0 {
        return None;
    }

    let chosen_icon = if !small_icons[0].is_null() {
        small_icons[0]
    } else {
        large_icons[0]
    };

    let image = if chosen_icon.is_null() {
        None
    } else {
        hicon_to_color_image(chosen_icon)
    };

    unsafe {
        if !small_icons[0].is_null() {
            DestroyIcon(small_icons[0]);
        }
        if !large_icons[0].is_null() && large_icons[0] != small_icons[0] {
            DestroyIcon(large_icons[0]);
        }
    }

    image
}

#[cfg(not(target_os = "windows"))]
fn load_app_icon_image(_process_path: &str) -> Option<egui::ColorImage> {
    None
}

#[cfg(target_os = "windows")]
fn hicon_to_color_image(icon: HICON) -> Option<egui::ColorImage> {
    let mut icon_info: ICONINFO = unsafe { std::mem::zeroed() };
    let got_icon = unsafe { GetIconInfo(icon, &mut icon_info) };
    if got_icon == 0 {
        return None;
    }

    let bitmap = if !icon_info.hbmColor.is_null() {
        icon_info.hbmColor
    } else {
        icon_info.hbmMask
    };

    if bitmap.is_null() {
        cleanup_icon_info(&icon_info);
        return None;
    }

    let mut bmp: BITMAP = unsafe { std::mem::zeroed() };
    let got_obj = unsafe {
        GetObjectW(
            bitmap as _,
            std::mem::size_of::<BITMAP>() as i32,
            &mut bmp as *mut _ as *mut c_void,
        )
    };
    if got_obj == 0 {
        cleanup_icon_info(&icon_info);
        return None;
    }

    let width = bmp.bmWidth.max(1);
    let mut height = bmp.bmHeight.abs().max(1);
    if icon_info.hbmColor.is_null() {
        height /= 2;
    }
    if height <= 0 {
        cleanup_icon_info(&icon_info);
        return None;
    }

    let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
    bmi.bmiHeader.biSize = std::mem::size_of_val(&bmi.bmiHeader) as u32;
    bmi.bmiHeader.biWidth = width;
    bmi.bmiHeader.biHeight = -height;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB;

    let pixel_count = (width as usize).saturating_mul(height as usize);
    let mut bgra = vec![0_u8; pixel_count.saturating_mul(4)];

    let dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
    if dc.is_null() {
        cleanup_icon_info(&icon_info);
        return None;
    }

    let got_bits = unsafe {
        GetDIBits(
            dc,
            bitmap,
            0,
            height as u32,
            bgra.as_mut_ptr() as *mut c_void,
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };

    unsafe {
        DeleteDC(dc);
    }

    if got_bits == 0 {
        cleanup_icon_info(&icon_info);
        return None;
    }

    cleanup_icon_info(&icon_info);

    let has_alpha = bgra.chunks_exact(4).any(|px| px[3] != 0);
    let mut rgba = Vec::with_capacity(bgra.len());
    for px in bgra.chunks_exact(4) {
        rgba.push(px[2]);
        rgba.push(px[1]);
        rgba.push(px[0]);
        rgba.push(if has_alpha { px[3] } else { 255 });
    }

    Some(egui::ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        &rgba,
    ))
}

#[cfg(target_os = "windows")]
fn cleanup_icon_info(icon_info: &ICONINFO) {
    unsafe {
        if !icon_info.hbmColor.is_null() {
            DeleteObject(icon_info.hbmColor as _);
        }
        if !icon_info.hbmMask.is_null() {
            DeleteObject(icon_info.hbmMask as _);
        }
    }
}

fn parse_db_path_from_args() -> Result<PathBuf> {
    let mut db_path = default_db_path();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => {
                let value = args.next().context("missing value for --db")?;
                db_path = PathBuf::from(value);
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            _ => return Err(anyhow!("unknown argument: {arg}")),
        }
    }
    Ok(db_path)
}

fn default_db_path() -> PathBuf {
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local)
            .join("LimeTrace")
            .join("tracker.db");
    }
    PathBuf::from("data").join("tracker.db")
}

fn print_help() {
    println!(
        "\
LimeTrace

Usage:
  limetrace [--db <path>]

Options:
  --db         SQLite file path (default: %LOCALAPPDATA%\\LimeTrace\\tracker.db)
  -h, --help   Print this help"
    );
}

fn main() -> Result<()> {
    let db_path = parse_db_path_from_args()?;
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 720.0])
        .with_min_inner_size([980.0, 640.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(APP_ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "LimeTrace",
        native_options,
        Box::new(move |cc| {
            configure_chinese_fonts(&cc.egui_ctx);
            configure_interaction_style(&cc.egui_ctx);
            cc.egui_ctx.set_theme(egui::ThemePreference::Light);
            Ok(Box::new(TimelineApp::new(db_path.clone())))
        }),
    )
    .map_err(|err| anyhow!("failed to start LimeTrace: {err}"))
}

