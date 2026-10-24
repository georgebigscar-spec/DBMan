//! Aggregation pipeline builder of a MongoDB collection: stages added one
//! after another, each with its operator, its body typed as in the shell and
//! a preview of the documents coming out of it; a list of the operators to
//! insert; and the finished pipeline exported as code.

use std::sync::Arc;

use dbman_core::aggregation::{self, Lang, STAGES, Stage};
use dbman_core::mongo::{self, Call, Reply, Shell};
use dbman_core::{Connection, DbKind, DbObject, DbResult};
use egui::{RichText, Ui};
use serde_json::Value;
use shadcn_egui::{Alert, Button, ButtonSize, Dialog, Input, Select, Switch, Theme, footer, toast};

use crate::events::{Event, TabEvent, TabId};
use crate::explorer::{Action, ConnState};
use crate::i18n::{tr, trf};
use crate::sql_editor;
use crate::tabs::{TabCtx, not_connected, toolbar_button};

const SAMPLES: [u64; 4] = [5, 10, 20, 50];
/// How long typing pauses before the previews run again.
const DEBOUNCE: f64 = 0.6;

pub enum PipelineEvent {
    Preview { index: usize, generation: u64, result: DbResult<(Vec<Value>, bool)> },
}

/// What a stage's preview shows.
pub enum Preview {
    Idle,
    Loading,
    Ready {
        docs: Vec<Value>,
        more: bool,
    },
    Failed(String),
    /// The stage, or one before it, writes to a collection.
    Writes,
    Disabled,
}

pub struct PipelineStage {
    pub stage: Stage,
    pub preview: Preview,
    generation: u64,
}

struct Export {
    lang: usize,
}

struct Import {
    text: String,
    error: Option<String>,
}

pub struct PipelineView {
    pub object: DbObject,
    pub stages: Vec<PipelineStage>,
    sample: usize,
    generation: u64,
    /// The first stage whose preview is out of date, and since when.
    dirty: Option<(usize, f64)>,
    /// The stage operators are inserted into.
    pub focused: Option<usize>,
    pub operator_query: String,
    show_operators: bool,
    add_choice: Option<usize>,
    export: Option<Export>,
    import: Option<Import>,
    /// The tab the view was last drawn in, which names its editors.
    tab: Option<TabId>,
}

impl PipelineView {
    /// A pipeline on `object` starting with a `$match` on `filter` (the shell's
    /// syntax), or the template one.
    pub fn new(object: DbObject, filter: Option<String>) -> Self {
        let mut first = Stage::new("$match");
        if let Some(f) = filter.filter(|f| !f.trim().is_empty()) {
            first.body = f;
        }
        Self {
            object,
            stages: vec![PipelineStage { stage: first, preview: Preview::Idle, generation: 0 }],
            sample: 1,
            generation: 0,
            dirty: Some((0, 0.0)),
            focused: Some(0),
            operator_query: String::new(),
            show_operators: true,
            add_choice: None,
            export: None,
            import: None,
            tab: None,
        }
    }

    pub fn title(&self) -> String {
        trf("Pipeline on {name}", &[("name", &self.object.path.name)])
    }

    pub fn is_busy(&self) -> bool {
        self.dirty.is_some() || self.stages.iter().any(|s| matches!(s.preview, Preview::Loading))
    }

    /// The documents of stage `i`'s preview, once read.
    pub fn preview_docs(&self, i: usize) -> Option<&[Value]> {
        match &self.stages.get(i)?.preview {
            Preview::Ready { docs, .. } => Some(docs),
            _ => None,
        }
    }

    /// The error of stage `i`'s preview, if it failed.
    pub fn preview_error(&self, i: usize) -> Option<&str> {
        match &self.stages.get(i)?.preview {
            Preview::Failed(e) => Some(e),
            _ => None,
        }
    }

    /// Marks the previews from stage `i` on as out of date; `now` runs them
    /// at once, else after a pause in typing.
    fn changed(&mut self, i: usize, at: f64) {
        let from = self.dirty.map_or(i, |(d, _)| d.min(i));
        self.dirty = Some((from, at));
    }

    pub fn add_stage(&mut self, name: &str) {
        self.stages.push(PipelineStage { stage: Stage::new(name), preview: Preview::Idle, generation: 0 });
        self.focused = Some(self.stages.len() - 1);
        self.changed(self.stages.len() - 1, 0.0);
    }

    pub fn set_body(&mut self, i: usize, body: &str) {
        self.stages[i].stage.body = body.into();
        self.changed(i, 0.0);
    }

    pub fn set_enabled(&mut self, i: usize, on: bool) {
        self.stages[i].stage.enabled = on;
        self.changed(i, 0.0);
    }

    pub fn remove_stage(&mut self, i: usize) {
        self.stages.remove(i);
        self.focused = None;
        if !self.stages.is_empty() {
            self.changed(i.min(self.stages.len() - 1), 0.0);
        }
    }

    fn swap(&mut self, a: usize, b: usize) {
        self.stages.swap(a, b);
        self.changed(a.min(b), 0.0);
    }

    fn raw_stages(&self) -> Vec<Stage> {
        self.stages.iter().map(|s| s.stage.clone()).collect()
    }

    /// The whole pipeline, or the stage (from 0) that does not parse.
    fn pipeline(&self) -> Result<Vec<Value>, (usize, String)> {
        let stages = self.raw_stages();
        match aggregation::pipeline(&stages, stages.len().saturating_sub(1))? {
            Value::Array(a) if !stages.is_empty() => Ok(a),
            _ => Ok(Vec::new()),
        }
    }

    /// The pipeline as code in `lang`, or why it cannot be.
    pub fn code(&self, lang: Lang, uri: &str) -> Result<String, String> {
        let p = self.pipeline().map_err(|(i, e)| stage_error(i, &e))?;
        Ok(aggregation::export(lang, uri, &self.object.path.database, &self.object.path.name, &p))
    }

    /// The aggregate as a console statement.
    fn statement(&self) -> Result<String, String> {
        let p = self.pipeline().map_err(|(i, e)| stage_error(i, &e))?;
        Ok(format!(
            "{}.aggregate({})",
            mongo::collection_ref(&self.object.path.name),
            mongo::to_shell(&Value::Array(p))
        ))
    }

    /// Opens the import dialog with the pipeline as text.
    pub fn open_import(&mut self) {
        let text = self
            .statement()
            .ok()
            .and_then(|s| s.split_once(".aggregate(").map(|(_, p)| p.trim_end_matches(')').to_string()));
        self.import = Some(Import { text: text.unwrap_or_else(|| "[]".into()), error: None });
    }

    /// Replaces the stages with those of a pipeline typed as a shell array.
    pub fn import_text(&mut self, text: &str) -> Result<(), String> {
        let stages = aggregation::stages_from_text(text)?;
        self.stages =
            stages.into_iter().map(|stage| PipelineStage { stage, preview: Preview::Idle, generation: 0 }).collect();
        self.focused = None;
        if !self.stages.is_empty() {
            self.changed(0, 0.0);
        }
        Ok(())
    }

    pub fn open_export(&mut self, lang: Lang) {
        self.export = Some(Export { lang: Lang::ALL.iter().position(|l| *l == lang).unwrap_or(0) });
    }

    pub fn on_event(&mut self, ev: TabEvent) {
        let TabEvent::Pipeline(ev) = ev else { return };
        let PipelineEvent::Preview { index, generation, result } = *ev;
        let Some(s) = self.stages.get_mut(index) else { return };
        if s.generation != generation {
            return;
        }
        s.preview = match result {
            Ok((docs, more)) => Preview::Ready { docs, more },
            Err(e) => Preview::Failed(e.message),
        };
    }

    /// Runs the previews from stage `from` on.
    fn run_previews(&mut self, from: usize, cx: &TabCtx<'_>, tab: TabId, session: &Arc<dyn Connection>) {
        let stages = self.raw_stages();
        let sample = SAMPLES[self.sample];
        for i in from..self.stages.len() {
            self.generation += 1;
            let generation = self.generation;
            let s = &mut self.stages[i];
            s.generation = generation;
            if !s.stage.enabled {
                s.preview = Preview::Disabled;
                continue;
            }
            match aggregation::preview(&stages, i, sample) {
                Err((k, e)) if k == i => s.preview = Preview::Failed(e),
                Err((k, _)) => {
                    s.preview = Preview::Failed(trf("Stage {n} has an error.", &[("n", &(k + 1))]));
                }
                Ok(None) => s.preview = Preview::Writes,
                Ok(Some(pipeline)) => {
                    s.preview = Preview::Loading;
                    let statement = Shell::Collection {
                        name: self.object.path.name.clone(),
                        call: Call { method: "aggregate".into(), args: vec![pipeline] },
                        chain: vec![],
                    };
                    let db = self.object.path.database.clone();
                    let session = session.clone();
                    cx.tasks.spawn(async move {
                        let result = session.mongo(Some(&db), &statement, sample as usize).await.map(|r| match r {
                            Reply::Documents { docs, more } => (docs, more),
                            Reply::Value(v) => (vec![v], false),
                            Reply::Written { .. } => (Vec::new(), false),
                        });
                        let event = PipelineEvent::Preview { index: i, generation, result };
                        Event::Tab { tab, event: TabEvent::Pipeline(Box::new(event)) }
                    });
                }
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, tab: TabId, mut cx: TabCtx<'_>) {
        let Some(session) = cx.conn.and_then(ConnState::session) else {
            not_connected(ui, &mut cx);
            return;
        };
        self.tab = Some(tab);
        let now = ui.input(|i| i.time);
        if let Some((from, at)) = self.dirty {
            if now - at >= DEBOUNCE {
                self.dirty = None;
                self.run_previews(from, &cx, tab, &session);
            } else {
                ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(DEBOUNCE - (now - at)));
            }
        }
        let uri = cx.conn.map(|c| connection_uri(&c.cfg)).unwrap_or_default();
        self.toolbar(ui, &mut cx);
        ui.add_space(6.0);
        if self.show_operators {
            egui::Panel::right(egui::Id::new(("pipeline-operators", tab)))
                .resizable(true)
                .default_size(250.0)
                .min_size(180.0)
                .show(ui, |ui| self.operators_ui(ui, tab, now));
        }
        self.stages_ui(ui, tab, now);
        self.dialogs(ui, tab, &uri);
    }

    fn toolbar(&mut self, ui: &mut Ui, cx: &mut TabCtx<'_>) {
        let t = Theme::get(ui.ctx());
        ui.horizontal(|ui| {
            let names: Vec<&str> = STAGES.iter().map(|s| s.name).collect();
            ui.add(Select::new(&mut self.add_choice, &names).placeholder(tr("Stage…")).width(150.0));
            let choice = self.add_choice.unwrap_or(0);
            if ui.add(Button::new(tr("+ Add stage")).size(ButtonSize::Sm)).clicked() {
                self.add_stage(STAGES[choice].name);
            }
            ui.add_space(8.0);
            ui.label(RichText::new(tr("Sample")).color(t.muted_foreground));
            let sizes: Vec<String> = SAMPLES.iter().map(|n| n.to_string()).collect();
            let refs: Vec<&str> = sizes.iter().map(String::as_str).collect();
            let mut size = Some(self.sample);
            ui.add(Select::new(&mut size, &refs).width(64.0));
            if let Some(s) = size.filter(|s| *s != self.sample) {
                self.sample = s;
                self.changed(0, 0.0);
            }
            ui.add_space(8.0);
            if toolbar_button(ui, tr("Export to code")).clicked() {
                self.open_export(Lang::Shell);
            }
            if toolbar_button(ui, tr("Import pipeline…")).clicked() {
                self.open_import();
            }
            if toolbar_button(ui, tr("Open in console")).clicked() {
                match self.statement() {
                    Ok(s) => {
                        let db = Some(self.object.path.database.clone());
                        cx.actions.push(Action::OpenConsoleWith(cx.conn_id, db, s));
                    }
                    Err(e) => toast(ui.ctx(), e, None),
                }
            }
            ui.add(Switch::new(&mut self.show_operators).label(tr("Operators")));
            if self.is_busy() {
                ui.add(shadcn_egui::Spinner::new().size(12.0));
            }
        });
    }

    fn stages_ui(&mut self, ui: &mut Ui, tab: TabId, now: f64) {
        let t = Theme::get(ui.ctx());
        if self.stages.is_empty() {
            ui.add_space(12.0);
            ui.label(RichText::new(tr("The pipeline is empty: add a stage to start.")).color(t.muted_foreground));
            return;
        }
        let names: Vec<&str> = STAGES.iter().map(|s| s.name).collect();
        let n = self.stages.len();
        let mut remove = None;
        let mut swap = None;
        let mut changed = None;
        egui::ScrollArea::vertical().id_salt(("pipeline-stages", tab)).auto_shrink([false, false]).show(ui, |ui| {
            for i in 0..n {
                let focused = self.focused == Some(i);
                let s = &mut self.stages[i];
                let stroke = if focused { t.ring } else { t.border };
                egui::Frame::new()
                    .stroke((1.0, stroke))
                    .corner_radius(t.radius_md())
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(trf("Stage {n}", &[("n", &(i + 1))])).strong());
                            let mut choice = names.iter().position(|n| *n == s.stage.name);
                            ui.add(Select::new(&mut choice, &names).width(150.0));
                            if let Some(c) = choice.filter(|c| names.get(*c) != Some(&s.stage.name.as_str())) {
                                let old = aggregation::stage_info(&s.stage.name).map(|s| s.template);
                                let untouched = old == Some(s.stage.body.as_str()) || s.stage.body.trim().is_empty();
                                s.stage.name = names[c].into();
                                if untouched {
                                    s.stage.body = STAGES[c].template.into();
                                }
                                changed = Some((i, 0.0));
                            }
                            if let Some(info) = aggregation::stage_info(&s.stage.name) {
                                ui.label(RichText::new(tr(info.about)).color(t.muted_foreground).small());
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui
                                    .add(Button::ghost("×").size(ButtonSize::Icon))
                                    .on_hover_text(tr("Delete stage"))
                                    .clicked()
                                {
                                    remove = Some(i);
                                }
                                if ui
                                    .add_enabled(i + 1 < n, Button::ghost("⏷").size(ButtonSize::Icon))
                                    .on_hover_text(tr("Move down"))
                                    .clicked()
                                {
                                    swap = Some((i, i + 1));
                                }
                                if ui
                                    .add_enabled(i > 0, Button::ghost("⏶").size(ButtonSize::Icon))
                                    .on_hover_text(tr("Move up"))
                                    .clicked()
                                {
                                    swap = Some((i - 1, i));
                                }
                                let mut on = s.stage.enabled;
                                if ui.add(Switch::new(&mut on).label(tr("Enabled"))).changed() {
                                    s.stage.enabled = on;
                                    changed = Some((i, 0.0));
                                }
                            });
                        });
                        ui.add_space(4.0);
                        ui.columns(2, |cols| {
                            let id = egui::Id::new(("pipeline-stage", tab, i));
                            let before = s.stage.body.clone();
                            sql_editor::editor(&mut cols[0], id, &mut s.stage.body, DbKind::MongoDb, 140.0);
                            if s.stage.body != before {
                                changed = Some((i, now));
                            }
                            if cols[0].memory(|m| m.has_focus(id)) {
                                self.focused = Some(i);
                            }
                            preview_ui(&mut cols[1], tab, i, s);
                        });
                    });
                ui.add_space(6.0);
            }
        });
        if let Some((i, at)) = changed {
            self.focused = Some(i);
            self.changed(i, at);
        }
        if let Some((a, b)) = swap {
            self.swap(a, b);
        }
        if let Some(i) = remove {
            self.remove_stage(i);
        }
    }

    fn operators_ui(&mut self, ui: &mut Ui, tab: TabId, now: f64) {
        let t = Theme::get(ui.ctx());
        ui.add(Input::new(&mut self.operator_query).placeholder(tr("Search operators")).width(ui.available_width()));
        ui.add_space(4.0);
        let hint = match self.focused {
            Some(i) => trf("Click to insert into stage {n}", &[("n", &(i + 1))]),
            None => tr("Click a stage's editor, then an operator").to_string(),
        };
        ui.label(RichText::new(hint).color(t.muted_foreground).small());
        ui.add_space(4.0);
        let found = aggregation::find_operators(&self.operator_query);
        let mut insert = None;
        egui::ScrollArea::vertical().id_salt(("pipeline-operators-list", tab)).auto_shrink([false, false]).show(
            ui,
            |ui| {
                let mut group = "";
                for o in found {
                    if o.group != group {
                        group = o.group;
                        ui.add_space(4.0);
                        ui.label(RichText::new(tr(group)).color(t.muted_foreground).small().strong());
                    }
                    let r = ui
                        .add(egui::Button::new(RichText::new(o.name).monospace()).frame(false))
                        .on_hover_text(o.snippet);
                    if r.clicked() {
                        insert = Some(o.snippet);
                    }
                }
            },
        );
        if let (Some(snippet), Some(i)) = (insert, self.focused) {
            self.insert(ui.ctx(), tab, i, snippet, now);
        }
    }

    /// Focuses stage `i`'s editor with the cursor before character `at`.
    pub fn place_cursor(&mut self, ctx: &egui::Context, i: usize, at: usize) {
        if let Some(tab) = self.tab {
            sql_editor::place_cursor(ctx, egui::Id::new(("pipeline-stage", tab, i)), at);
            self.focused = Some(i);
        }
    }

    /// Inserts `snippet` into stage `i`'s body at its cursor (or at the end).
    pub fn insert(&mut self, ctx: &egui::Context, tab: TabId, i: usize, snippet: &str, now: f64) {
        let Some(s) = self.stages.get_mut(i) else { return };
        let id = egui::Id::new(("pipeline-stage", tab, i));
        let chars = s.stage.body.chars().count();
        let at = egui::TextEdit::load_state(ctx, id)
            .and_then(|st| st.cursor.char_range())
            .map_or(chars, |r| usize::from(r.primary.index).min(chars));
        let byte = s.stage.body.char_indices().nth(at).map_or(s.stage.body.len(), |(b, _)| b);
        s.stage.body.insert_str(byte, snippet);
        sql_editor::place_cursor(ctx, id, at + snippet.chars().count());
        self.changed(i, now);
    }

    fn dialogs(&mut self, ui: &mut Ui, tab: TabId, uri: &str) {
        if let Some(ex) = &mut self.export {
            let mut close = false;
            let lang = Lang::ALL[ex.lang];
            let code = {
                let p = self.stages.iter().map(|s| s.stage.clone()).collect::<Vec<_>>();
                aggregation::pipeline(&p, p.len().saturating_sub(1))
                    .map(|v| {
                        let stages = if p.is_empty() { Vec::new() } else { v.as_array().cloned().unwrap_or_default() };
                        aggregation::export(lang, uri, &self.object.path.database, &self.object.path.name, &stages)
                    })
                    .map_err(|(i, e)| stage_error(i, &e))
            };
            let r = Dialog::new(("pipeline-export", tab), tr("Export pipeline to code"))
                .description(tr("The pipeline as a program using the official driver of the language."))
                .width(760.0)
                .show(ui.ctx(), |ui| {
                    let labels: Vec<&str> = Lang::ALL.iter().map(|l| l.label()).collect();
                    let mut choice = Some(ex.lang);
                    ui.add(Select::new(&mut choice, &labels).width(160.0));
                    ex.lang = choice.unwrap_or(0);
                    ui.add_space(6.0);
                    match &code {
                        Ok(code) => {
                            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                                sql_editor::viewer(ui, egui::Id::new(("pipeline-code", tab)), code, DbKind::MongoDb);
                            });
                        }
                        Err(e) => {
                            Alert::new(tr("The pipeline has an error"))
                                .icon("!")
                                .description(e.as_str())
                                .destructive()
                                .show(ui);
                        }
                    }
                    footer(ui, |ui| {
                        if ui.add_enabled(code.is_ok(), Button::new(tr("Copy"))).clicked()
                            && let Ok(c) = &code
                        {
                            ui.ctx().copy_text(c.clone());
                            toast(ui.ctx(), tr("Copied"), None);
                        }
                        if ui.add(Button::outline(tr("Close"))).clicked() {
                            close = true;
                        }
                    });
                });
            if close || r.should_close {
                self.export = None;
            }
        }
        if let Some(im) = &mut self.import {
            let mut close = false;
            let mut apply = None;
            let r = Dialog::new(("pipeline-import", tab), tr("Import pipeline"))
                .description(tr("A pipeline as the shell writes it: [{ $match: … }, { $group: … }]."))
                .width(640.0)
                .show(ui.ctx(), |ui| {
                    sql_editor::editor(
                        ui,
                        egui::Id::new(("pipeline-import-text", tab)),
                        &mut im.text,
                        DbKind::MongoDb,
                        300.0,
                    );
                    if let Some(e) = &im.error {
                        ui.add_space(6.0);
                        Alert::new(tr("Failed")).icon("!").description(e.as_str()).destructive().show(ui);
                    }
                    footer(ui, |ui| {
                        if ui.add(Button::new(tr("Import"))).clicked() {
                            apply = Some(im.text.clone());
                        }
                        if ui.add(Button::outline(tr("Cancel"))).clicked() {
                            close = true;
                        }
                    });
                });
            close |= r.should_close;
            if let Some(text) = apply {
                match self.import_text(&text) {
                    Ok(()) => close = true,
                    Err(e) => {
                        if let Some(im) = &mut self.import {
                            im.error = Some(e);
                        }
                    }
                }
            }
            if close {
                self.import = None;
            }
        }
    }
}

fn stage_error(i: usize, e: &str) -> String {
    trf("Stage {n}: {error}", &[("n", &(i + 1)), ("error", &e)])
}

/// The connection string for exported code, with the password left out.
fn connection_uri(cfg: &dbman_core::ConnectionConfig) -> String {
    let host = cfg.host.trim();
    if host.starts_with("mongodb://") || host.starts_with("mongodb+srv://") {
        // Drop a password written into the URI.
        if let Some((scheme, rest)) = host.split_once("://")
            && let Some((auth, addr)) = rest.split_once('@')
            && let Some((user, _)) = auth.split_once(':')
        {
            return format!("{scheme}://{user}:<password>@{addr}");
        }
        return host.to_string();
    }
    let host = if host.is_empty() { "localhost" } else { host };
    let auth = if cfg.user.is_empty() { String::new() } else { format!("{}:<password>@", cfg.user) };
    format!("mongodb://{auth}{host}:{}", cfg.port)
}

fn preview_ui(ui: &mut Ui, tab: TabId, i: usize, s: &PipelineStage) {
    let t = Theme::get(ui.ctx());
    let muted = |text: String| RichText::new(text).color(t.muted_foreground);
    match &s.preview {
        Preview::Idle => {}
        Preview::Loading => {
            ui.horizontal(|ui| {
                ui.add(shadcn_egui::Spinner::new().size(12.0));
                ui.label(muted(tr("Running…").into()));
            });
        }
        Preview::Disabled => {
            ui.label(muted(tr("The stage is off: documents pass through unchanged.").into()));
        }
        Preview::Writes => {
            ui.label(muted(
                tr("$out and $merge write to a collection, so they are not previewed. Run the pipeline in a console.")
                    .into(),
            ));
        }
        Preview::Failed(e) => {
            Alert::new(tr("Failed")).icon("!").description(e.as_str()).destructive().show(ui);
        }
        Preview::Ready { docs, more } => {
            let n = docs.len();
            let head = match (n, more) {
                (0, _) => tr("No documents come out of this stage.").to_string(),
                (_, true) => trf("Sample of the first {n} documents", &[("n", &n)]),
                (_, false) => trf("{n} documents", &[("n", &n)]),
            };
            ui.label(muted(head));
            egui::ScrollArea::vertical().id_salt(("pipeline-preview", tab, i)).max_height(240.0).show(ui, |ui| {
                for (k, d) in docs.iter().enumerate() {
                    egui::Frame::new()
                        .fill(t.muted)
                        .corner_radius(t.radius_md())
                        .inner_margin(egui::Margin::same(6))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            sql_editor::viewer(
                                ui,
                                egui::Id::new(("pipeline-doc", tab, i, k)),
                                &aggregation::compact(d, 100),
                                DbKind::MongoDb,
                            );
                        });
                    ui.add_space(4.0);
                }
            });
        }
    }
}
