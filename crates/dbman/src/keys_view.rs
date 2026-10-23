//! Keys tab of a Redis database: a `SCAN` of the keys grouped by their `:`
//! prefixes, and the type, time to live and value of the key picked, with its
//! string value, expiry and existence editable.

use std::sync::Arc;

use dbman_core::redis::{self as core_redis, KeyNode};
use dbman_core::{Connection, DbResult};
use dbman_drivers::redis::{self, KeyInfo, KeyValue, Scan};
use egui::{RichText, Ui};
use shadcn_egui::{Badge, Button, ButtonSize, Column, DataTable, Input, Resizable, Theme, Tree, TreeUi, alert_dialog};

use crate::events::{Event, TabEvent, TabId};
use crate::explorer::{Action, ConnState};
use crate::i18n::{tr, trf};
use crate::tabs::{TabCtx, error_alert, not_connected};

/// Keys read per scan; "Load more" reads the next ones.
const PAGE: usize = 500;
/// Items of a hash, list, set, sorted set or stream shown at most.
const ITEMS: usize = 1000;
/// Bytes of a binary value shown as hex.
const HEX: usize = 4096;

pub enum KeysEvent {
    /// A scan page; `more` appends it to the keys already read.
    Scanned {
        result: DbResult<Scan>,
        more: bool,
    },
    Size(DbResult<i64>),
    Loaded(DbResult<KeyInfo>),
    /// A change to the key: it is read again, or dropped from the list when deleted.
    Changed {
        result: DbResult<()>,
        deleted: bool,
    },
}

pub struct KeysView {
    pub database: String,
    read_only: bool,
    pub pattern: String,
    pub keys: Vec<String>,
    nodes: Vec<KeyNode>,
    /// Where the next scan page starts; "0" when the scan is over.
    cursor: String,
    scanned: bool,
    scanning: bool,
    size: Option<i64>,
    /// The key picked in the tree (`Some(key)`), or a group (`None` inside).
    selected: Option<(Option<String>, String)>,
    pub key: Option<KeyInfo>,
    loading: bool,
    /// The string value being edited.
    pub edit: String,
    /// Seconds typed for "Set TTL".
    ttl: String,
    confirm_delete: bool,
    error: Option<String>,
    sizes: [f32; 2],
}

impl KeysView {
    pub fn new(database: String, read_only: bool) -> Self {
        Self {
            database,
            read_only,
            pattern: String::new(),
            keys: Vec::new(),
            nodes: Vec::new(),
            cursor: "0".into(),
            scanned: false,
            scanning: false,
            size: None,
            selected: None,
            key: None,
            loading: false,
            edit: String::new(),
            ttl: String::new(),
            confirm_delete: false,
            error: None,
            sizes: [0.35, 0.65],
        }
    }

    pub fn title(&self) -> String {
        trf("Keys in {db}", &[("db", &self.database)])
    }

    /// A scan or a key read or change is under way.
    pub fn is_busy(&self) -> bool {
        self.scanning || self.loading
    }

    pub fn on_event(&mut self, ev: TabEvent) -> Vec<Action> {
        let TabEvent::Keys(ev) = ev else { return Vec::new() };
        match *ev {
            KeysEvent::Scanned { result, more } => {
                self.scanning = false;
                match result {
                    Ok(scan) => {
                        if !more {
                            self.keys.clear();
                        }
                        self.keys.extend(scan.keys);
                        self.keys.sort_unstable();
                        self.keys.dedup();
                        self.cursor = scan.cursor;
                        self.nodes = core_redis::tree(&self.keys, ":");
                    }
                    Err(e) => self.error = Some(e.message),
                }
            }
            KeysEvent::Size(result) => self.size = result.ok(),
            KeysEvent::Loaded(result) => {
                self.loading = false;
                match result {
                    Ok(k) => {
                        if let KeyValue::Text(t) = &k.value {
                            self.edit = t.clone();
                        }
                        self.key = Some(k);
                    }
                    Err(e) => self.error = Some(e.message),
                }
            }
            KeysEvent::Changed { result: Err(e), .. } => {
                self.loading = false;
                self.error = Some(e.message);
            }
            KeysEvent::Changed { result: Ok(()), deleted } => {
                self.loading = false;
                if deleted && let Some(k) = self.key.take() {
                    self.keys.retain(|x| *x != k.key);
                    self.nodes = core_redis::tree(&self.keys, ":");
                    self.selected = None;
                    self.size = self.size.map(|n| n - 1);
                }
            }
        }
        Vec::new()
    }

    fn spawn(cx: &TabCtx<'_>, tab: TabId, fut: impl Future<Output = KeysEvent> + Send + 'static) {
        cx.tasks.spawn(async move { Event::Tab { tab, event: TabEvent::Keys(Box::new(fut.await)) } });
    }

    /// Scans from the start (`more` false) or on from where the last page stopped.
    pub fn scan(&mut self, cx: &TabCtx<'_>, tab: TabId, c: Arc<dyn Connection>, more: bool) {
        self.scanned = true;
        self.scanning = true;
        self.error = None;
        let (db, pattern) = (self.database.clone(), self.pattern.clone());
        let cursor = if more { self.cursor.clone() } else { "0".to_string() };
        if !more {
            let (c, db) = (c.clone(), db.clone());
            Self::spawn(cx, tab, async move { KeysEvent::Size(redis::dbsize(c.as_ref(), &db).await) });
        }
        Self::spawn(cx, tab, async move {
            KeysEvent::Scanned { result: redis::scan(c.as_ref(), &db, &cursor, &pattern, PAGE).await, more }
        });
    }

    fn load(&mut self, cx: &TabCtx<'_>, tab: TabId, c: Arc<dyn Connection>, key: String) {
        self.loading = true;
        self.error = None;
        let db = self.database.clone();
        Self::spawn(cx, tab, async move { KeysEvent::Loaded(redis::load_key(c.as_ref(), &db, &key, ITEMS).await) });
    }

    /// Runs a change to the shown key, then reads it again unless it was deleted.
    fn change(&mut self, cx: &TabCtx<'_>, tab: TabId, c: Arc<dyn Connection>, edit: Edit) {
        let Some(key) = self.key.as_ref().map(|k| k.key.clone()) else { return };
        self.loading = true;
        self.error = None;
        let db = self.database.clone();
        Self::spawn(cx, tab, async move {
            let c = c.as_ref();
            let deleted = matches!(edit, Edit::Delete);
            let result = match edit {
                Edit::Save(value) => redis::set_string(c, &db, &key, &value).await,
                Edit::Expire(seconds) => redis::expire(c, &db, &key, seconds).await,
                Edit::Delete => redis::delete(c, &db, std::slice::from_ref(&key)).await.map(|_| ()),
            };
            if result.is_ok() && !deleted {
                return KeysEvent::Loaded(redis::load_key(c, &db, &key, ITEMS).await);
            }
            KeysEvent::Changed { result, deleted }
        });
    }

    pub fn ui(&mut self, ui: &mut Ui, tab: TabId, mut cx: TabCtx<'_>) {
        let Some(c) = cx.conn.and_then(ConnState::session) else {
            not_connected(ui, &mut cx);
            return;
        };
        if !self.scanned {
            self.scan(&cx, tab, c.clone(), false);
        }
        let t = Theme::get(ui.ctx());
        let mut go = false;
        ui.horizontal(|ui| {
            let r = ui.add(Input::new(&mut self.pattern).placeholder(tr("Pattern, e.g. user:*")).width(260.0));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                go = true;
            }
            if ui.add_enabled(!self.scanning, Button::new(tr("Scan")).size(ButtonSize::Sm)).clicked() {
                go = true;
            }
            if self.cursor != "0"
                && ui.add_enabled(!self.scanning, Button::outline(tr("Load more")).size(ButtonSize::Sm)).clicked()
            {
                self.scan(&cx, tab, c.clone(), true);
            }
            if self.scanning {
                ui.add(shadcn_egui::Spinner::new().size(14.0));
            }
            let mut summary = trf("{n} keys shown", &[("n", &self.keys.len())]);
            if let Some(n) = self.size {
                summary.push_str(&trf(" of {n} in the database", &[("n", &n)]));
            }
            ui.label(RichText::new(summary).color(t.muted_foreground));
        });
        if go {
            self.scan(&cx, tab, c.clone(), false);
        }
        ui.add_space(6.0);
        if let Some(e) = &self.error {
            error_alert(ui, tr("The command failed"), e);
        }
        let mut sizes = self.sizes;
        let mut picked = None;
        Resizable::horizontal(("keys", tab), &mut sizes).min_size(160.0).with_handle().show(ui, |ui, panel| {
            if panel == 0 {
                picked = self.tree(ui, tab);
            } else {
                egui::Frame::NONE.inner_margin(egui::Margin { left: 10, ..Default::default() }).show(ui, |ui| {
                    egui::ScrollArea::vertical().id_salt(("key", tab)).auto_shrink([false, false]).show(ui, |ui| {
                        self.details(ui, tab, &cx, &c);
                    });
                });
            }
        });
        self.sizes = sizes;
        if let Some(key) = picked {
            self.load(&cx, tab, c.clone(), key);
        }
        if self.confirm_delete {
            let key = self.key.as_ref().map_or("", |k| k.key.as_str());
            match alert_dialog(
                ui.ctx(),
                ("keys-delete", tab),
                tr("Delete key?"),
                &trf("\"{key}\" and its value will be deleted from {db}.", &[("key", &key), ("db", &self.database)]),
                tr("Delete"),
            ) {
                Some(true) => {
                    self.confirm_delete = false;
                    self.change(&cx, tab, c, Edit::Delete);
                }
                Some(false) => self.confirm_delete = false,
                None => {}
            }
        }
    }

    /// The key tree; returns the key clicked.
    fn tree(&mut self, ui: &mut Ui, tab: TabId) -> Option<String> {
        let t = Theme::get(ui.ctx());
        if self.nodes.is_empty() && !self.scanning {
            ui.label(RichText::new(tr("No keys match.")).color(t.muted_foreground));
            return None;
        }
        let before = self.selected.clone();
        egui::ScrollArea::vertical().id_salt(("key-tree", tab)).auto_shrink([false, false]).show(ui, |ui| {
            Tree::new(("key-tree", tab)).default_open_depth(0).show(ui, &mut self.selected, |tree| {
                nodes(tree, &self.nodes, "");
            });
        });
        match &self.selected {
            Some((Some(key), _)) if self.selected != before => Some(key.clone()),
            _ => None,
        }
    }

    fn details(&mut self, ui: &mut Ui, tab: TabId, cx: &TabCtx<'_>, c: &Arc<dyn Connection>) {
        let t = Theme::get(ui.ctx());
        let Some(k) = &self.key else {
            let text = if self.loading { tr("Loading…") } else { tr("Pick a key to see its value.") };
            ui.label(RichText::new(text).color(t.muted_foreground));
            return;
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(&k.key).strong().size(15.0));
            if self.loading {
                ui.add(shadcn_egui::Spinner::new().size(12.0));
            }
        });
        if !k.exists() {
            ui.label(RichText::new(tr("The key no longer exists.")).color(t.muted_foreground));
            return;
        }
        let ttl = if k.ttl < 0 { tr("no expiry").to_string() } else { core_redis::ttl_label(k.ttl) };
        ui.horizontal(|ui| {
            ui.add(Badge::new(&k.kind).secondary());
            if let Some(n) = k.len {
                ui.label(RichText::new(trf("length {n}", &[("n", &n)])).color(t.muted_foreground));
            }
            ui.label(RichText::new(trf("TTL: {ttl}", &[("ttl", &ttl)])).color(t.muted_foreground));
        });
        let (ro, has_ttl) = (self.read_only, k.ttl >= 0);
        let mut action = None;
        if !ro {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add(Input::new(&mut self.ttl).placeholder(tr("Seconds")).width(90.0));
                let seconds = self.ttl.trim().parse::<i64>().ok().filter(|s| *s > 0);
                if ui.add_enabled(seconds.is_some(), Button::outline(tr("Set TTL")).size(ButtonSize::Sm)).clicked() {
                    action = Some(Edit::Expire(seconds));
                }
                if has_ttl && ui.add(Button::outline(tr("Remove TTL")).size(ButtonSize::Sm)).clicked() {
                    action = Some(Edit::Expire(None));
                }
                if ui.add(Button::destructive(tr("Delete key")).size(ButtonSize::Sm)).clicked() {
                    self.confirm_delete = true;
                }
            });
        }
        ui.add_space(8.0);
        let k = self.key.as_ref().expect("checked above");
        match &k.value {
            KeyValue::Text(text) if k.kind == "string" && !ro => {
                ui.add(
                    egui::TextEdit::multiline(&mut self.edit)
                        .font(egui::TextStyle::Monospace)
                        .desired_rows(8)
                        .desired_width(f32::INFINITY),
                );
                let changed = self.edit != *text;
                ui.horizontal(|ui| {
                    if ui.add_enabled(changed, Button::new(tr("Save")).size(ButtonSize::Sm)).clicked() {
                        action = Some(Edit::Save(self.edit.clone()));
                    }
                    if changed && ui.add(Button::ghost(tr("Revert")).size(ButtonSize::Sm)).clicked() {
                        self.edit = text.clone();
                    }
                });
            }
            KeyValue::Text(text) => {
                let full = dbman_core::value::pretty_json(text).unwrap_or_else(|| text.clone());
                let mut view = full.as_str();
                ui.add(
                    egui::TextEdit::multiline(&mut view).font(egui::TextStyle::Monospace).desired_width(f32::INFINITY),
                );
            }
            KeyValue::Bytes(b) => {
                ui.label(
                    RichText::new(trf("Binary value, {size}.", &[("size", &dbman_core::value::size_label(b.len()))]))
                        .color(t.muted_foreground),
                );
                let full = dbman_core::value::hex_dump(b, HEX);
                let mut view = full.as_str();
                ui.add(
                    egui::TextEdit::multiline(&mut view).font(egui::TextStyle::Monospace).desired_width(f32::INFINITY),
                );
            }
            KeyValue::Rows { columns, rows } => {
                let cols: Vec<Column> = columns.iter().map(|c| Column::new(c.as_str()).sortable()).collect();
                DataTable::new(("key-rows", tab, &k.key), cols)
                    .filter(tr("Filter…"))
                    .row_height(26.0)
                    .resizable()
                    .show(ui, rows);
                if let Some(n) = k.len
                    && (rows.len() as i64) < n
                {
                    ui.label(
                        RichText::new(trf("Showing the first {shown} of {n}.", &[("shown", &rows.len()), ("n", &n)]))
                            .color(t.muted_foreground),
                    );
                }
            }
            KeyValue::Other => {
                ui.label(
                    RichText::new(tr("DBMan does not show this type; a console can read it."))
                        .color(t.muted_foreground),
                );
            }
        }
        if let Some(edit) = action {
            if matches!(edit, Edit::Expire(_)) {
                self.ttl.clear();
            }
            self.change(cx, tab, c.clone(), edit);
        }
    }
}

enum Edit {
    Save(String),
    /// Seconds to live, or none to keep the key for good.
    Expire(Option<i64>),
    Delete,
}

/// Tree rows: a group is (None, its prefix), a key is (Some(key), key).
fn nodes(tree: &mut TreeUi<'_, (Option<String>, String)>, list: &[KeyNode], prefix: &str) {
    for n in list {
        match &n.key {
            Some(key) => {
                tree.leaf_ui((Some(key.clone()), key.clone()), key, |ui| {
                    ui.add(egui::Label::new(&n.label).selectable(false).truncate());
                });
            }
            None => {
                let path = format!("{prefix}{}:", n.label);
                tree.folder_ui(
                    (None, path.clone()),
                    &path,
                    |ui| {
                        let t = Theme::get(ui.ctx());
                        ui.add(egui::Label::new(&n.label).selectable(false));
                        ui.label(RichText::new(n.count.to_string()).size(12.0).color(t.muted_foreground));
                    },
                    |tree| nodes(tree, &n.children, &path),
                );
            }
        }
    }
}
