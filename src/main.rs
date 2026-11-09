#![windows_subsystem = "windows"]

mod alpha;
mod optimize;
mod system;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Instant;

use eframe::egui::{self, Color32, RichText, Sense};
use egui_extras::{Column, TableBuilder};
use elegance::{Accent, BadgeTone, Button, ButtonSize, Card, Checkbox, ProgressBar, Select, Switch, Theme};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use rayon::prelude::*;

use optimize::{Level, Outcome, Settings};

const COLOR_PRESETS: [u32; 15] = [
    0,
    // Auto quality targets on pngquant's 0–100 scale.
    optimize::AUTO_BASE + 85,
    optimize::AUTO_BASE + 65,
    optimize::AUTO_BASE + 40,
    256, 192, 128, 96, 64, 48, 32, 16, 8, 4, 2,
];
const PREFS_KEY: &str = "prefs";
const SWITCH_LABELS: [&str; 4] = ["Overwrite originals", "Dithering", "Preserve date/time", "Include subfolders"];
/// Switch track plus the gap before its label.
const SWITCH_TRACK: f32 = 52.0;
const SWITCH_GAP: f32 = 16.0;
/// Natural width of the Colors + Compression selects.
const SELECTS_W: f32 = 210.0 + 12.0 + 260.0;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("PNG Optimizer")
            .with_inner_size([1000.0, 640.0])
            .with_min_inner_size([560.0, 560.0])
            .with_drag_and_drop(true)
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native("PNG Optimizer", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

/// Window icon: the largest PNG entry of the embedded .ico.
fn app_icon() -> egui::IconData {
    let ico = include_bytes!("../assets/app.ico");
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    let best = (0..count)
        .map(|i| {
            let e = &ico[6 + i * 16..6 + (i + 1) * 16];
            let size = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as usize;
            let offset = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as usize;
            (if e[0] == 0 { 256 } else { e[0] as u32 }, offset, size)
        })
        .max_by_key(|&(dim, ..)| dim);
    if let Some((_, offset, size)) = best
        && let Ok(bmp) = lodepng::decode32(&ico[offset..offset + size])
    {
        return egui::IconData {
            rgba: bmp.buffer.iter().flat_map(|p| [p.r, p.g, p.b, p.a]).collect(),
            width: bmp.width as u32,
            height: bmp.height as u32,
        };
    }
    egui::IconData::default()
}

/// Segoe UI (the Windows system font) as the primary face. Uses add_font, not set_fonts,
/// so the theme's symbol font and egui's fallbacks stay registered.
fn install_fonts(ctx: &egui::Context) {
    let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\segoeui.ttf") else { return };
    ctx.add_font(egui::epaint::text::FontInsert::new(
        "segoe",
        egui::FontData::from_owned(bytes),
        vec![egui::epaint::text::InsertFontFamily {
            family: egui::FontFamily::Proportional,
            priority: egui::epaint::text::FontPriority::Highest,
        }],
    ));
}

fn system_is_dark(ctx: &egui::Context) -> bool {
    ctx.system_theme().is_none_or(|t| t == egui::Theme::Dark)
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)] // settings saved by older versions lack newer fields
struct Prefs {
    colors: u32,
    level: Level,
    dither: bool,
    keep_date: bool,
    subfolders: bool,
    overwrite: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        // First-run defaults: safe (originals kept) and adaptive (Auto picks colors per file).
        Self {
            colors: optimize::AUTO_BASE + 85, // Auto (high quality)
            level: Level::Maximum,
            dither: true,
            keep_date: false,
            subfolders: true,
            overwrite: false,
        }
    }
}

enum Status {
    Idle,
    Queued,
    Working,
    Done(Outcome),
    Cancelled,
    Error(String),
}

struct Row {
    path: PathBuf,
    checked: bool,
    selected: bool,
    size_in: u64,
    status: Status,
}

enum Msg {
    Started(usize),
    Finished(usize, Result<Outcome, String>),
    Cancelled(usize),
    AllDone,
}

struct Run {
    rx: mpsc::Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    total: usize,
    done: usize,
    started: Instant,
}

struct App {
    prefs: Prefs,
    rows: Vec<Row>,
    run: Option<Run>,
    last_summary: Option<String>,
    accent: system::Accent,
    /// Light/dark state the title bar was last painted for.
    title_bar_dark: Option<bool>,
    /// Real width of the switches laid out in one row (measured once it has been drawn that way).
    switches_w: Option<f32>,
    /// Row a Shift+click range starts from (the last plain or Ctrl click), like in Explorer.
    anchor: Option<usize>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let accent = system::Accent::read();
        system::theme(system_is_dark(&cc.egui_ctx), accent).install(&cc.egui_ctx);
        install_fonts(&cc.egui_ctx);
        let prefs = cc.storage.and_then(|s| eframe::get_value(s, PREFS_KEY)).unwrap_or_default();
        let mut app = Self { prefs, rows: Vec::new(), run: None, last_summary: None, accent, title_bar_dark: None, switches_w: None, anchor: None };
        let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
        app.add_paths(args);
        app
    }

    fn busy(&self) -> bool {
        self.run.is_some()
    }

    fn add_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        if self.busy() {
            return;
        }
        let mut known: std::collections::HashSet<String> = self.rows.iter().map(|r| key(&r.path)).collect();
        let mut found = Vec::new();
        for p in paths {
            if p.is_dir() {
                collect_images(&p, self.prefs.subfolders, &mut found);
            } else if is_supported(&p) {
                found.push(p);
            }
        }
        for p in found {
            let p = std::path::absolute(&p).unwrap_or(p);
            if known.insert(key(&p)) {
                let size_in = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                self.rows.push(Row { path: p, checked: true, selected: false, size_in, status: Status::Idle });
            }
        }
        self.last_summary = None;
    }

    fn pick_files(&mut self) {
        if let Some(files) = rfd::FileDialog::new().add_filter("Images", &optimize::EXTENSIONS).pick_files() {
            self.add_paths(files);
        }
    }

    fn pick_folder(&mut self) {
        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
            self.add_paths([dir]);
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        let jobs: Vec<(usize, PathBuf)> =
            self.rows.iter().enumerate().filter(|(_, r)| r.checked).map(|(i, r)| (i, r.path.clone())).collect();
        if jobs.is_empty() {
            return;
        }
        for &(i, _) in &jobs {
            let r = &mut self.rows[i];
            r.size_in = std::fs::metadata(&r.path).map(|m| m.len()).unwrap_or(r.size_in);
            r.status = Status::Queued;
        }

        let settings = Settings {
            colors: self.prefs.colors,
            dither: self.prefs.dither,
            level: self.prefs.level,
            keep_date: self.prefs.keep_date,
            overwrite: self.prefs.overwrite,
        };
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let total = jobs.len();
        {
            let cancel = cancel.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                jobs.into_par_iter().for_each_with(tx.clone(), |tx, (i, path)| {
                    if cancel.load(Ordering::Relaxed) {
                        let _ = tx.send(Msg::Cancelled(i));
                    } else {
                        let _ = tx.send(Msg::Started(i));
                        ctx.request_repaint();
                        let _ = tx.send(Msg::Finished(i, optimize::process(&path, &settings)));
                    }
                    ctx.request_repaint();
                });
                let _ = tx.send(Msg::AllDone);
                ctx.request_repaint();
            });
        }
        self.run = Some(Run { rx, cancel, total, done: 0, started: Instant::now() });
        self.last_summary = None;
    }

    fn poll(&mut self) {
        let Some(run) = &mut self.run else { return };
        let mut finished = false;
        while let Ok(msg) = run.rx.try_recv() {
            match msg {
                Msg::Started(i) => self.rows[i].status = Status::Working,
                Msg::Finished(i, r) => {
                    run.done += 1;
                    self.rows[i].status = match r {
                        Ok(o) => Status::Done(o),
                        Err(e) => Status::Error(e),
                    };
                }
                Msg::Cancelled(i) => {
                    run.done += 1;
                    self.rows[i].status = Status::Cancelled;
                }
                Msg::AllDone => finished = true,
            }
        }
        if finished {
            let secs = run.started.elapsed().as_secs();
            self.last_summary = Some(format!("Done in {:02}:{:02}", secs / 60, secs % 60));
            self.run = None;
        }
    }

    fn remove_selected(&mut self) {
        if !self.busy() {
            self.rows.retain(|r| !r.selected);
            self.anchor = None;
        }
    }

    fn clear(&mut self) {
        if !self.busy() {
            self.rows.clear();
            self.anchor = None;
            self.last_summary = None;
        }
    }

    /// (bytes before, bytes after) over finished files.
    fn totals(&self) -> Option<(u64, u64)> {
        let mut t: Option<(u64, u64)> = None;
        for r in &self.rows {
            let (b, a) = match &r.status {
                Status::Done(Outcome::Optimized { before, after, .. }) => (*before, *after),
                Status::Done(Outcome::AlreadyOptimal { size, .. } | Outcome::Skipped { size, .. }) => (*size, *size),
                _ => continue,
            };
            let (tb, ta) = t.get_or_insert((0, 0));
            *tb += b;
            *ta += a;
        }
        t
    }

    // ---------------- UI sections ----------------

    /// Settings card, adapting to the window width instead of forcing a wide minimum:
    /// selects + Optimize share the first row and the switches go below (in two rows if needed);
    /// when even the selects and the button don't fit side by side, the button drops to full width.
    fn settings_card(&mut self, ui: &mut egui::Ui) {
        const BUTTON_W: f32 = 150.0;
        const GAP: f32 = 24.0;
        Card::new().padding(16).show(ui, |ui| {
            let avail = ui.available_width();
            if avail >= SELECTS_W + GAP + BUTTON_W {
                // A plain horizontal row is only as tall as the selects, so the button can sit on its bottom edge.
                ui.horizontal(|ui| {
                    self.selects_ui(ui, SELECTS_W);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| {
                        self.optimize_button(ui, BUTTON_W);
                    });
                });
                ui.add_space(14.0);
                self.switches_ui(ui, avail);
            } else {
                self.selects_ui(ui, avail);
                ui.add_space(14.0);
                self.switches_ui(ui, avail);
                ui.add_space(14.0);
                self.optimize_button(ui, avail);
            }
        });
    }

    fn selects_ui(&mut self, ui: &mut egui::Ui, width: f32) {
        let busy = self.busy();
        // Selects shrink proportionally when there is less room than their natural width.
        let k = (width / SELECTS_W).min(1.0);
        ui.horizontal_top(|ui| {
            ui.add(
                Select::new("colors", &mut self.prefs.colors)
                    .label("Colors")
                    .options(COLOR_PRESETS.map(|c| (c, colors_label(c))))
                    .width(210.0 * k)
                    .enabled(!busy),
            );
            ui.add_space(12.0 * k);
            ui.add(
                Select::new("level", &mut self.prefs.level)
                    .label("Compression")
                    .options(Level::ALL.map(|l| (l, l.label())))
                    .width(260.0 * k)
                    .enabled(!busy),
            );
        });
    }

    /// Switches in one row when they fit, else two. Switches stay live while busy
    /// (a disabled switch renders as "off"); settings are captured when a run starts.
    fn switches_ui(&mut self, ui: &mut egui::Ui, width: f32) {
        let theme = Theme::current(ui.ctx());
        let estimate = {
            let font = egui::FontId::proportional(theme.typography.body);
            SWITCH_LABELS
                .iter()
                .map(|l| ui.painter().layout_no_wrap(l.to_string(), font.clone(), Color32::WHITE).size().x + SWITCH_TRACK)
                .sum::<f32>()
                + SWITCH_GAP * (SWITCH_LABELS.len() - 1) as f32
        };
        let per_row = if width >= self.switches_w.unwrap_or(estimate) { SWITCH_LABELS.len() } else { 2 };

        // Real one-row width = sum of the drawn rows + the gaps between them; drives the next frame's layout.
        let (mut drawn_w, mut rows) = (0.0, 0);
        {
            let p = &mut self.prefs;
            let mut switches = [&mut p.overwrite, &mut p.dither, &mut p.keep_date, &mut p.subfolders]
                .into_iter()
                .zip(SWITCH_LABELS);
            loop {
                let row: Vec<_> = switches.by_ref().take(per_row).collect();
                if row.is_empty() {
                    break;
                }
                let resp = ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = SWITCH_GAP;
                    let left = ui.min_rect().left();
                    let mut right = left;
                    for (value, label) in row {
                        right = ui.add(Switch::new(value, label)).rect.right();
                    }
                    right - left
                });
                drawn_w += resp.inner;
                rows += 1;
            }
        }
        self.switches_w = Some(drawn_w + SWITCH_GAP * (rows - 1) as f32);

        if !self.prefs.overwrite {
            ui.add_space(6.0);
            ui.label(theme.faint_text("Originals are kept; optimized copies are saved next to them as name_opt.png"));
        }
    }
    fn optimize_button(&mut self, ui: &mut egui::Ui, width: f32) {
        let busy = self.busy();
        let theme = Theme::current(ui.ctx());
        let checked = self.rows.iter().filter(|r| r.checked).count();
        let (text, accent) = if busy { ("Stop", Accent::Red) } else { ("Optimize", Accent::Blue) };
        let btn = Button::new(RichText::new(text).size(theme.typography.button + 3.0).strong())
            .accent(accent)
            .size(ButtonSize::Large)
            .min_width(width)
            .enabled(busy || checked > 0);
        if ui.add(btn).clicked() {
            match &self.run {
                Some(run) => run.cancel.store(true, Ordering::Relaxed),
                None => self.start(ui.ctx()),
            }
        }
    }
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy();
        let theme = Theme::current(ui.ctx());
        ui.horizontal(|ui| {
            let small = |t: &str| Button::new(t).outline().size(ButtonSize::Small).enabled(!busy);
            if ui.add(small("Add files…")).clicked() {
                self.pick_files();
            }
            if ui.add(small("Add folder…")).clicked() {
                self.pick_folder();
            }
            let any_selected = self.rows.iter().any(|r| r.selected);
            if ui.add(small("Remove selected").enabled(!busy && any_selected)).clicked() {
                self.remove_selected();
            }
            if ui.add(small("Clear").enabled(!busy && !self.rows.is_empty())).clicked() {
                self.clear();
            }

            // Right side: totals. On narrow windows the byte counts go first, then the pill,
            // so nothing ever overlaps the buttons.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let font = egui::FontId::proportional(theme.typography.body);
                let text_w = |ui: &egui::Ui, t: &str| ui.painter().layout_no_wrap(t.to_owned(), font.clone(), Color32::WHITE).size().x;
                let gap = ui.spacing().item_spacing.x;
                if let Some((before, after)) = self.totals()
                    && before > after
                {
                    let saved = format!("−{:.1}%", 100.0 - 100.0 * after as f64 / before as f64);
                    let bytes = format!("{} → {}", fmt_size(before), fmt_size(after));
                    let size = pill_size(ui, &saved);
                    if ui.available_width() >= size.x + gap {
                        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
                        pill(ui, rect.left_center(), &saved, BadgeTone::Ok);
                        if ui.available_width() >= text_w(ui, &bytes) + 2.0 * gap {
                            ui.label(theme.muted_text(bytes));
                        }
                    }
                } else if !self.rows.is_empty() {
                    let n = self.rows.len();
                    let files = format!("{n} file{}", if n == 1 { "" } else { "s" });
                    if ui.available_width() >= text_w(ui, &files) + gap {
                        ui.label(theme.muted_text(files));
                    }
                }
            });
        });
    }

    fn file_table(&mut self, ui: &mut egui::Ui) {
        let theme = Theme::current(ui.ctx());
        let p = theme.palette;
        let row_h = 36.0;
        let busy = self.busy();
        let mut clicked: Option<(usize, egui::Modifiers)> = None;
        let mut explorer: Option<usize> = None;
        let mut remove: Option<usize> = None;

        Card::new().padding(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            // Every cell is painted relative to its own vertical center, so text, numbers,
            // checkboxes and badges share one baseline regardless of widget internals.
            // Narrow windows drop the "In" column and tighten the rest, so the table always fits.
            let compact = ui.available_width() < 720.0;
            let mut table = TableBuilder::new(ui)
                .id_salt(if compact { "files_compact" } else { "files" })
                .striped(false)
                .resizable(false)
                .sense(Sense::click())
                .column(Column::exact(26.0))
                .column(Column::remainder().at_least(100.0).clip(true));
            table = if compact {
                table.column(Column::exact(84.0)).column(Column::exact(64.0)).column(Column::exact(128.0).clip(true))
            } else {
                table
                    .column(Column::exact(100.0))
                    .column(Column::exact(100.0))
                    .column(Column::exact(80.0))
                    .column(Column::exact(80.0))
                    .column(Column::exact(150.0).clip(true))
            };
            table
                .header(32.0, |mut h| {
                    let head = egui::FontId::proportional(theme.typography.small);
                    let label = |ui: &mut egui::Ui, t: &str, right: bool| {
                        let r = ui.max_rect();
                        let (pos, align) = if right {
                            (egui::pos2(r.right() - CELL_PAD, r.center().y), egui::Align2::RIGHT_CENTER)
                        } else {
                            (egui::pos2(r.left() + CELL_PAD, r.center().y), egui::Align2::LEFT_CENTER)
                        };
                        ui.painter().text(pos, align, t.to_uppercase(), head.clone(), p.text_faint);
                    };
                    let label_at = |ui: &mut egui::Ui, t: &str, x: f32| {
                        let r = ui.max_rect();
                        let pos = egui::pos2(r.left() + x, r.center().y);
                        ui.painter().text(pos, egui::Align2::LEFT_CENTER, t.to_uppercase(), head.clone(), p.text_faint);
                    };
                    h.col(|_| {});
                    h.col(|ui| label_at(ui, "File", 0.0));
                    if !compact {
                        h.col(|ui| label(ui, "In", true));
                    }
                    h.col(|ui| label(ui, "Out", true));
                    h.col(|ui| label(ui, "Ratio", true));
                    if !compact {
                        h.col(|ui| label(ui, "Colors", true));
                    }
                    h.col(|ui| label(ui, "Status", false));
                })
                .body(|body| {
                    body.rows(row_h, self.rows.len(), |mut row| {
                        let i = row.index();
                        let r = &mut self.rows[i];
                        row.set_selected(r.selected);
                        let body_font = egui::FontId::proportional(theme.typography.body);
                        let (out, ratio) = match &r.status {
                            Status::Done(Outcome::Optimized { before, after, .. }) => {
                                (fmt_size(*after), format!("{:.1}%", 100.0 * *after as f64 / *before as f64))
                            }
                            Status::Done(Outcome::AlreadyOptimal { size, .. } | Outcome::Skipped { size, .. }) => {
                                (fmt_size(*size), "100.0%".into())
                            }
                            _ => ("—".into(), "—".into()),
                        };

                        row.col(|ui| {
                            let cell = ui.max_rect();
                            let box_rect = egui::Rect::from_center_size(
                                egui::pos2(cell.left() + CELL_PAD + 9.0, cell.center().y),
                                egui::vec2(18.0, 18.0),
                            );
                            ui.add_enabled_ui(!busy, |ui| ui.put(box_rect, Checkbox::new(&mut r.checked, "")));
                        });
                        row.col(|ui| {
                            let cell = ui.max_rect();
                            let full = r.path.display().to_string();
                            let shown = compact_path(ui, &full, cell.width() - CELL_PAD, &body_font);
                            let (dir, file) = split_path(&shown);
                            let mut job = egui::text::LayoutJob::default();
                            job.append(&dir, 0.0, egui::TextFormat::simple(body_font.clone(), p.text_muted));
                            job.append(&file, 0.0, egui::TextFormat::simple(body_font.clone(), p.text));
                            let galley = ui.painter().layout_job(job);
                            let pos = egui::pos2(cell.left(), cell.center().y - galley.size().y / 2.0);
                            let hover = egui::Rect::from_min_size(pos, galley.size());
                            ui.painter().galley(pos, galley, p.text);
                            ui.interact(hover, ui.id().with(("path", i)), Sense::hover()).on_hover_text(full);
                        });
                        let good = matches!(r.status, Status::Done(Outcome::Optimized { before, after, .. }) if after < before);
                        let mut cells = vec![
                            (fmt_size(r.size_in), p.text_muted),
                            (out, p.text_muted),
                            (ratio, if good { p.success } else { p.text_muted }),
                        ];
                        let colors = match &r.status {
                            Status::Done(
                                Outcome::Optimized { colors, .. }
                                | Outcome::AlreadyOptimal { colors, .. }
                                | Outcome::Skipped { colors, .. },
                            ) => fmt_count(*colors),
                            _ => "—".into(),
                        };
                        if compact {
                            cells.remove(0); // no "In" column
                        } else {
                            cells.push((colors, p.text_muted));
                        }
                        for (text, color) in cells {
                            row.col(|ui| {
                                let cell = ui.max_rect();
                                ui.painter().text(
                                    egui::pos2(cell.right() - CELL_PAD, cell.center().y),
                                    egui::Align2::RIGHT_CENTER,
                                    text,
                                    body_font.clone(),
                                    color,
                                );
                            });
                        }
                        row.col(|ui| {
                            let cell = ui.max_rect();
                            if let Some((label, tone)) = status_badge(&r.status) {
                                let rect = pill(ui, egui::pos2(cell.left() + CELL_PAD, cell.center().y), label, tone);
                                if let Some(why) = status_hint(&r.status) {
                                    ui.interact(rect, ui.id().with(("status", i)), Sense::hover()).on_hover_text(why);
                                }
                            }
                        });

                        let resp = row.response();
                        if resp.clicked() {
                            clicked = Some((i, resp.ctx.input(|inp| inp.modifiers)));
                        }
                        if resp.double_clicked() {
                            explorer = Some(i);
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Show in Explorer").clicked() {
                                explorer = Some(i);
                                ui.close();
                            }
                            if ui.add_enabled(!busy, egui::Button::new("Remove from list")).clicked() {
                                remove = Some(i);
                                ui.close();
                            }
                        });
                    });
                });
        });
        if let Some((i, mods)) = clicked {
            self.click_select(i, mods);
        }
        if let Some(i) = explorer {
            let r = &self.rows[i];
            match &r.status {
                Status::Done(Outcome::Optimized { output, .. }) => show_in_explorer(output),
                _ => show_in_explorer(&r.path),
            }
        }
        if let Some(i) = remove
            && !busy
        {
            self.rows.remove(i);
            self.anchor = None;
        }
    }

    fn click_select(&mut self, i: usize, mods: egui::Modifiers) {
        let mut selected: Vec<bool> = self.rows.iter().map(|r| r.selected).collect();
        self.anchor = apply_click(&mut selected, self.anchor, i, mods.shift, mods.ctrl);
        for (r, s) in self.rows.iter_mut().zip(selected) {
            r.selected = s;
        }
    }
    fn status_bar(&self, ui: &mut egui::Ui) {
        let theme = Theme::current(ui.ctx());
        ui.horizontal(|ui| {
            ui.set_min_height(20.0);
            let text = match (&self.run, &self.last_summary) {
                (Some(run), _) => format!("Processing {} of {}…", run.done.min(run.total), run.total),
                (None, Some(s)) => s.clone(),
                (None, None) if self.rows.is_empty() => "Ready".into(),
                (None, None) => format!("{} of {} files checked", self.rows.iter().filter(|r| r.checked).count(), self.rows.len()),
            };
            ui.label(theme.muted_text(text));
            if let Some(run) = &self.run {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(
                        ProgressBar::new(run.done as f32 / run.total.max(1) as f32)
                            .desired_width(240.0)
                            .accent(Accent::Blue)
                            .text(format!("{} / {}", run.done, run.total)),
                    );
                });
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let dark = system_is_dark(&ctx);
        if self.title_bar_dark != Some(dark) {
            // Light/dark switched (or first frame): re-read the accent and repaint the title bar.
            self.accent = system::Accent::read();
            let p = system::theme(dark, self.accent).palette;
            if let Ok(handle) = frame.window_handle()
                && let RawWindowHandle::Win32(w) = handle.as_raw()
            {
                system::style_title_bar(w.hwnd.get(), dark, p.bg, p.text);
            }
            self.title_bar_dark = Some(dark);
        }
        system::theme(dark, self.accent).install(&ctx);
        self.poll();

        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            self.add_paths(dropped);
        }
        if !ctx.egui_wants_keyboard_input() {
            if ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
                self.remove_selected();
            }
            if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::A)) {
                self.rows.iter_mut().for_each(|r| r.selected = true);
            }
        }

        let bg = Theme::current(&ctx).palette.bg;
        // One 12 px rhythm between blocks; less on top, where the title bar already blends in.
        let margin = egui::Margin { left: 16, right: 16, top: 8, bottom: 0 };

        egui::Panel::top("top").show_separator_line(false).frame(egui::Frame::new().fill(bg).inner_margin(margin)).show(ui, |ui| {
            self.settings_card(ui);
            ui.add_space(12.0);
            self.toolbar(ui);
        });

        egui::Panel::bottom("status")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(bg).inner_margin(egui::Margin { left: 16, right: 16, top: 0, bottom: 12 }))
            .show(ui, |ui| self.status_bar(ui));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(bg).inner_margin(egui::Margin { left: 16, right: 16, top: 12, bottom: 12 }))
            .show(ui, |ui| {
                if self.rows.is_empty() {
                    if drop_zone(ui, !self.busy()).clicked() {
                        self.pick_files();
                    }
                } else {
                    self.file_table(ui);
                    if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
                        let blue = Theme::current(&ctx).palette.blue;
                        ui.painter().rect_stroke(
                            ui.max_rect().shrink(2.0),
                            10.0,
                            egui::Stroke::new(2.0, blue),
                            egui::StrokeKind::Inside,
                        );
                    }
                }
            });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, PREFS_KEY, &self.prefs);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }
}

// ---------------- helpers ----------------

/// Explorer-style selection. Click = only this row, Ctrl = toggle it, Shift = range from the anchor,
/// Ctrl+Shift = add that range. Returns the new anchor (Shift clicks keep the old one).
fn apply_click(selected: &mut [bool], anchor: Option<usize>, i: usize, shift: bool, ctrl: bool) -> Option<usize> {
    match (shift, ctrl, anchor.filter(|&a| a < selected.len())) {
        (true, ctrl, Some(a)) => {
            let range = a.min(i)..=a.max(i);
            for (j, s) in selected.iter_mut().enumerate() {
                if range.contains(&j) {
                    *s = true;
                } else if !ctrl {
                    *s = false;
                }
            }
            Some(a)
        }
        (_, true, _) => {
            selected[i] ^= true;
            Some(i)
        }
        _ => {
            for (j, s) in selected.iter_mut().enumerate() {
                *s = j == i;
            }
            Some(i)
        }
    }
}

#[cfg(test)]
mod selection_tests {
    use super::apply_click;

    fn sel(s: &[bool]) -> String {
        s.iter().map(|&b| if b { 'x' } else { '.' }).collect()
    }

    #[test]
    fn first_run_defaults() {
        let p = super::Prefs::default();
        assert_eq!(super::colors_label(p.colors), "Auto (high quality)");
        assert!(p.level == super::Level::Maximum);
        assert!(!p.overwrite && p.dither && !p.keep_date && p.subfolders);
    }

    #[test]
    fn explorer_like_selection() {
        let mut s = vec![false; 8];
        let a = apply_click(&mut s, None, 2, false, false); // click row 2
        assert_eq!(sel(&s), "..x.....");
        let a = apply_click(&mut s, a, 5, true, false); // shift-click row 5
        assert_eq!(sel(&s), "..xxxx..");
        let a = apply_click(&mut s, a, 0, true, false); // shift-click row 0: re-pivots around 2
        assert_eq!(sel(&s), "xxx.....");
        let a = apply_click(&mut s, a, 6, false, true); // ctrl-click row 6
        assert_eq!(sel(&s), "xxx...x.");
        let a = apply_click(&mut s, a, 7, true, true); // ctrl+shift-click row 7: adds 6..=7
        assert_eq!(sel(&s), "xxx...xx");
        apply_click(&mut s, a, 4, true, false); // shift-click row 4 from anchor 6: only 4..=6
        assert_eq!(sel(&s), "....xxx.");
        let mut s = vec![false; 3];
        apply_click(&mut s, None, 1, true, false); // shift without an anchor acts like a click
        assert_eq!(sel(&s), ".x.");
    }
}

/// Horizontal padding inside table cells.
const CELL_PAD: f32 = 8.0;


const PILL_PAD: egui::Vec2 = egui::vec2(10.0, 4.0);

fn pill_size(ui: &egui::Ui, label: &str) -> egui::Vec2 {
    let font = egui::FontId::proportional(Theme::current(ui.ctx()).typography.small);
    ui.painter().layout_no_wrap(label.to_uppercase(), font, Color32::WHITE).size() + 2.0 * PILL_PAD
}

/// Status pill painted with its vertical center on `left_center.y`. Returns its rect.
fn pill(ui: &egui::Ui, left_center: egui::Pos2, label: &str, tone: BadgeTone) -> egui::Rect {
    let theme = Theme::current(ui.ctx());
    let p = theme.palette;
    let fg = match tone {
        BadgeTone::Ok => p.success,
        BadgeTone::Warning => p.warning,
        BadgeTone::Danger => p.danger,
        BadgeTone::Info => p.focus,
        BadgeTone::Neutral => p.text_muted,
    };
    let galley = ui.painter().layout_no_wrap(label.to_uppercase(), egui::FontId::proportional(theme.typography.small), fg);
    let pad = PILL_PAD;
    let size = galley.size() + 2.0 * pad;
    let rect = egui::Rect::from_min_size(egui::pos2(left_center.x, left_center.y - size.y / 2.0), size);
    ui.painter().rect_filled(rect, size.y / 2.0, fg.gamma_multiply(0.16));
    ui.painter().galley(rect.min + pad, galley, fg);
    rect
}

/// Empty-state drop target: rounded card with a rounded dashed outline.
fn drop_zone(ui: &mut egui::Ui, enabled: bool) -> egui::Response {
    let theme = Theme::current(ui.ctx());
    let p = theme.palette;
    let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click());
    let resp = if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
    let dragover = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
    let hot = enabled && (dragover || resp.hovered());
    let radius = theme.card_radius;
    let painter = ui.painter();

    painter.rect_filled(rect, radius, if dragover { p.focus.gamma_multiply(0.10) } else { p.card });
    let outline = rounded_rect_outline(rect.shrink(0.75), radius);
    let border = if dragover { p.focus } else if hot { p.text_muted } else { p.border };
    painter.extend(egui::Shape::dashed_line(&outline, egui::Stroke::new(1.5, border), 7.0, 5.0));

    // Upload icon, prompt and hint as a centered stack. When the zone is short, drop the hint,
    // then the icon, so nothing is ever painted outside the zone.
    let prompt = if dragover { "Release to add" } else { "Drop images or folders here" };
    let prompt_font = egui::FontId::proportional(theme.typography.body + 2.0);
    let hint_font = egui::FontId::proportional(theme.typography.small);
    let (icon_h, prompt_h, hint_h) = (52.0 + 14.0, prompt_font.size + 6.0, 8.0 + hint_font.size + 4.0);
    let room = rect.height() - 16.0;
    let show_icon = room >= icon_h + prompt_h;
    let show_hint = room >= icon_h + prompt_h + hint_h;
    let stack_h = prompt_h + if show_icon { icon_h } else { 0.0 } + if show_hint { hint_h } else { 0.0 };
    let painter = painter.with_clip_rect(rect.shrink(2.0));
    let mut y = rect.center().y - stack_h / 2.0;

    if show_icon {
        let c = egui::pos2(rect.center().x, y + 26.0);
        let icon = if dragover { p.focus } else if hot { p.text } else { p.text_muted };
        painter.circle_filled(c, 26.0, p.input_bg);
        let s = egui::Stroke::new(2.0, icon);
        painter.line_segment([c + egui::vec2(0.0, 6.0), c + egui::vec2(0.0, -9.0)], s);
        painter.line_segment([c + egui::vec2(-6.0, -3.0), c + egui::vec2(0.0, -9.0)], s);
        painter.line_segment([c + egui::vec2(6.0, -3.0), c + egui::vec2(0.0, -9.0)], s);
        painter.line_segment([c + egui::vec2(-9.0, 4.0), c + egui::vec2(-9.0, 10.0)], s);
        painter.line_segment([c + egui::vec2(-9.0, 10.0), c + egui::vec2(9.0, 10.0)], s);
        painter.line_segment([c + egui::vec2(9.0, 10.0), c + egui::vec2(9.0, 4.0)], s);
        y += icon_h;
    }
    painter.text(
        egui::pos2(rect.center().x, y + prompt_h / 2.0),
        egui::Align2::CENTER_CENTER,
        prompt,
        prompt_font,
        if dragover { p.focus } else { p.text },
    );
    y += prompt_h;
    if show_hint {
        painter.text(
            egui::pos2(rect.center().x, y + hint_h / 2.0 + 4.0),
            egui::Align2::CENTER_CENTER,
            "or click to browse · PNG, JPG, GIF, WebP, BMP, TIFF, TGA, ICO — non-PNG files are saved as .png",
            hint_font,
            p.text_faint,
        );
    }    resp
}

/// Closed polyline around a rounded rectangle, clockwise from the top-left corner.
fn rounded_rect_outline(rect: egui::Rect, radius: f32) -> Vec<egui::Pos2> {
    let r = radius.min(rect.width() / 2.0).min(rect.height() / 2.0);
    let corners = [
        (egui::pos2(rect.left() + r, rect.top() + r), 180.0_f32),
        (egui::pos2(rect.right() - r, rect.top() + r), 270.0),
        (egui::pos2(rect.right() - r, rect.bottom() - r), 0.0),
        (egui::pos2(rect.left() + r, rect.bottom() - r), 90.0),
    ];
    let mut pts = Vec::with_capacity(4 * 9 + 1);
    for (center, start) in corners {
        for i in 0..=8 {
            let a = (start + 90.0 * i as f32 / 8.0).to_radians();
            pts.push(center + egui::vec2(a.cos(), a.sin()) * r);
        }
    }
    pts.push(pts[0]);
    pts
}

fn colors_label(c: u32) -> String {
    match optimize::auto_quality(c) {
        Some(80..) => "Auto (high quality)".into(),
        Some(60..) => "Auto (balanced)".into(),
        Some(_) => "Auto (smaller)".into(),
        None if c == 0 => "Lossless".into(),
        None => format!("{c} colors"),
    }
}

/// Explanation shown when hovering a status pill.
fn status_hint(s: &Status) -> Option<String> {
    Some(match s {
        Status::Error(e) => e.clone(),
        Status::Done(Outcome::Optimized { note, output, converted, first_frame, colors, quantized, .. }) => {
            let mut hint = format!("Saved to {}", output.display());
            hint += &if *quantized {
                format!("\n{colors} colors after quantization")
            } else {
                format!("\n{colors} colors (lossless)")
            };
            if *converted {
                hint += "\n\nConverted to PNG; the original file was kept.";
            }
            if *first_frame {
                hint += "\nThe source is animated — only its first frame was saved.";
            }
            if note.contains("lossless") {
                hint += "\n\nThe reduced-color result would have made transparent pixels visible, \
                         so the file was recompressed losslessly instead.";
            } else if note.contains("alpha") {
                hint += "\n\nSome fully transparent pixels were remapped back to transparent after quantization.";
            }
            hint
        }
        Status::Done(Outcome::Skipped { .. }) => "No result passed the transparency check; the file was left unchanged.".into(),
        Status::Done(Outcome::AlreadyOptimal { .. }) => "The optimized file was not smaller; the original was kept.".into(),
        _ => return None,
    })
}

fn status_badge(s: &Status) -> Option<(&'static str, BadgeTone)> {
    Some(match s {
        Status::Idle => return None,
        Status::Queued => ("Queued", BadgeTone::Neutral),
        Status::Working => ("Processing", BadgeTone::Info),
        Status::Done(Outcome::Optimized { first_frame: true, .. }) => ("First frame", BadgeTone::Warning),
        Status::Done(Outcome::Optimized { converted: true, .. }) => ("Converted", BadgeTone::Ok),
        Status::Done(Outcome::Optimized { note: "", .. }) => ("Optimized", BadgeTone::Ok),
        Status::Done(Outcome::Optimized { note, .. }) if note.contains("lossless") => ("Lossless", BadgeTone::Ok),
        Status::Done(Outcome::Optimized { .. }) => ("Alpha fixed", BadgeTone::Ok),
        Status::Done(Outcome::AlreadyOptimal { .. }) => ("Already optimal", BadgeTone::Neutral),
        Status::Done(Outcome::Skipped { .. }) => ("Skipped", BadgeTone::Warning),
        Status::Cancelled => ("Cancelled", BadgeTone::Neutral),
        Status::Error(_) => ("Error", BadgeTone::Danger),
    })
}

/// "12 345" — thousands grouped with a narrow no-break space.
fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('\u{202F}');
        }
        out.push(ch);
    }
    out
}

/// Human-readable size in binary units, like Explorer: 812 B, 23.7 KB, 1.95 MB.
fn fmt_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    // Three significant digits: 1.95 MB, 23.7 KB, 512 KB.
    let text = if v < 10.0 { format!("{v:.2}") } else if v < 100.0 { format!("{v:.1}") } else { format!("{v:.0}") };
    format!("{text} {}", UNITS[unit])
}

/// Splits "C:\…\dir\file.png" into ("C:\…\dir\", "file.png").
fn split_path(p: &str) -> (String, String) {
    match p.rfind('\\') {
        Some(i) => (p[..=i].to_owned(), p[i + 1..].to_owned()),
        None => (String::new(), p.to_owned()),
    }
}

/// Shortens a path from the middle so the file name stays visible: C:\…\folder\file.png
fn compact_path(ui: &egui::Ui, path: &str, width: f32, font: &egui::FontId) -> String {
    let fits = |s: &str| ui.painter().layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE).size().x <= width;
    if fits(path) {
        return path.to_owned();
    }
    let parts: Vec<&str> = path.split('\\').collect();
    if parts.len() < 3 {
        return path.to_owned();
    }
    let (root, file) = (parts[0], parts[parts.len() - 1]);
    for keep in (0..=parts.len() - 3).rev() {
        let mut s = vec![root, "…"];
        s.extend(&parts[parts.len() - 1 - keep..parts.len() - 1]);
        s.push(file);
        let s = s.join("\\");
        if fits(&s) {
            return s;
        }
    }
    format!("…\\{file}")
}

fn is_supported(p: &Path) -> bool {
    p.is_file()
        && p.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| optimize::EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

fn collect_images(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            if recursive {
                collect_images(&p, true, out);
            }
        } else if is_supported(&p) {
            out.push(p);
        }
    }
}

fn key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

fn show_in_explorer(p: &Path) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{}\"", p.display())).spawn();
}
