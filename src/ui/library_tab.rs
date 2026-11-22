//! The clip library (Clips tab), the Uploads list and the Streamable login.
//!
//! Clips from the clips folder and any imported folders ("collections"),
//! grouped by day, as cards: thumbnail, favourite star, length, the game it
//! came from, and Upload -> progress -> Copy link. Thumbnails and lengths
//! come from a worker thread (ffprobe/ffmpeg), cached on disk; only cards
//! on screen get a thumbnail texture, and old ones are dropped, so a big
//! library doesn't grow the window's memory.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Response, RichText, Sense, Shape, Stroke, TextureOptions, pos2, vec2};
use serde::{Deserialize, Serialize};

use super::widgets::{fmt_bytes, fmt_duration};
use super::{Shared, dim};
use crate::library::{ClipView, Host, Library, UploadRecord};
use crate::upload::{Request, Uploader, View, credentials, litterbox, streamable};
use crate::win::LocalTime;

/// Thumbnail textures kept at most (about 330 KB each).
const MAX_TEXTURES: usize = 64;
const THUMB_W: u32 = 384;
const CARD_MIN_W: f32 = 236.0;
/// Compact view cards: about 60% of a grid card.
const COMPACT_MIN_W: f32 = 142.0;
const LIST_ROW_H: f32 = 58.0;
const LIST_THUMB: egui::Vec2 = vec2(80.0, 45.0);
const GAP: f32 = 14.0;
/// A favourite's star.
const STAR: Color32 = Color32::from_rgb(255, 196, 0);
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

struct Clip {
    path: PathBuf,
    name: String,
    size: u64,
    /// Modified time, unix seconds (cache key for thumbnails/lengths).
    stamp: u64,
    when: LocalTime,
    day: i64,
    /// "Clip" / "Recording" for Snag's own files, else "".
    kind: &'static str,
    /// "Trimmed" / "Edited" for the trimmer's and editor's files.
    tag: Option<&'static str>,
    /// Index into `Library::collections`; None = the clips folder.
    collection: Option<usize>,
}

/// A clip offered in the editor's "Add clip" list.
pub struct ClipChoice {
    pub path: PathBuf,
    /// The game (title tag), else the file name.
    pub name: String,
    /// "Yesterday 19:09 · 2:01 · 261 MB".
    pub detail: String,
    pub thumb: Option<egui::TextureHandle>,
}

/// Length and game of a clip (from ffprobe), cached across runs.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Meta {
    stamp: u64,
    #[serde(default)]
    size: u64,
    seconds: Option<f64>,
    title: Option<String>,
}

enum Slot {
    Pending,
    Ready(egui::TextureHandle, u64),
    Failed,
}

/// (modified time, size): a file's identity for the caches. Trimming keeps
/// a clip's date, so the size is what tells the new file apart.
type Ident = (u64, u64);

enum Req {
    Meta(PathBuf),
    Thumb(PathBuf, Ident),
}

enum Res {
    Meta(PathBuf, Meta),
    Thumb(PathBuf, Ident, Option<(u32, u32, Vec<u8>)>),
}

/// Which list the Clips tab shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Clips,
    Uploads,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Today,
    Week,
    Favorites,
    Collection(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Newest,
    Oldest,
    Biggest,
    Longest,
}

impl Sort {
    const ALL: [Sort; 4] = [Sort::Newest, Sort::Oldest, Sort::Biggest, Sort::Longest];
    fn label(self) -> &'static str {
        match self {
            Sort::Newest => "Newest first",
            Sort::Oldest => "Oldest first",
            Sort::Biggest => "Biggest first",
            Sort::Longest => "Longest first",
        }
    }
}

/// Things a card asks for (applied after drawing).
enum Action {
    Open(PathBuf),
    Play(PathBuf),
    Reveal(PathBuf),
    Favorite(PathBuf),
    Upload(PathBuf, Option<Host>),
    Copy(String),
    Cancel(PathBuf),
    Dismiss(PathBuf),
    /// Switch the card title between the game and the file name.
    ToggleName(PathBuf),
    StartRename(PathBuf),
    Rename(PathBuf, String),
    /// Ask to move the clip to the Recycle Bin.
    Delete(PathBuf),
}

#[derive(Default)]
struct LoginForm {
    open: bool,
    /// The first-run card ("Log in with Streamable" / "Skip").
    onboarding: bool,
    /// The email/password fields are showing.
    expanded: bool,
    email: String,
    password: String,
    error: Option<String>,
}

pub struct LibraryTab {
    pub store: Library,
    pub uploader: Uploader,
    clips: Vec<Clip>,
    last_scan: Option<Instant>,
    meta: HashMap<PathBuf, Meta>,
    meta_dirty: Option<Instant>,
    thumbs: HashMap<PathBuf, Slot>,
    /// The file each thumbnail (or pending request) was made from.
    thumb_ident: HashMap<PathBuf, Ident>,
    pane: Pane,
    worker: Option<(Sender<Req>, Receiver<Res>)>,
    asked_meta: std::collections::HashSet<PathBuf>,
    query: String,
    filter: Filter,
    sort: Sort,
    picking: Option<Receiver<PathBuf>>,
    open_dialog: Option<Receiver<PathBuf>>,
    /// The link last copied, and when ("Copied!" on that button only).
    copied: Option<(String, Instant)>,
    /// Cards showing the file name instead of the game.
    show_file: std::collections::HashSet<PathBuf>,
    /// The file being renamed and the name typed so far.
    renaming: Option<(PathBuf, String)>,
    frame: u64,
    login: LoginForm,
    /// The clip the "Move to the Recycle Bin?" dialog is about.
    confirm_delete: Option<PathBuf>,
    /// The Delete key was pressed this frame (and may delete a clip).
    delete_key: bool,
    /// Files the trimmer is using (set by the window every frame); they
    /// can't be deleted.
    pub trimmer_files: Vec<PathBuf>,
    /// Messages for the window's toast.
    pub toasts: Vec<String>,
}

fn meta_file() -> PathBuf {
    crate::paths::thumbs_dir().join("library-meta.json")
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl LibraryTab {
    pub fn new() -> LibraryTab {
        let meta = std::fs::read_to_string(meta_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        LibraryTab {
            store: Library::load(),
            uploader: Uploader::default(),
            clips: Vec::new(),
            last_scan: None,
            meta,
            meta_dirty: None,
            thumbs: HashMap::new(),
            thumb_ident: HashMap::new(),
            pane: Pane::Clips,
            worker: None,
            asked_meta: Default::default(),
            query: String::new(),
            filter: Filter::All,
            sort: Sort::Newest,
            picking: None,
            open_dialog: None,
            copied: None,
            show_file: Default::default(),
            renaming: None,
            frame: 0,
            login: LoginForm::default(),
            confirm_delete: None,
            delete_key: false,
            trimmer_files: Vec::new(),
            toasts: Vec::new(),
        }
    }

    // ---- background ------------------------------------------------------

    /// Upload results and the login check; runs every frame, on any tab.
    pub fn poll_background(&mut self, ctx: &egui::Context) {
        for rec in self.uploader.poll() {
            ctx.copy_text(rec.url.clone());
            self.toasts.push(format!("Uploaded to {}. Link copied: {}", rec.host.name(), rec.url));
            self.copied = Some((rec.url.clone(), Instant::now()));
            self.store.uploads.push(rec);
            self.store.save();
        }
        if self.uploader.busy() > 0 {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        if let Some(r) = self.uploader.poll_login() {
            match r {
                Ok((email, plan)) => {
                    self.toasts.push(format!("Connected to Streamable as {email} ({} plan)", plan.name));
                    self.store.streamable = Some(plan);
                    self.store.onboarding_done = true;
                    self.store.save();
                    self.login = LoginForm::default();
                }
                Err(e) => {
                    self.login.error = Some(e);
                    self.login.password.clear();
                }
            }
        }
        if self.uploader.logging_in() {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }

    pub fn uploading(&self) -> usize {
        self.uploader.busy()
    }

    pub fn cancel_uploads(&mut self) {
        self.uploader.cancel_all();
    }

    /// Opens the Streamable login (Settings > Uploads, or the first run).
    fn open_login(&mut self, onboarding: bool) {
        self.login = LoginForm { open: true, onboarding, expanded: !onboarding, ..Default::default() };
        if let Some(email) = credentials::email() {
            self.login.email = email;
        }
    }

    /// Keeps the clip list fresh while the library isn't on screen (for the
    /// editor's "Add clip" list, at most every 3 seconds) and makes the
    /// newest clips' thumbnails, which that list shows.
    pub fn refresh_list(&mut self, ctx: &egui::Context, shared: &Shared) {
        if self.last_scan.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
            self.scan(shared);
        }
        self.collect(ctx);
        let wanted: Vec<(PathBuf, Ident)> = self
            .newest_videos()
            .into_iter()
            .take(40)
            .filter(|c| !self.thumbs.contains_key(&c.path))
            .map(|c| (c.path.clone(), (c.stamp, c.size)))
            .collect();
        for (path, ident) in wanted {
            self.thumbs.insert(path.clone(), Slot::Pending);
            self.thumb_ident.insert(path.clone(), ident);
            let (tx, _) = self.worker.get_or_insert_with(start_worker);
            let _ = tx.send(Req::Thumb(path, ident));
        }
    }

    /// The videos, newest first.
    fn newest_videos(&self) -> Vec<&Clip> {
        let mut list: Vec<&Clip> = self
            .clips
            .iter()
            .filter(|c| c.path.extension().is_some_and(|x| ["mp4", "mkv", "mov", "m4v"].iter().any(|v| x.eq_ignore_ascii_case(v))))
            .collect();
        list.sort_by_key(|c| std::cmp::Reverse((c.when.day_number(), c.when.hour, c.when.minute, c.stamp)));
        list
    }

    fn scan(&mut self, shared: &Shared) {
        self.last_scan = Some(Instant::now());
        let mut dirs: Vec<(PathBuf, Option<usize>)> = vec![(shared.settings.clips_dir.clone(), None)];
        for (i, d) in self.store.collections.iter().enumerate() {
            if !dirs.iter().any(|(x, _)| x == d) {
                dirs.push((d.clone(), Some(i)));
            }
        }
        let mut clips = Vec::new();
        for (dir, collection) in dirs {
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = e.path();
                let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                let video = path.extension().is_some_and(|x| ["mp4", "mkv", "mov", "m4v"].iter().any(|v| x.eq_ignore_ascii_case(v)));
                if !video || name.starts_with('.') {
                    continue; // temp/partial files
                }
                let Ok(meta) = e.metadata() else { continue };
                let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
                let when = LocalTime::from_file_name(&name).or_else(|| LocalTime::from_system(mtime)).unwrap_or_else(LocalTime::now);
                let kind = if name.starts_with("Clip_") {
                    "Clip"
                } else if name.starts_with("Recording_") {
                    "Recording"
                } else {
                    ""
                };
                let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                clips.push(Clip {
                    tag: if stem.contains("_edit") {
                        Some("Edited")
                    } else if stem.contains("_trim") {
                        Some("Trimmed")
                    } else {
                        None
                    },
                    path,
                    name,
                    size: meta.len(),
                    stamp: unix(mtime),
                    day: when.day_number(),
                    when,
                    kind,
                    collection,
                });
                if clips.len() >= 5000 {
                    break;
                }
            }
        }
        // Lengths and games for clips not probed yet (one at a time, in the
        // background; cached in library-meta.json).
        for c in &clips {
            let known = self.meta.get(&c.path).is_some_and(|m| m.stamp == c.stamp && m.size == c.size);
            if !known && self.asked_meta.insert(c.path.clone()) {
                let (tx, _) = self.worker.get_or_insert_with(start_worker);
                let _ = tx.send(Req::Meta(c.path.clone()));
            }
        }
        self.clips = clips;
    }

    fn collect(&mut self, ctx: &egui::Context) {
        let Some((_, rx)) = &self.worker else { return };
        let mut got = false;
        while let Ok(r) = rx.try_recv() {
            got = true;
            match r {
                Res::Meta(path, m) => {
                    self.asked_meta.remove(&path);
                    self.meta.insert(path, m);
                    self.meta_dirty = Some(Instant::now());
                }
                Res::Thumb(path, ident, rgba) => {
                    if self.thumb_ident.get(&path) != Some(&ident) {
                        continue; // made from a file that has since changed
                    }
                    let slot = match rgba {
                        Some((w, h, px)) => {
                            let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px);
                            Slot::Ready(ctx.load_texture(format!("lib-{}", path.display()), img, TextureOptions::LINEAR), self.frame)
                        }
                        None => Slot::Failed,
                    };
                    self.thumbs.insert(path, slot);
                }
            }
        }
        if got {
            ctx.request_repaint();
        }
        if self.meta_dirty.is_some_and(|t| t.elapsed() > Duration::from_secs(2)) {
            self.meta_dirty = None;
            // Forget files that are gone.
            let present: std::collections::HashSet<&PathBuf> = self.clips.iter().map(|c| &c.path).collect();
            let kept: HashMap<&PathBuf, &Meta> = self.meta.iter().filter(|(p, _)| present.contains(p)).collect();
            if let Ok(json) = serde_json::to_vec(&kept) {
                let _ = std::fs::create_dir_all(crate::paths::thumbs_dir());
                let _ = std::fs::write(meta_file(), json);
            }
        }
        if self.meta_dirty.is_some() || !self.asked_meta.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(300));
        }
    }

    /// Drops textures not drawn recently (beyond the limit).
    fn trim_textures(&mut self) {
        let ready = self.thumbs.values().filter(|s| matches!(s, Slot::Ready(..))).count();
        if ready <= MAX_TEXTURES {
            return;
        }
        let mut by_age: Vec<(u64, PathBuf)> = self
            .thumbs
            .iter()
            .filter_map(|(p, s)| match s {
                Slot::Ready(_, used) if *used + 2 < self.frame => Some((*used, p.clone())),
                _ => None,
            })
            .collect();
        by_age.sort();
        for (_, p) in by_age.into_iter().take(ready - MAX_TEXTURES) {
            self.thumbs.remove(&p);
        }
    }

    /// The clip's display name: the game it came from, else "Clip 19:23".
    fn title(&self, c: &Clip) -> String {
        let game = self.meta.get(&c.path).and_then(|m| m.title.clone()).filter(|t| !t.trim().is_empty());
        match (game, c.kind) {
            (Some(g), _) => g,
            (None, "") => c.path.file_stem().unwrap_or_default().to_string_lossy().into_owned(),
            (None, k) => format!("{k} {:02}:{:02}", c.when.hour, c.when.minute),
        }
    }

    fn seconds(&self, c: &Clip) -> Option<f64> {
        self.meta.get(&c.path).filter(|m| m.stamp == c.stamp && m.size == c.size).and_then(|m| m.seconds)
    }

    /// The videos in the library, newest first, for the editor's "Add clip".
    pub fn clip_choices(&self) -> Vec<ClipChoice> {
        let today = LocalTime::now().day_number();
        self.newest_videos()
            .into_iter()
            .map(|c| {
                let title = self.meta.get(&c.path).and_then(|m| m.title.clone());
                let stem = c.path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                let mut detail = format!("{} {:02}:{:02}", day_label(&c.when, today), c.when.hour, c.when.minute);
                if let Some(s) = self.seconds(c) {
                    detail += &format!(" · {}", fmt_duration(s));
                }
                detail += &format!(" · {}", fmt_bytes(c.size));
                if title.is_some() {
                    detail += &format!(" · {stem}");
                }
                let thumb = match self.thumbs.get(&c.path) {
                    Some(Slot::Ready(t, _)) => Some(t.clone()),
                    _ => None,
                };
                ClipChoice { path: c.path.clone(), name: title.unwrap_or(stem), detail, thumb }
            })
            .collect()
    }

    // ---- the Clips tab ------------------------------------------------------

    /// Draws the library; returns a clip to open in the trimmer.
    pub fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared) -> Option<PathBuf> {
        self.frame += 1;
        if self.last_scan.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
            self.scan(shared);
        }
        self.collect(ui.ctx());
        ui.ctx().request_repaint_after(Duration::from_secs(3));
        if let Some(rx) = &self.picking {
            if let Ok(dir) = rx.try_recv() {
                if !self.store.collections.contains(&dir) && dir != shared.settings.clips_dir {
                    self.store.collections.push(dir);
                    self.store.save();
                    self.filter = Filter::Collection(self.store.collections.len() - 1);
                    self.last_scan = None;
                }
                self.picking = None;
            }
        }
        let mut open = None;
        if let Some(rx) = &self.open_dialog {
            if let Ok(p) = rx.try_recv() {
                open = Some(p);
                self.open_dialog = None;
            }
        }
        if !self.store.onboarding_done && self.store.streamable.is_none() && !self.login.open {
            self.open_login(true);
        }

        self.header(ui, shared);
        ui.add_space(12.0);
        if self.pane == Pane::Uploads {
            self.uploads_list(ui, shared);
            return open;
        }
        self.filter_row(ui, shared);
        ui.add_space(10.0);

        // Which clips, in which order.
        let today = LocalTime::now().day_number();
        let q = self.query.trim().to_lowercase();
        let mut shown: Vec<usize> = (0..self.clips.len())
            .filter(|&i| {
                let c = &self.clips[i];
                let pass = match self.filter {
                    Filter::All => true,
                    Filter::Today => c.day == today,
                    Filter::Week => c.day > today - 7,
                    Filter::Favorites => self.store.favorites.contains(&c.path),
                    Filter::Collection(k) => c.collection == Some(k),
                };
                pass && (q.is_empty() || c.name.to_lowercase().contains(&q) || self.title(c).to_lowercase().contains(&q))
            })
            .collect();
        let key = |c: &Clip| (c.day, c.when.hour, c.when.minute, c.stamp);
        match self.sort {
            Sort::Newest => shown.sort_by_key(|&i| std::cmp::Reverse(key(&self.clips[i]))),
            Sort::Oldest => shown.sort_by_key(|&i| key(&self.clips[i])),
            Sort::Biggest => shown.sort_by_key(|&i| std::cmp::Reverse(self.clips[i].size)),
            Sort::Longest => shown.sort_by(|&a, &b| {
                let (x, y) = (self.seconds(&self.clips[a]).unwrap_or(-1.0), self.seconds(&self.clips[b]).unwrap_or(-1.0));
                y.total_cmp(&x)
            }),
        }

        // Footer first (bottom of the tab), then the cards fill the rest.
        let total: u64 = shown.iter().map(|&i| self.clips[i].size).sum();
        egui::Panel::bottom("library-footer")
            .frame(egui::Frame::new().inner_margin(egui::Margin { top: 8, ..Default::default() }))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let n = shown.len();
                    ui.label(dim(shared, format!("{n} {} · {}", if n == 1 { "clip" } else { "clips" }, fmt_bytes(total))));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.link("Open clips folder").clicked() {
                            crate::win::reveal_in_explorer(&shared.settings.clips_dir);
                        }
                        ui.label(dim(shared, "·"));
                        if ui.link("Open a video").on_hover_text("Trim any video file").clicked() {
                            self.open_dialog = Some(super::dialogs::pick_video(&shared.settings.clips_dir));
                        }
                    });
                });
            });

        let mut actions = Vec::new();
        // The Delete key works on the clip under the mouse, or the one with
        // keyboard focus (not while typing).
        let delete_key = self.confirm_delete.is_none()
            && self.renaming.is_none()
            && !ui.ctx().text_edit_focused()
            && ui.input(|i| i.key_pressed(egui::Key::Delete));
        self.delete_key = delete_key;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if shown.is_empty() {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    let msg = if self.clips.is_empty() {
                        format!("No clips yet. Press {} while playing to save the last moments.", shared.settings.hotkeys.clip)
                    } else {
                        "No clips match.".to_string()
                    };
                    ui.label(dim(shared, msg));
                });
                return;
            }
            let avail = ui.available_width() - 4.0;
            let view = self.store.view;
            // Items per row, their size, and the gap between rows.
            let (cols, item_w, item_h, gap_y) = match view {
                ClipView::Grid | ClipView::Compact => {
                    let (min_w, below) = if view == ClipView::Grid { (CARD_MIN_W, 64.0) } else { (COMPACT_MIN_W, 30.0) };
                    let cols = (((avail + GAP) / (min_w + GAP)).floor() as usize).max(1);
                    let w = ((avail - GAP * (cols - 1) as f32) / cols as f32).floor();
                    (cols, w, (w * 9.0 / 16.0).round() + below, GAP)
                }
                ClipView::List => (1, avail, LIST_ROW_H, 4.0),
            };
            let grouped = matches!(self.sort, Sort::Newest | Sort::Oldest);
            let mut start = 0;
            while start < shown.len() {
                let day = self.clips[shown[start]].day;
                let end = if grouped {
                    start + shown[start..].iter().take_while(|&&i| self.clips[i].day == day).count()
                } else {
                    shown.len()
                };
                if grouped {
                    let label = day_label(&self.clips[shown[start]].when, today);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(label).family(crate::themes::brand_bold_family()).size(shared.theme.fonts.body + 1.0));
                        ui.label(dim(shared, format!("{}", end - start)).small());
                    });
                    ui.add_space(6.0);
                }
                for row in shown[start..end].chunks(cols) {
                    // Every row takes exactly its height, drawn or not, so
                    // the scroll length doesn't change while scrolling.
                    let row_rect = Rect::from_min_size(ui.cursor().min, vec2(avail, item_h));
                    if ui.is_rect_visible(row_rect) {
                        let mut row_ui = ui.new_child(
                            egui::UiBuilder::new()
                                .id_salt(("clip-row", &self.clips[row[0]].path))
                                .max_rect(row_rect)
                                .layout(egui::Layout::left_to_right(egui::Align::Min)),
                        );
                        row_ui.spacing_mut().item_spacing.x = GAP;
                        for &i in row {
                            let item = row_ui.allocate_ui_with_layout(vec2(item_w, item_h), egui::Layout::top_down(egui::Align::Min), |ui| {
                                ui.set_width(item_w);
                                match view {
                                    ClipView::Grid => self.card(ui, shared, i, item_w, &mut actions),
                                    ClipView::Compact => self.compact_card(ui, shared, i, item_w, &mut actions),
                                    ClipView::List => self.list_row(ui, shared, i, item_w, &mut actions),
                                }
                            });
                            let c = &self.clips[i];
                            if delete_key && c.collection.is_none() && row_ui.rect_contains_pointer(item.response.rect) {
                                actions.push(Action::Delete(c.path.clone()));
                            }
                        }
                    }
                    ui.allocate_space(vec2(avail, item_h));
                    ui.add_space(gap_y);
                }
                ui.add_space(6.0);
                start = end;
            }
        });
        self.trim_textures();
        for a in actions {
            match a {
                Action::Open(p) => open = Some(p),
                other => self.apply(other, ui.ctx(), shared),
            }
        }
        open
    }

    fn apply(&mut self, a: Action, ctx: &egui::Context, shared: &Shared) {
        match a {
            Action::Open(_) => {}
            Action::Play(p) => crate::win::shell_open(&p.display().to_string()),
            Action::Reveal(p) => crate::win::reveal_in_explorer(&p),
            Action::Favorite(p) => {
                if !self.store.favorites.remove(&p) {
                    self.store.favorites.insert(p);
                }
                self.store.save();
            }
            Action::Upload(p, force) => self.start_upload(&p, force, shared),
            Action::Copy(url) => {
                ctx.copy_text(url.clone());
                self.copied = Some((url, Instant::now()));
            }
            Action::ToggleName(p) => {
                if !self.show_file.remove(&p) {
                    self.show_file.insert(p);
                }
            }
            Action::StartRename(p) => {
                let stem = p.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                self.renaming = Some((p, stem));
            }
            Action::Rename(p, name) => {
                self.renaming = None;
                if let Err(e) = self.rename(&p, &name) {
                    self.toasts.push(format!("Could not rename: {e}"));
                }
            }
            Action::Cancel(p) => self.uploader.cancel(&p),
            Action::Dismiss(p) => self.uploader.dismiss(&p),
            Action::Delete(p) => match self.why_not_delete(&p, shared) {
                Some(why) => self.toasts.push(why),
                None => self.confirm_delete = Some(p),
            },
        }
    }

    fn file_name(path: &Path) -> String {
        path.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }

    /// Why this clip can't be deleted right now (None = it can).
    fn why_not_delete(&self, path: &Path, shared: &Shared) -> Option<String> {
        let name = Self::file_name(path);
        if matches!(self.uploader.view(path), Some(View::Busy { .. })) {
            return Some(format!("Can't delete {name}: it's uploading. Cancel the upload first."));
        }
        if self.trimmer_files.iter().any(|p| p == path) {
            return Some(format!("Can't delete {name}: the trimmer is using it. Wait for the export to finish."));
        }
        crate::library::deletable(path, &shared.settings.clips_dir).err().map(|e| format!("Can't delete {name}: {e}."))
    }

    /// Moves a clip to the Recycle Bin and forgets what the library knew
    /// about it. Upload records stay: the Uploads list keeps the links to
    /// copies that are still online.
    fn delete(&mut self, path: &Path, shared: &Shared) -> Result<(), String> {
        if let Some(why) = self.why_not_delete(path, shared) {
            return Err(why);
        }
        let real = crate::library::deletable(path, &shared.settings.clips_dir)?;
        let ident = self.clips.iter().find(|c| c.path == path).map(|c| (c.stamp, c.size));
        crate::win::recycle(&real).map_err(|e| format!("Couldn't delete {}: {e:#}", Self::file_name(path)))?;
        log::info!("moved {} to the Recycle Bin", path.display());
        if self.store.favorites.remove(path) {
            self.store.save();
        }
        self.uploader.dismiss(path);
        if self.meta.remove(path).is_some() {
            self.meta_dirty = Some(Instant::now());
        }
        self.thumbs.remove(path);
        self.thumb_ident.remove(path);
        self.show_file.remove(path);
        // The cached thumbnails (Clips tab and Live tab).
        if let Some((stamp, size)) = ident {
            let dir = crate::paths::thumbs_dir();
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            let _ = std::fs::remove_file(dir.join(format!("{:016x}-{stamp}-{size}-{THUMB_W}.png", path_hash(path))));
            let _ = std::fs::remove_file(dir.join(format!("{stem}-{stamp}.png")));
        }
        self.clips.retain(|c| c.path != path);
        self.last_scan = None;
        Ok(())
    }

    fn start_upload(&mut self, path: &Path, force: Option<Host>, shared: &Shared) {
        let (title, seconds) = match self.clips.iter().find(|c| c.path == path) {
            Some(c) => {
                let mut t = self.title(c);
                if c.kind.is_empty() || self.meta.get(&c.path).and_then(|m| m.title.as_ref()).is_some() {
                    t = format!("{t} · {} {} {:02}:{:02}", c.when.day, MONTHS[(c.when.month - 1) as usize], c.when.hour, c.when.minute);
                }
                (t, self.seconds(c))
            }
            None => (path.file_stem().unwrap_or_default().to_string_lossy().into_owned(), None),
        };
        self.uploader.start(Request {
            path: path.to_path_buf(),
            title,
            seconds,
            plan: self.store.streamable.clone().filter(|_| credentials::load().is_some()),
            force,
            litterbox_hours: shared.settings.uploads.litterbox_hours,
        });
    }

    fn header(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let theme = shared.theme.clone();
        let hint = if self.pane == Pane::Uploads { "Search uploads" } else { "Search clips" };
        ui.horizontal(|ui| {
            super::widgets::search_box(ui, &theme, &mut self.query, 280.0, false, hint);
            ui.add_space(28.0);
            let counts = [self.clips.len(), self.store.uploads.len()];
            for (k, (pane, name)) in [(Pane::Clips, "Clips"), (Pane::Uploads, "Uploads")].into_iter().enumerate() {
                if k == 1 {
                    // The divider between the two.
                    let (r, _) = ui.allocate_exact_size(vec2(1.0, 30.0), Sense::hover());
                    ui.painter().line_segment([pos2(r.center().x, r.top() + 6.0), pos2(r.center().x, r.bottom() - 6.0)], Stroke::new(1.0, theme.colors.border.0));
                }
                if segment(ui, &theme, name, counts[k], self.pane == pane).clicked() {
                    self.pane = pane;
                }
            }
        });
    }

    /// Opens the Uploads list (e.g. `--tab uploads`).
    pub fn show_uploads(&mut self) {
        self.pane = Pane::Uploads;
    }

    fn filter_row(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let today = LocalTime::now().day_number();
        let count = |f: Filter| {
            self.clips
                .iter()
                .filter(|c| match f {
                    Filter::All => true,
                    Filter::Today => c.day == today,
                    Filter::Week => c.day > today - 7,
                    Filter::Favorites => self.store.favorites.contains(&c.path),
                    Filter::Collection(k) => c.collection == Some(k),
                })
                .count()
        };
        let theme = shared.theme.clone();
        let mut remove = None;
        let mut view = self.store.view;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            for (f, name) in [(Filter::All, "All"), (Filter::Today, "Today"), (Filter::Week, "This week"), (Filter::Favorites, "Favorites")] {
                if pill(ui, &theme, name, count(f), self.filter == f).clicked() {
                    self.filter = f;
                }
            }
            for (k, dir) in self.store.collections.clone().iter().enumerate() {
                let name = dir.file_name().unwrap_or_default().to_string_lossy().to_string();
                let f = Filter::Collection(k);
                let r = pill(ui, &theme, &name, count(f), self.filter == f).on_hover_text(format!("{}\nRight-click for options", dir.display()));
                if r.clicked() {
                    self.filter = f;
                }
                r.context_menu(|ui| {
                    if ui.button("Open folder").clicked() {
                        crate::win::reveal_in_explorer(dir);
                        ui.close();
                    }
                    if ui.button("Remove from library").on_hover_text("The folder and its videos stay where they are").clicked() {
                        remove = Some(k);
                        ui.close();
                    }
                });
            }
            let add = egui::Button::new(RichText::new("+ Add folder").color(theme.colors.text_dim.0))
                .fill(Color32::TRANSPARENT)
                .stroke(Stroke::new(1.0, theme.colors.border.0))
                .corner_radius(CornerRadius::same(14))
                .min_size(vec2(0.0, 28.0));
            if ui.add(add).on_hover_text("Show another folder of videos here, as its own group").clicked() && self.picking.is_none() {
                self.picking = Some(super::dialogs::pick_folder(&shared.settings.clips_dir));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                view_switch(ui, &theme, &mut view);
                ui.add_space(8.0);
                egui::ComboBox::from_id_salt("library-sort").selected_text(format!("Sort: {}", self.sort.label())).show_ui(ui, |ui| {
                    for s in Sort::ALL {
                        ui.selectable_value(&mut self.sort, s, s.label());
                    }
                });
            });
        });
        if view != self.store.view {
            self.store.view = view;
            self.store.save();
        }
        if let Some(k) = remove {
            self.store.collections.remove(k);
            self.store.save();
            self.filter = Filter::All;
            self.last_scan = None;
        }
    }

    /// A clip's thumbnail with its overlays: play on hover, and (unless
    /// `small`) the length, "Trimmed" and the favourite star. Click opens
    /// the trimmer; right-click shows the clip's menu.
    fn thumbnail(&mut self, ui: &mut egui::Ui, shared: &Shared, i: usize, size: egui::Vec2, small: bool, actions: &mut Vec<Action>) {
        let c = shared.theme.colors.clone();
        let clip = &self.clips[i];
        let path = clip.path.clone();
        let seconds = self.seconds(clip);
        let favorite = self.store.favorites.contains(&path);
        let (tag, ident) = (clip.tag, (clip.stamp, clip.size));
        let can_delete = clip.collection.is_none();
        let radius = if small { 6.0 } else { 10.0 };

        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        let p = ui.painter().clone();
        p.rect_filled(rect, radius, c.preview.0);
        if self.thumb_ident.get(&path).is_some_and(|i| *i != ident) {
            self.thumbs.remove(&path); // the file changed (e.g. trimmed in place)
        }
        match self.thumbs.get_mut(&path) {
            Some(Slot::Ready(tex, used)) => {
                *used = self.frame;
                p.add(
                    egui::epaint::RectShape::filled(rect, radius, Color32::WHITE)
                        .with_texture(tex.id(), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))),
                );
            }
            Some(Slot::Pending | Slot::Failed) => {}
            None => {
                self.thumbs.insert(path.clone(), Slot::Pending);
                self.thumb_ident.insert(path.clone(), ident);
                let (tx, _) = self.worker.get_or_insert_with(start_worker);
                let _ = tx.send(Req::Thumb(path.clone(), ident));
            }
        }
        let hovered = resp.hovered();
        if hovered {
            let (r, k) = if small { (12.0, 0.55) } else { (22.0, 1.0) };
            p.rect_stroke(rect, radius, Stroke::new(2.0, c.accent.0), egui::StrokeKind::Inside);
            p.circle_filled(rect.center(), r, Color32::from_black_alpha(140));
            let ctr = rect.center();
            p.add(Shape::convex_polygon(
                vec![ctr + vec2(-6.0, -9.0) * k, ctr + vec2(10.0, 0.0) * k, ctr + vec2(-6.0, 9.0) * k],
                Color32::WHITE,
                Stroke::NONE,
            ));
        }
        let mut star_clicked = false;
        if !small {
            if let Some(s) = seconds {
                let badge = p.layout_no_wrap(fmt_duration(s), egui::FontId::proportional(11.5), Color32::WHITE);
                let b = Rect::from_min_size(rect.max - badge.size() - vec2(16.0, 12.0), badge.size() + vec2(10.0, 4.0));
                p.rect_filled(b, 5.0, Color32::from_black_alpha(185));
                p.galley(b.min + vec2(5.0, 2.0), badge, Color32::WHITE);
            }
            if let Some(tag) = tag {
                let tag = p.layout_no_wrap(tag.into(), egui::FontId::proportional(10.5), Color32::WHITE);
                let b = Rect::from_min_size(pos2(rect.right() - tag.size().x - 18.0, rect.top() + 10.0), tag.size() + vec2(10.0, 4.0));
                p.rect_filled(b, 5.0, Color32::from_black_alpha(160));
                p.galley(b.min + vec2(5.0, 2.0), tag, Color32::WHITE);
            }
            // Favourite star (top left).
            let star_c = pos2(rect.left() + 20.0, rect.top() + 20.0);
            let star_rect = Rect::from_center_size(star_c, vec2(28.0, 28.0));
            let star = ui.interact(star_rect, ui.scope_id().with(("star", &path)), Sense::click());
            if favorite || hovered || star.hovered() {
                p.circle_filled(star_c, 14.0, Color32::from_black_alpha(if star.hovered() { 170 } else { 120 }));
                paint_star(&p, star_c, 8.0, if favorite { Some(STAR) } else { None }, Color32::WHITE);
            }
            star_clicked = star.on_hover_text(if favorite { "Remove from favorites" } else { "Add to favorites" }).clicked();
            if star_clicked {
                actions.push(Action::Favorite(path.clone()));
            }
        }
        if resp.clicked() && !star_clicked {
            actions.push(Action::Open(path.clone()));
        }
        if self.delete_key && can_delete && resp.has_focus() {
            actions.push(Action::Delete(path.clone()));
        }
        let upload = self.store.upload_of(&path).cloned();
        resp.on_hover_text("Open in the trimmer")
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .context_menu(|ui| clip_menu(ui, &path, favorite, upload.as_ref(), can_delete, actions));
    }

    /// Grid view: a big thumbnail, the title, then time/size | Upload | Trim.
    fn card(&mut self, ui: &mut egui::Ui, shared: &Shared, i: usize, w: f32, actions: &mut Vec<Action>) {
        let c = shared.theme.colors.clone();
        let clip = &self.clips[i];
        let path = clip.path.clone();
        let title = self.title(clip);
        let seconds = self.seconds(clip);
        let subtitle = format!("{:02}:{:02} · {}", clip.when.hour, clip.when.minute, fmt_bytes(clip.size));
        let size = clip.size;
        self.thumbnail(ui, shared, i, vec2(w, (w * 9.0 / 16.0).round()), false, actions);

        ui.add_space(6.0);
        self.title_row(ui, shared, &path, &title, w, actions);
        let (row, _) = ui.allocate_exact_size(vec2(w, 24.0), Sense::hover());
        let p = ui.painter().clone();
        let sub = p.layout_no_wrap(subtitle, egui::FontId::proportional(12.0), c.text_dim.0);
        let sub_right = row.left() + sub.size().x;
        p.galley(pos2(row.left(), row.center().y - sub.size().y / 2.0), sub, c.text_dim.0);
        let trim_rect = Rect::from_min_max(pos2(row.right() - 34.0, row.top()), row.max);
        let trim = ui.put(trim_rect, egui::Link::new(RichText::new("Trim").color(c.accent.0)));
        if trim.on_hover_text("Open in the trimmer").clicked() {
            actions.push(Action::Open(path.clone()));
        }
        // Centered in the space between the time/size and "Trim".
        let (free_l, free_r) = (sub_right + 8.0, trim_rect.left() - 8.0);
        let x = if free_r - free_l >= 86.0 { (free_l + free_r) / 2.0 } else { row.center().x };
        let btn_rect = Rect::from_center_size(pos2(x, row.center().y), vec2(86.0, 22.0));
        let upload = self.store.upload_of(&path).cloned();
        self.upload_widget(ui, shared, btn_rect, &path, size, seconds, upload.as_ref(), actions);
    }

    /// Compact view: a small thumbnail (with the length) and the title.
    fn compact_card(&mut self, ui: &mut egui::Ui, shared: &Shared, i: usize, w: f32, actions: &mut Vec<Action>) {
        let path = self.clips[i].path.clone();
        let title = self.title(&self.clips[i]);
        self.thumbnail(ui, shared, i, vec2(w, (w * 9.0 / 16.0).round()), false, actions);
        ui.add_space(4.0);
        self.title_row(ui, shared, &path, &title, w, actions);
    }

    /// List view: thumbnail | title over date, time and size | star |
    /// length | Upload | Trim.
    fn list_row(&mut self, ui: &mut egui::Ui, shared: &Shared, i: usize, w: f32, actions: &mut Vec<Action>) {
        let c = shared.theme.colors.clone();
        let clip = &self.clips[i];
        let path = clip.path.clone();
        let title = self.title(clip);
        let seconds = self.seconds(clip);
        let favorite = self.store.favorites.contains(&path);
        let when = format!(
            "{} {} {:02}:{:02} · {}",
            clip.when.day,
            MONTHS[(clip.when.month - 1) as usize],
            clip.when.hour,
            clip.when.minute,
            fmt_bytes(clip.size)
        );
        let size = clip.size;
        let upload = self.store.upload_of(&path).cloned();

        let row = Rect::from_min_size(ui.cursor().min, vec2(w, LIST_ROW_H));
        if ui.rect_contains_pointer(row) {
            ui.painter().rect_filled(row, 8.0, c.widget_hover.0.gamma_multiply(0.6));
        }
        // Right-hand side, from the right edge: Trim, Upload, length, star.
        let right = 34.0 + 10.0 + 86.0 + 12.0 + 52.0 + 6.0 + 26.0 + 10.0;
        let text_w = (w - 8.0 - LIST_THUMB.x - 12.0 - right).max(60.0);
        ui.allocate_ui_with_layout(vec2(w, LIST_ROW_H), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.add_space(8.0);
            self.thumbnail(ui, shared, i, LIST_THUMB, true, actions);
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(vec2(text_w, LIST_ROW_H), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(text_w);
                ui.add_space(9.0);
                self.title_row(ui, shared, &path, &title, text_w, actions);
                ui.label(RichText::new(when).size(12.0).color(c.text_dim.0));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.add_space(10.0);
                let trim = ui.add_sized(vec2(34.0, 22.0), egui::Link::new(RichText::new("Trim").color(c.accent.0)));
                if trim.on_hover_text("Open in the trimmer").clicked() {
                    actions.push(Action::Open(path.clone()));
                }
                ui.add_space(10.0);
                let (btn, _) = ui.allocate_exact_size(vec2(86.0, 22.0), Sense::hover());
                self.upload_widget(ui, shared, btn, &path, size, seconds, upload.as_ref(), actions);
                ui.add_space(12.0);
                let (len, _) = ui.allocate_exact_size(vec2(52.0, 22.0), Sense::hover());
                if let Some(s) = seconds {
                    ui.painter().text(len.right_center(), egui::Align2::RIGHT_CENTER, fmt_duration(s), egui::FontId::proportional(12.5), c.text.0);
                }
                ui.add_space(6.0);
                let (star_rect, star) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
                let col = if favorite { STAR } else if star.hovered() { c.text.0 } else { c.text_dim.0 };
                paint_star(ui.painter(), star_rect.center(), 7.5, favorite.then_some(STAR), col);
                if star
                    .on_hover_text(if favorite { "Remove from favorites" } else { "Add to favorites" })
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    actions.push(Action::Favorite(path.clone()));
                }
            });
        });
    }

    /// The card's title: the game (or "Clip 19:23"); click to show the file
    /// name instead, which has a pen to rename the file.
    fn title_row(&mut self, ui: &mut egui::Ui, shared: &Shared, path: &Path, title: &str, w: f32, actions: &mut Vec<Action>) {
        let c = &shared.theme.colors;
        let size = shared.theme.fonts.body;
        if let Some((p, text)) = self.renaming.as_mut().filter(|(p, _)| p == path) {
            let id = ui.scope_id().with(("rename", &*p));
            let resp = ui.add_sized(vec2(w, 22.0), egui::TextEdit::singleline(text).id(id).font(egui::FontId::proportional(size)));
            if !resp.has_focus() && !resp.lost_focus() {
                resp.request_focus();
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.renaming = None;
            } else if resp.lost_focus() {
                actions.push(Action::Rename(p.clone(), text.trim().to_string()));
            }
            return;
        }
        let file = self.show_file.contains(path);
        let shown = if file { path.file_stem().unwrap_or_default().to_string_lossy().into_owned() } else { title.to_string() };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.set_max_width(w);
            let pen_w = if file { 22.0 } else { 0.0 };
            let label = egui::Label::new(RichText::new(&shown).size(size).strong()).truncate().sense(Sense::click());
            let resp = ui.scope(|ui| {
                ui.set_max_width((w - pen_w).max(40.0));
                ui.add(label)
            })
            .inner;
            let tip = if file { "Click to show the game" } else { "Click to show the file name" };
            if resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                actions.push(Action::ToggleName(path.to_path_buf()));
            }
            if file {
                let (r, pen) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
                let col = if pen.hovered() { c.accent.0 } else { c.text_dim.0 };
                paint_pen(ui.painter(), r.shrink(2.0), col);
                if pen.on_hover_text("Rename the file").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    actions.push(Action::StartRename(path.to_path_buf()));
                }
            }
        });
    }

    /// Renames a clip file (same folder and extension) and everything the
    /// library remembers about it.
    fn rename(&mut self, from: &Path, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() || name.chars().any(|ch| r#"\/:*?"<>|"#.contains(ch) || ch.is_control()) || name.ends_with('.') {
            return Err(r#"names can't be empty, end with a dot, or use \ / : * ? " < > |"#.into());
        }
        let ext = from.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "mp4".into());
        let to = from.with_file_name(format!("{name}.{ext}"));
        if to == from {
            return Ok(());
        }
        if to.exists() && !to.to_string_lossy().eq_ignore_ascii_case(&from.to_string_lossy()) {
            return Err(format!("{name}.{ext} already exists"));
        }
        std::fs::rename(from, &to).map_err(|e| e.to_string())?;
        if self.store.favorites.remove(from) {
            self.store.favorites.insert(to.clone());
        }
        for u in self.store.uploads.iter_mut().filter(|u| u.path == from) {
            u.path = to.clone();
        }
        self.store.save();
        if let Some(m) = self.meta.remove(from) {
            self.meta.insert(to.clone(), m);
            self.meta_dirty = Some(Instant::now());
        }
        if let Some(s) = self.thumbs.remove(from) {
            self.thumbs.insert(to.clone(), s);
        }
        if let Some(i) = self.thumb_ident.remove(from) {
            self.thumb_ident.insert(to.clone(), i);
        }
        if self.show_file.remove(from) {
            self.show_file.insert(to.clone());
        }
        self.last_scan = None;
        log::info!("renamed {} -> {}", from.display(), to.display());
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn upload_widget(
        &self,
        ui: &mut egui::Ui,
        shared: &Shared,
        rect: Rect,
        path: &Path,
        bytes: u64,
        seconds: Option<f64>,
        done: Option<&UploadRecord>,
        actions: &mut Vec<Action>,
    ) {
        let c = &shared.theme.colors;
        let id = ui.scope_id().with(("upload", path));
        let font = egui::FontId::proportional(12.0);
        let p = ui.painter().clone();
        let button = |ui: &mut egui::Ui, text: &str, color: Color32| -> Response {
            let r = ui.interact(rect, id, Sense::click());
            let fill = color.gamma_multiply(if r.hovered() { 0.24 } else { 0.13 });
            p.rect_filled(rect, 11.0, fill);
            p.text(rect.center(), egui::Align2::CENTER_CENTER, text, font.clone(), color);
            r.on_hover_cursor(egui::CursorIcon::PointingHand)
        };
        match self.uploader.view(path) {
            Some(View::Busy { host, fraction }) => {
                let r = ui.interact(rect, id, Sense::click());
                p.rect_filled(rect, 11.0, c.widget_hover.0);
                if fraction > 0.0 {
                    let mut f = rect;
                    f.set_width((rect.width() * fraction).max(22.0));
                    p.rect_filled(f, 11.0, c.accent.0.gamma_multiply(0.55));
                }
                let label = match host {
                    None => "Preparing".to_string(),
                    Some(_) if fraction >= 0.999 => "Finishing".to_string(),
                    Some(_) if fraction == 0.0 => "Starting".to_string(),
                    Some(_) => format!("{:.0}%", fraction * 100.0),
                };
                p.text(rect.center(), egui::Align2::CENTER_CENTER, label, font.clone(), c.text.0);
                let to = host.map(|h| h.name()).unwrap_or("Streamable or Litterbox");
                if r.on_hover_text(format!("Uploading to {to}. Click to cancel.")).clicked() {
                    actions.push(Action::Cancel(path.to_path_buf()));
                }
            }
            Some(View::Failed(f)) => {
                let litterbox_ok = f.can_litterbox && f.host != Some(Host::Litterbox);
                let mut tip = format!("Upload failed: {}\nClick to try again.", f.error);
                if litterbox_ok {
                    tip.push_str(" Right-click to use Litterbox instead.");
                }
                let r = button(ui, "Retry", c.error.0).on_hover_text(tip);
                if r.clicked() {
                    actions.push(Action::Upload(path.to_path_buf(), f.host));
                }
                r.context_menu(|ui| {
                    if ui.button("Try again").clicked() {
                        actions.push(Action::Upload(path.to_path_buf(), f.host));
                        ui.close();
                    }
                    if litterbox_ok && ui.button("Upload to Litterbox instead").clicked() {
                        actions.push(Action::Upload(path.to_path_buf(), Some(Host::Litterbox)));
                        ui.close();
                    }
                    if ui.button("Dismiss").clicked() {
                        actions.push(Action::Dismiss(path.to_path_buf()));
                        ui.close();
                    }
                });
            }
            None => match done {
                Some(u) => {
                    let just = self.copied.as_ref().is_some_and(|(url, t)| *url == u.url && t.elapsed() < Duration::from_millis(1500));
                    if just {
                        ui.ctx().request_repaint_after(Duration::from_millis(300));
                    }
                    let r = button(ui, if just { "Copied!" } else { "Copy link" }, c.success.0)
                        .on_hover_text(format!("{}\n{} · {}", u.url, u.host.name(), u.expiry_text()));
                    if r.clicked() {
                        actions.push(Action::Copy(u.url.clone()));
                    }
                    r.context_menu(|ui| {
                        if ui.button("Open link").clicked() {
                            crate::win::open_link(&u.url);
                            ui.close();
                        }
                        if ui.button("Upload again").clicked() {
                            actions.push(Action::Upload(path.to_path_buf(), None));
                            ui.close();
                        }
                    });
                }
                None => {
                    let plan = self.store.streamable.as_ref();
                    match crate::upload::choose_host(bytes, seconds, plan) {
                        Ok(host) => {
                            let tip = match host {
                                Host::Streamable => "Upload to Streamable and copy the link".to_string(),
                                Host::Litterbox if plan.is_some() => format!(
                                    "Too big or long for your Streamable plan: uploads to Litterbox (link lasts {} h)",
                                    shared.settings.uploads.litterbox_hours
                                ),
                                Host::Litterbox => format!(
                                    "Upload to Litterbox (anonymous link, lasts {} h). Connect Streamable in Settings > Uploads for lasting links.",
                                    shared.settings.uploads.litterbox_hours
                                ),
                            };
                            if button(ui, "Upload", c.accent.0).on_hover_text(tip).clicked() {
                                actions.push(Action::Upload(path.to_path_buf(), None));
                            }
                        }
                        Err(why) => {
                            p.rect_filled(rect, 11.0, c.widget_hover.0);
                            p.text(rect.center(), egui::Align2::CENTER_CENTER, "Too big", font.clone(), c.text_dim.0);
                            ui.interact(rect, id, Sense::hover()).on_hover_text(why);
                        }
                    }
                }
            },
        }
    }

    // ---- the Uploads list ---------------------------------------------------------

    fn uploads_list(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let q = self.query.trim().to_lowercase();
        let c = shared.theme.colors.clone();
        let mut forget = None;
        let mut clear_expired = false;
        let mut copy = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(980.0));
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.store.uploads.iter().any(UploadRecord::expired) && ui.button("Remove expired").clicked() {
                        clear_expired = true;
                    }
                    let who = match &self.store.streamable {
                        Some(p) => format!("Streamable: connected ({} plan)", p.name),
                        None => "Streamable: not connected".to_string(),
                    };
                    ui.label(dim(shared, who).small());
                });
            });
            ui.add_space(10.0);
            if self.store.uploads.is_empty() {
                ui.label(dim(shared, "Nothing uploaded yet. Press Upload on a clip to get a link to share."));
                return;
            }
            for (idx, u) in self.store.uploads.iter().enumerate().rev() {
                if !q.is_empty() && !u.title.to_lowercase().contains(&q) && !u.url.to_lowercase().contains(&q) {
                    continue;
                }
                egui::Frame::new()
                    .fill(c.panel.0)
                    .stroke(Stroke::new(1.0, c.border.0))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(vec2(128.0, 72.0), Sense::hover());
                            ui.painter().rect_filled(r, 8.0, c.preview.0);
                            match self.thumbs.get_mut(&u.path) {
                                Some(Slot::Ready(tex, used)) => {
                                    *used = self.frame;
                                    ui.painter().add(
                                        egui::epaint::RectShape::filled(r, 8.0, Color32::WHITE)
                                            .with_texture(tex.id(), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))),
                                    );
                                }
                                Some(_) => {}
                                None if u.path.is_file() => {
                                    let ident = file_ident(&u.path);
                                    self.thumbs.insert(u.path.clone(), Slot::Pending);
                                    self.thumb_ident.insert(u.path.clone(), ident);
                                    let (tx, _) = self.worker.get_or_insert_with(start_worker);
                                    let _ = tx.send(Req::Thumb(u.path.clone(), ident));
                                }
                                None => {}
                            }
                            ui.add_space(8.0);
                            ui.vertical(|ui| {
                                ui.add(egui::Label::new(RichText::new(&u.title).strong()).truncate());
                                let when = LocalTime::from_system(UNIX_EPOCH + Duration::from_secs(u.uploaded_at))
                                    .map(|t| format!("{} {} {}, {:02}:{:02}", t.day, MONTHS[(t.month - 1) as usize], t.year, t.hour, t.minute))
                                    .unwrap_or_default();
                                ui.label(dim(shared, format!("{} · {} · {}", u.host.name(), when, fmt_bytes(u.bytes))).small());
                                let left = u.expires_at.map(|e| e.saturating_sub(crate::library::now()));
                                let color = match left {
                                    Some(0) => c.error.0,
                                    Some(s) if s < 86_400 => c.warning.0,
                                    _ => c.text_dim.0,
                                };
                                ui.label(RichText::new(u.expiry_text()).color(color).small());
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Forget").on_hover_text("Remove from this list (the upload itself stays online until it expires)").clicked() {
                                    forget = Some(idx);
                                }
                                if ui.add_enabled(!u.expired(), egui::Button::new("Open")).on_hover_text(&u.url).clicked() {
                                    crate::win::open_link(&u.url);
                                }
                                let just = self.copied.as_ref().is_some_and(|(url, t)| *url == u.url && t.elapsed() < Duration::from_millis(1500));
                                let copy_btn = egui::Button::new(RichText::new(if just { "Copied!" } else { "Copy link" }).color(c.accent_text.0)).fill(c.accent.0);
                                if ui.add_enabled(!u.expired(), copy_btn).clicked() {
                                    copy = Some(u.url.clone());
                                }
                            });
                        });
                    });
                ui.add_space(8.0);
            }
        });
        self.trim_textures();
        if let Some(url) = copy {
            self.apply(Action::Copy(url), ui.ctx(), shared);
            ui.ctx().request_repaint_after(Duration::from_millis(1600));
        }
        if let Some(i) = forget {
            self.store.uploads.remove(i);
            self.store.save();
        }
        if clear_expired {
            self.store.uploads.retain(|u| !u.expired());
            self.store.save();
        }
    }

    // ---- Settings > Uploads ---------------------------------------------------

    pub fn settings_section(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let mut hours = shared.settings.uploads.litterbox_hours;
        let email = credentials::email();
        let mut disconnect = false;
        let mut connect = false;
        super::section(ui, shared, "Uploads", |ui| {
            ui.label(RichText::new("Streamable").strong());
            match (&self.store.streamable, &email) {
                (Some(plan), Some(email)) => {
                    ui.horizontal(|ui| {
                        ui.label(format!("Connected as {email} ({} plan)", plan.name));
                        if ui.button("Disconnect").clicked() {
                            disconnect = true;
                        }
                    });
                    let keep = if plan.free {
                        format!("Free plan: clips up to 250 MB and 10 minutes, kept {} days.", streamable::FREE_KEEP_DAYS)
                    } else {
                        "Paid plan: clips are kept until you delete them.".to_string()
                    };
                    ui.label(RichText::new(keep).small());
                }
                _ => {
                    ui.horizontal(|ui| {
                        ui.label("Not connected.");
                        if ui.button("Log in with Streamable").clicked() {
                            connect = true;
                        }
                    });
                    ui.label(RichText::new("Without it, clips go to Litterbox: anonymous links that expire.").small());
                }
            }
            ui.add_space(10.0);
            ui.label(RichText::new("Litterbox").strong());
            ui.horizontal(|ui| {
                super::widgets::label_w(ui, "Links last", 110.0);
                egui::ComboBox::from_id_salt("litterbox-hours").selected_text(format!("{hours} hours")).show_ui(ui, |ui| {
                    for h in litterbox::HOURS {
                        ui.selectable_value(&mut hours, h, format!("{h} hours"));
                    }
                });
            });
            ui.label(
                RichText::new(
                    "Used when Streamable isn't connected, or a clip is too big or long for your plan (up to 1 GB). \
                     Anyone with the link can watch until it expires.",
                )
                .small(),
            );
        });
        if hours != shared.settings.uploads.litterbox_hours {
            shared.settings.uploads.litterbox_hours = hours;
            shared.mark_dirty();
        }
        if disconnect {
            credentials::delete();
            self.store.streamable = None;
            self.store.save();
        }
        if connect {
            self.open_login(false);
        }
    }

    // ---- the login card -------------------------------------------------------

    /// "Move <clip> to the Recycle Bin?"
    fn delete_dialog(&mut self, ctx: &egui::Context, shared: &Shared) {
        let Some(path) = self.confirm_delete.clone() else { return };
        let c = shared.theme.colors.clone();
        let online = self.store.uploads.iter().any(|u| u.path == path && !u.expired());
        let (mut yes, mut no) = (false, false);
        let resp = egui::Modal::new(egui::Id::unique("delete-clip")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(RichText::new("Delete clip?").family(crate::themes::brand_bold_family()).size(shared.theme.fonts.body + 5.0));
            ui.add_space(6.0);
            ui.label(format!("Move {} to the Recycle Bin?", Self::file_name(&path)));
            if online {
                ui.add_space(4.0);
                ui.label(RichText::new("Uploaded copies stay online.").color(c.text_dim.0));
            }
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                let b = egui::Button::new(RichText::new("Move to Recycle Bin").color(Color32::WHITE)).fill(c.error.0).min_size(vec2(0.0, 30.0));
                if ui.add(b).clicked() {
                    yes = true;
                }
                if ui.add(egui::Button::new("Cancel").min_size(vec2(0.0, 30.0))).clicked() {
                    no = true;
                }
            });
        });
        if yes {
            self.confirm_delete = None;
            if let Err(e) = self.delete(&path, shared) {
                log::warn!("{e}");
                self.toasts.push(e);
            }
        } else if no || resp.should_close() {
            self.confirm_delete = None;
        }
    }

    /// Draws the delete dialog and the Streamable login when they're open
    /// (call every frame).
    pub fn modals(&mut self, ctx: &egui::Context, shared: &Shared) {
        self.delete_dialog(ctx, shared);
        if !self.login.open {
            return;
        }
        let c = shared.theme.colors.clone();
        let busy = self.uploader.logging_in();
        let mut close = false;
        let mut skip = false;
        let mut submit = false;
        egui::Modal::new(egui::Id::unique("streamable-login")).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.label(RichText::new("Share clips with a link").family(crate::themes::brand_bold_family()).size(shared.theme.fonts.body + 5.0));
            ui.add_space(6.0);
            ui.label("Log in to Streamable for links that last. Or skip and use short-lived Litterbox links.");
            ui.add_space(12.0);
            if !self.login.expanded {
                let b = egui::Button::new(RichText::new("Log in with Streamable").color(c.accent_text.0).strong())
                    .fill(c.accent.0)
                    .min_size(vec2(ui.available_width(), 36.0));
                if ui.add(b).clicked() {
                    self.login.expanded = true;
                }
            } else {
                ui.label("Streamable email");
                let email = ui.add(egui::TextEdit::singleline(&mut self.login.email).hint_text("you@example.com").desired_width(f32::INFINITY));
                ui.add_space(4.0);
                ui.label("Streamable password");
                let pw = ui.add(egui::TextEdit::singleline(&mut self.login.password).password(true).desired_width(f32::INFINITY));
                if self.login.email.is_empty() && !busy {
                    email.request_focus();
                }
                let enter = pw.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.add_space(6.0);
                ui.label(RichText::new("Kept in Windows Credential Manager, only sent to streamable.com.").small().color(c.text_dim.0)).on_hover_text(
                    "Streamable has no official upload API, so Snag signs in the way its website does. \
                     A password you don't use anywhere else is safest. Signed up with Google? Set a password \
                     on streamable.com first (Snag never asks for Google logins).",
                );
                if let Some(e) = &self.login.error {
                    ui.add_space(4.0);
                    ui.label(RichText::new(e).color(c.error.0));
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let ready = !busy && self.login.email.contains('@') && !self.login.password.is_empty();
                    let b = egui::Button::new(RichText::new("Log in").color(c.accent_text.0)).fill(c.accent.0).min_size(vec2(90.0, 30.0));
                    if ui.add_enabled(ready, b).clicked() || (ready && enter) {
                        submit = true;
                    }
                    if busy {
                        ui.spinner();
                        ui.label(dim(shared, "Checking with Streamable"));
                    } else if !self.login.onboarding && ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(dim(shared, "No account?").small());
                let link = ui.link(RichText::new("Sign up on streamable.com").small()).on_hover_text("https://streamable.com/signup");
                if link.clicked() {
                    crate::win::shell_open("https://streamable.com/signup");
                }
            });
            if self.login.onboarding {
                ui.add_space(10.0);
                ui.vertical_centered(|ui| {
                    if ui.add_enabled(!busy, egui::Link::new(RichText::new("Skip").color(c.text_dim.0))).on_hover_text("You can connect later in Settings > Uploads").clicked() {
                        skip = true;
                    }
                });
            }
        });
        if submit {
            self.login.error = None;
            self.uploader.login(self.login.email.trim().to_string(), self.login.password.clone());
        }
        if skip {
            self.store.onboarding_done = true;
            self.store.save();
            close = true;
        }
        if close {
            self.login = LoginForm::default();
        }
    }
}

/// "Today", "Yesterday", "Monday", or "Monday, 9 Nov (2025)".
fn day_label(t: &LocalTime, today: i64) -> String {
    let d = t.day_number();
    let weekday = DAYS[t.weekday() as usize];
    match today - d {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => weekday.into(),
        _ if t.year == LocalTime::now().year => format!("{weekday}, {} {}", t.day, MONTHS[(t.month - 1) as usize]),
        _ => format!("{weekday}, {} {} {}", t.day, MONTHS[(t.month - 1) as usize], t.year),
    }
}

/// A clip's right-click menu (every view).
/// `can_delete`: the clip is in Snag's clips folder (imported folders'
/// files aren't deleted from here).
fn clip_menu(ui: &mut egui::Ui, path: &Path, favorite: bool, upload: Option<&UploadRecord>, can_delete: bool, actions: &mut Vec<Action>) {
    if ui.button("Open in the trimmer").clicked() {
        actions.push(Action::Open(path.to_path_buf()));
        ui.close();
    }
    if ui.button("Play").clicked() {
        actions.push(Action::Play(path.to_path_buf()));
        ui.close();
    }
    if ui.button("Show in folder").clicked() {
        actions.push(Action::Reveal(path.to_path_buf()));
        ui.close();
    }
    if ui.button(if favorite { "Remove from favorites" } else { "Add to favorites" }).clicked() {
        actions.push(Action::Favorite(path.to_path_buf()));
        ui.close();
    }
    ui.separator();
    if let Some(u) = upload {
        if ui.button("Copy link").clicked() {
            actions.push(Action::Copy(u.url.clone()));
            ui.close();
        }
        if ui.button("Open link").clicked() {
            crate::win::open_link(&u.url);
            ui.close();
        }
    }
    if ui.button("Upload to Litterbox (temporary link)").clicked() {
        actions.push(Action::Upload(path.to_path_buf(), Some(Host::Litterbox)));
        ui.close();
    }
    if can_delete {
        ui.separator();
        let red = ui.visuals().error_fg_color;
        if ui.button(RichText::new("Delete").color(red)).on_hover_text("Move to the Recycle Bin (Delete key)").clicked() {
            actions.push(Action::Delete(path.to_path_buf()));
            ui.close();
        }
    }
}

/// Grid | Compact | List, as three icon buttons.
fn view_switch(ui: &mut egui::Ui, theme: &crate::themes::Theme, view: &mut ClipView) {
    let c = &theme.colors;
    // Right to left: List is drawn first so the order reads Grid, Compact, List.
    for (v, tip) in [(ClipView::List, "List"), (ClipView::Compact, "Compact grid"), (ClipView::Grid, "Grid")] {
        let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 28.0), Sense::click());
        resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, *view == v, tip));
        let selected = *view == v;
        let bg = if selected {
            c.accent.0
        } else if resp.hovered() {
            c.widget_active.0
        } else {
            c.widget_hover.0
        };
        let fg = if selected { c.accent_text.0 } else { c.text.0 };
        let p = ui.painter();
        p.rect_filled(rect, 8.0, bg);
        let ctr = rect.center();
        match v {
            ClipView::Grid => {
                for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                    p.rect_filled(Rect::from_center_size(ctr + vec2(dx, dy) * 3.6, vec2(6.0, 6.0)), 1.5, fg);
                }
            }
            ClipView::Compact => {
                for dx in [-1.0, 0.0, 1.0] {
                    for dy in [-1.0, 0.0, 1.0] {
                        p.rect_filled(Rect::from_center_size(ctr + vec2(dx, dy) * 4.6, vec2(3.4, 3.4)), 0.8, fg);
                    }
                }
            }
            ClipView::List => {
                for dy in [-1.0, 0.0, 1.0] {
                    let y = ctr.y + dy * 4.6;
                    p.circle_filled(pos2(ctr.x - 6.0, y), 1.3, fg);
                    p.line_segment([pos2(ctr.x - 3.0, y), pos2(ctr.x + 7.5, y)], Stroke::new(1.6, fg));
                }
            }
        }
        if resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            *view = v;
        }
        ui.add_space(2.0);
    }
}

/// One side of the Clips | Uploads switch. The selected one is in the
/// accent color; the count shows while hovered.
fn segment(ui: &mut egui::Ui, theme: &crate::themes::Theme, name: &str, count: usize, selected: bool) -> Response {
    let c = &theme.colors;
    let (rect, resp) = ui.allocate_exact_size(vec2(150.0, 30.0), Sense::click());
    let p = ui.painter();
    let hovered = resp.hovered();
    if hovered {
        p.rect_filled(rect, 15.0, c.widget_hover.0);
    }
    let fg = if selected { c.accent.0 } else { c.text.0 };
    let name_font = egui::FontId::new(theme.fonts.body + 1.0, crate::themes::brand_bold_family());
    let a = p.layout_no_wrap(name.to_string(), name_font.clone(), fg);
    let b = p.layout_no_wrap(count.to_string(), egui::FontId::proportional(theme.fonts.body - 1.0), c.text_dim.0);
    let w = if hovered { a.size().x + 7.0 + b.size().x } else { a.size().x };
    let x = rect.center().x - w / 2.0;
    let aw = a.size().x;
    // Centered by what's drawn, not by the text boxes (two fonts, two
    // sizes): the name's capital letters and the number's digits share
    // the pill's middle line.
    let caps = p.layout_no_wrap("H".into(), name_font, fg).mesh_bounds.center().y;
    let ay = rect.center().y - caps;
    p.galley(pos2(x, ay), a, fg);
    if hovered {
        let by = rect.center().y - b.mesh_bounds.center().y;
        p.galley(pos2(x + aw + 7.0, by), b, c.text_dim.0);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A small pen (rename): a thick body, a pointed tip, a cap line.
fn paint_pen(p: &egui::Painter, r: Rect, col: Color32) {
    let tip = pos2(r.left() + 1.0, r.bottom() - 1.0);
    let end = pos2(r.right() - 1.0, r.top() + 1.0);
    let dir = (end - tip).normalized();
    let perp = vec2(dir.y, -dir.x);
    let a = tip + dir * 4.0; // where the tip meets the body
    let w = 2.4;
    p.add(Shape::convex_polygon(vec![a + perp * w, end + perp * w, end - perp * w, a - perp * w], col, Stroke::NONE));
    p.add(Shape::convex_polygon(vec![a + perp * w, a - perp * w, tip], col, Stroke::NONE));
    // The cap: a thin gap near the top end.
    let cap = end - dir * 3.5;
    p.line_segment([cap + perp * (w + 0.5), cap - perp * (w + 0.5)], Stroke::new(1.2, Color32::from_black_alpha(160)));
}

/// A filter pill: "Today 4".
fn pill(ui: &mut egui::Ui, theme: &crate::themes::Theme, name: &str, count: usize, selected: bool) -> Response {
    let c = &theme.colors;
    let font = egui::FontId::proportional(theme.fonts.body - 0.5);
    let (fg, sub, bg) = if selected {
        (c.accent_text.0, c.accent_text.0.gamma_multiply(0.75), c.accent.0)
    } else {
        (c.text.0, c.text_dim.0, c.widget_hover.0)
    };
    let a = ui.painter().layout_no_wrap(name.to_string(), font.clone(), fg);
    let b = ui.painter().layout_no_wrap(count.to_string(), font, sub);
    let size = vec2(a.size().x + b.size().x + 34.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let bg = if resp.hovered() && !selected { c.widget_active.0 } else { bg };
    let p = ui.painter();
    p.rect_filled(rect, 14.0, bg);
    let y = rect.center().y - a.size().y / 2.0;
    let aw = a.size().x;
    p.galley(pos2(rect.left() + 14.0, y), a, fg);
    p.galley(pos2(rect.left() + 20.0 + aw, y), b, sub);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A five-point star; filled with `fill`, or an outline.
fn paint_star(p: &egui::Painter, c: Pos2, r: f32, fill: Option<Color32>, outline: Color32) {
    let pt = |i: usize, rad: f32| {
        let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
        c + vec2(a.cos(), a.sin()) * rad
    };
    let pts: Vec<Pos2> = (0..10).map(|i| pt(i, if i % 2 == 0 { r } else { r * 0.45 })).collect();
    match fill {
        Some(col) => {
            // Convex pieces: a kite per point.
            for k in 0..5 {
                let outer = pts[2 * k];
                let before = pts[(2 * k + 9) % 10];
                let after = pts[2 * k + 1];
                p.add(Shape::convex_polygon(vec![c, before, outer, after], col, Stroke::NONE));
            }
        }
        None => {
            p.add(Shape::closed_line(pts, Stroke::new(1.4, outline)));
        }
    }
}

// ---- worker: lengths, games and thumbnails ------------------------------------

fn start_worker() -> (Sender<Req>, Receiver<Res>) {
    let (req_tx, req_rx) = channel::<Req>();
    let (res_tx, res_rx) = channel::<Res>();
    std::thread::Builder::new()
        .name("library".into())
        .spawn(move || {
            let mut thumbs: VecDeque<(PathBuf, Ident)> = VecDeque::new();
            let mut metas: VecDeque<PathBuf> = VecDeque::new();
            loop {
                if thumbs.is_empty() && metas.is_empty() {
                    match req_rx.recv() {
                        Ok(r) => queue(r, &mut thumbs, &mut metas),
                        Err(_) => return,
                    }
                }
                while let Ok(r) = req_rx.try_recv() {
                    queue(r, &mut thumbs, &mut metas);
                }
                // Thumbnails for cards on screen first (newest request first).
                let res = if let Some((path, ident)) = thumbs.pop_back() {
                    Res::Thumb(path.clone(), ident, make_thumb(&path, ident))
                } else if let Some(path) = metas.pop_front() {
                    let m = probe_meta(&path);
                    Res::Meta(path, m)
                } else {
                    continue;
                };
                if res_tx.send(res).is_err() {
                    return;
                }
            }
        })
        .ok();
    (req_tx, res_rx)
}

fn queue(r: Req, thumbs: &mut VecDeque<(PathBuf, Ident)>, metas: &mut VecDeque<PathBuf>) {
    match r {
        Req::Thumb(p, s) => thumbs.push_back((p, s)),
        Req::Meta(p) => metas.push_back(p),
    }
}

fn file_ident(path: &Path) -> Ident {
    std::fs::metadata(path).map(|m| (m.modified().map(unix).unwrap_or(0), m.len())).unwrap_or((0, 0))
}

fn probe_meta(path: &Path) -> Meta {
    let (stamp, size) = file_ident(path);
    let out = crate::win::hidden_command(crate::paths::ffprobe_exe())
        .args(["-v", "error", "-show_entries", "format=duration:format_tags=title", "-of", "json"])
        .arg(path)
        .output()
        .ok();
    let v: serde_json::Value = out.and_then(|o| serde_json::from_slice(&o.stdout).ok()).unwrap_or_default();
    let format = &v["format"];
    Meta {
        stamp,
        size,
        seconds: format["duration"].as_str().and_then(|s| s.parse().ok()),
        title: format["tags"]["title"].as_str().map(str::to_string).filter(|s| !s.trim().is_empty()),
    }
}

/// FNV-1a of the path, for thumbnail file names (same names in different
/// folders don't collide).
fn path_hash(p: &Path) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in p.to_string_lossy().to_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn make_thumb(path: &Path, (stamp, size): Ident) -> Option<(u32, u32, Vec<u8>)> {
    let dir = crate::paths::thumbs_dir();
    let _ = std::fs::create_dir_all(&dir);
    let png_path = dir.join(format!("{:016x}-{stamp}-{size}-{THUMB_W}.png", path_hash(path)));
    if !png_path.is_file() {
        // A frame a little way in (the very first can be black).
        let at = probe_meta(path).seconds.map(|s| (s / 3.0).min(2.0)).unwrap_or(0.0);
        let _ = crate::win::hidden_command(crate::paths::ffmpeg_exe())
            .args(["-v", "error", "-nostdin", "-y", "-ss", &format!("{at:.2}"), "-i"])
            .arg(path)
            .args(["-frames:v", "1", "-vf", &format!("scale={THUMB_W}:-2"), "-f", "image2", "-c:v", "png"])
            .arg(&png_path)
            .status();
    }
    read_png(&png_path)
}

/// A small PNG (a cached thumbnail) as RGBA.
fn read_png(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).ok()?));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..(w * h * 4) as usize].to_vec(),
        png::ColorType::Rgb => buf[..(w * h * 3) as usize].as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        _ => return None,
    };
    Some((w, h, rgba))
}
