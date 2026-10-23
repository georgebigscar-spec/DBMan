//! Search tab: finds a text in the names of a database's objects, in the
//! definitions of its views and routines, and in the values of its tables.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dbman_core::search::{self, HitKind};
use dbman_core::{Cell, ConnectionConfig, DbKind, DbObject, StatementOutcome};
use egui::{RichText, Ui};
use shadcn_egui::{Button, Checkbox, Column, DataTable, Input, Theme};

use crate::events::{Event, EventSender, TabEvent, TabId};
use crate::explorer::{Action, ConnState};
use crate::i18n::{tr, trf};
use crate::tabs::{TabCtx, error_alert, not_connected};

/// Rows counted per table at most; more show as "1000+".
const ROW_CAP: usize = 1000;

/// One place the text was found.
#[derive(Debug, Clone)]
pub struct Hit {
    pub kind: HitKind,
    pub object: DbObject,
    /// The matching line of a definition, or the number of matching rows.
    pub detail: String,
    /// For data hits, the `WHERE` condition that selects the matching rows.
    pub filter: Option<String>,
}

pub enum SearchEvent {
    Progress {
        done: usize,
        total: usize,
        current: String,
    },
    Found(Box<Hit>),
    /// A table or object that could not be read, and why.
    Skipped(String),
    Finished(Result<(), String>),
}

pub struct SearchView {
    cfg: ConnectionConfig,
    pub database: String,
    /// Only this schema, or the whole database.
    pub schema: Option<String>,
    pub text: String,
    pub names: bool,
    pub definitions: bool,
    pub data: bool,
    pub hits: Vec<Hit>,
    skipped: Vec<String>,
    running: Option<Arc<AtomicBool>>,
    progress: (usize, usize, String),
    error: Option<String>,
    /// The text of the last search, for the summary line.
    searched: String,
    version: u64,
}

impl SearchView {
    pub fn new(cfg: ConnectionConfig, database: String, schema: Option<String>) -> Self {
        Self {
            cfg,
            database,
            schema,
            text: String::new(),
            names: true,
            definitions: true,
            data: false,
            hits: Vec::new(),
            skipped: Vec::new(),
            running: None,
            progress: (0, 0, String::new()),
            error: None,
            searched: String::new(),
            version: 0,
        }
    }

    pub fn title(&self) -> String {
        trf("Search in {db}", &[("db", &self.schema.as_deref().unwrap_or(&self.database))])
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn on_event(&mut self, ev: TabEvent) {
        let TabEvent::Search(ev) = ev else { return };
        match *ev {
            SearchEvent::Progress { done, total, current } => self.progress = (done, total, current),
            SearchEvent::Found(hit) => {
                self.hits.push(*hit);
                self.version += 1;
            }
            SearchEvent::Skipped(what) => self.skipped.push(what),
            SearchEvent::Finished(result) => {
                self.running = None;
                self.error = result.err();
            }
        }
    }

    /// Starts a search for `text` with the ticked kinds, on a session of its own.
    pub fn start(&mut self, cx: &TabCtx<'_>, tab: TabId) {
        let needle = self.text.trim().to_string();
        if needle.is_empty() || !(self.names || self.definitions || self.data) {
            return;
        }
        self.stop();
        self.hits.clear();
        self.skipped.clear();
        self.error = None;
        self.searched = needle.clone();
        self.version += 1;
        let stop = Arc::new(AtomicBool::new(false));
        self.running = Some(stop.clone());
        self.progress = (0, 0, String::new());
        let job = Job {
            cfg: self.cfg.clone(),
            database: self.database.clone(),
            schema: self.schema.clone(),
            needle,
            names: self.names,
            definitions: self.definitions,
            data: self.data,
            stop,
            tx: cx.tasks.sender(),
            tab,
        };
        cx.tasks.runtime().spawn(async move {
            let (tx, tab) = (job.tx.clone(), job.tab);
            let result = job.run().await;
            tx.send(Event::Tab { tab, event: TabEvent::Search(Box::new(SearchEvent::Finished(result))) });
        });
    }

    pub fn stop(&mut self) {
        if let Some(stop) = self.running.take() {
            stop.store(true, Ordering::Relaxed);
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, tab: TabId, mut cx: TabCtx<'_>) {
        // The search runs on a session of its own, but what it opens needs the connection.
        if cx.conn.and_then(ConnState::session).is_none() {
            not_connected(ui, &mut cx);
            return;
        }
        let t = Theme::get(ui.ctx());
        let mut go = false;
        ui.horizontal(|ui| {
            let r = ui.add(Input::new(&mut self.text).placeholder(tr("Text to find")).width(320.0));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                go = true;
            }
            ui.add(Checkbox::new(&mut self.names).label(tr("Names")));
            ui.add(Checkbox::new(&mut self.definitions).label(tr("Definitions")));
            ui.add(Checkbox::new(&mut self.data).label(tr("Data")));
            if self.is_running() {
                if ui.add(Button::outline(tr("Stop"))).clicked() {
                    self.stop();
                }
            } else if ui.add(Button::new(tr("Search"))).clicked() {
                go = true;
            }
        });
        if go {
            self.start(&cx, tab);
        }
        ui.label(
            RichText::new(tr(
                "Names and definitions of tables, views, routines and triggers; data searches every text-like column of every table, which reads whole tables.",
            ))
            .size(12.0)
            .color(t.muted_foreground),
        );
        ui.add_space(6.0);
        if let Some(e) = &self.error {
            error_alert(ui, tr("The search failed"), e);
        }
        let (done, total, current) = &self.progress;
        if self.is_running() {
            ui.horizontal(|ui| {
                ui.add(shadcn_egui::Spinner::new().size(12.0));
                let text =
                    trf("{done} of {total}: {current}", &[("done", done), ("total", total), ("current", current)]);
                ui.label(RichText::new(text).color(t.muted_foreground));
            });
        } else if !self.searched.is_empty() {
            let mut summary = trf("{n} found for “{text}”", &[("n", &self.hits.len()), ("text", &self.searched)]);
            if !self.skipped.is_empty() {
                summary.push_str(&trf("; {n} could not be read", &[("n", &self.skipped.len())]));
            }
            ui.label(RichText::new(summary).color(t.muted_foreground)).on_hover_text(self.skipped.join("\n"));
        }
        ui.add_space(6.0);
        if self.hits.is_empty() {
            return;
        }
        let texts: Vec<Vec<String>> = self
            .hits
            .iter()
            .map(|h| {
                vec![
                    tr(h.kind.label()).to_string(),
                    tr(h.object.kind.label()).to_string(),
                    h.object.path.schema.clone(),
                    h.object.path.name.clone(),
                    h.detail.clone(),
                ]
            })
            .collect();
        let columns = vec![
            Column::new(tr("Found in")).sortable().weight(0.6),
            Column::new(tr("Type")).sortable().weight(0.6),
            Column::new(tr("Schema")).sortable().weight(0.8),
            Column::new(tr("Name")).sortable(),
            Column::new(tr("Match")).weight(3.0),
        ];
        let resp = DataTable::new(("search", tab), columns)
            .filter(tr("Filter results…"))
            .row_height(28.0)
            .resizable()
            .version(self.version)
            .max_height(ui.available_height() - 40.0)
            .show(ui, &texts);
        if let Some(h) = resp.clicked.and_then(|i| self.hits.get(i)) {
            cx.actions.push(match (&h.filter, h.kind) {
                (Some(f), HitKind::Data) => Action::OpenDataFiltered(cx.conn_id, h.object.clone(), f.clone()),
                _ => Action::OpenObject(cx.conn_id, h.object.clone()),
            });
        }
        ui.label(
            RichText::new(tr("A click opens the object, or for data the table filtered to the matching rows."))
                .size(12.0)
                .color(t.muted_foreground),
        );
    }
}

/// The search itself, off the UI thread.
struct Job {
    cfg: ConnectionConfig,
    database: String,
    schema: Option<String>,
    needle: String,
    names: bool,
    definitions: bool,
    data: bool,
    stop: Arc<AtomicBool>,
    tx: EventSender,
    tab: TabId,
}

impl Job {
    fn send(&self, ev: SearchEvent) {
        self.tx.send(Event::Tab { tab: self.tab, event: TabEvent::Search(Box::new(ev)) });
    }

    fn found(&self, kind: HitKind, object: &DbObject, detail: String, filter: Option<String>) {
        self.send(SearchEvent::Found(Box::new(Hit { kind, object: object.clone(), detail, filter })));
    }

    async fn run(&self) -> Result<(), String> {
        let conn = dbman_drivers::connect(&self.cfg).await.map_err(|e| e.to_string())?;
        let db = self.database.as_str();
        let schemas = match &self.schema {
            Some(s) => vec![s.clone()],
            None => conn.schemas(db).await.map_err(|e| e.to_string())?,
        };
        let mut objects = Vec::new();
        for s in &schemas {
            match conn.objects(db, s).await {
                Ok(o) => objects.extend(o),
                Err(e) => self.send(SearchEvent::Skipped(format!("{s}: {e}"))),
            }
        }
        let kind: DbKind = conn.kind();
        let total = objects.len();
        for (i, o) in objects.iter().enumerate() {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            self.send(SearchEvent::Progress { done: i, total, current: o.path.name.clone() });
            if self.names && search::name_matches(&o.path.name, &self.needle) {
                self.found(HitKind::Name, o, String::new(), None);
            }
            if self.definitions && search::has_definition(o.kind) {
                match conn.ddl(o).await {
                    Ok(text) => {
                        if let Some((line, s)) = search::excerpt(&text, &self.needle, 120) {
                            self.found(
                                HitKind::Definition,
                                o,
                                trf("line {n}: {text}", &[("n", &line), ("text", &s)]),
                                None,
                            );
                        }
                    }
                    Err(e) => self.send(SearchEvent::Skipped(format!("{}: {e}", o.path.name))),
                }
            }
            if self.data && search::has_rows(o.kind) {
                let details = match conn.table_details(&o.path).await {
                    Ok(d) => d,
                    Err(e) => {
                        self.send(SearchEvent::Skipped(format!("{}: {e}", o.path.name)));
                        continue;
                    }
                };
                let columns: Vec<String> = details
                    .columns
                    .iter()
                    .filter(|c| search::searchable_type(&c.data_type))
                    .map(|c| c.name.clone())
                    .collect();
                if columns.is_empty() {
                    continue;
                }
                let filter = search::value_filter(kind, &columns, &self.needle);
                let sql = search::count_sql(kind, &o.path, &filter, ROW_CAP);
                match conn.execute(Some(db), &sql, 1).await {
                    Ok(StatementOutcome::Rows(r)) => {
                        let n: usize = match r.rows.first().and_then(|row| row.first()) {
                            Some(Cell::Text(t)) => t.trim().parse().unwrap_or(0),
                            _ => 0,
                        };
                        if n > 0 {
                            let label = if n > ROW_CAP { format!("{ROW_CAP}+") } else { n.to_string() };
                            self.found(HitKind::Data, o, trf("Matching rows: {n}", &[("n", &label)]), Some(filter));
                        }
                    }
                    Ok(_) => {}
                    Err(e) => self.send(SearchEvent::Skipped(format!("{}: {e}", o.path.name))),
                }
            }
        }
        self.send(SearchEvent::Progress { done: total, total, current: String::new() });
        Ok(())
    }
}
