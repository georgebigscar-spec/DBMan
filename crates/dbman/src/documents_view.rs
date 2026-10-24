//! Documents tab of a MongoDB collection: a page of documents found with a
//! filter, projection and sort typed as in the shell, shown as a table (one
//! column per field) or as documents, and each one edited, inserted or
//! deleted through a JSON editor.

use std::sync::Arc;

use dbman_core::mongo::{self, Call, Reply, Shell};
use dbman_core::{Connection, DbKind, DbObject, DbResult};
use egui::{RichText, Ui};
use serde_json::{Value, json};
use shadcn_egui::{Alert, Button, ButtonSize, Dialog, Input, Select, Theme, alert_dialog, footer, toast, toggle_group};

use crate::events::{Event, TabEvent, TabId};
use crate::explorer::{Action, ConnState};
use crate::grid::{self, Grid};
use crate::i18n::{tr, trf};
use crate::sql_editor;
use crate::tabs::{TabCtx, error_alert, not_connected, toolbar_button};
use crate::value_panel::ValuePanel;

const PAGE_SIZES: [u64; 4] = [20, 50, 100, 500];

pub enum DocumentsEvent {
    Page(DbResult<(Vec<Value>, bool)>),
    Count(DbResult<u64>),
    /// A document was saved, inserted or deleted.
    Written(DbResult<String>),
}

/// The document being edited, or a new one (`original` is `None`).
struct Editor {
    text: String,
    original: Option<Value>,
    error: Option<String>,
}

pub struct DocumentsView {
    pub object: DbObject,
    read_only: bool,
    pub filter: String,
    pub projection: String,
    pub sort: String,
    page_size: usize,
    pub skip: u64,
    docs: Vec<Value>,
    more: bool,
    grid: Option<Grid>,
    count: Option<u64>,
    loading: bool,
    loaded: bool,
    error: Option<String>,
    /// 0: table, 1: documents.
    mode: usize,
    value: ValuePanel,
    editor: Option<Editor>,
    confirm_delete: Option<usize>,
}

/// A shell value typed in a box; empty is `{}`.
fn parse_box(text: &str, what: &str) -> Result<Value, String> {
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    match mongo::parse_value(text) {
        Ok(v @ Value::Object(_)) => Ok(v),
        Ok(_) => Err(trf("The {what} must be a document: {…}.", &[("what", &what)])),
        Err(e) => Err(format!("{what}: {e}")),
    }
}

impl DocumentsView {
    pub fn new(object: DbObject, read_only: bool) -> Self {
        Self {
            object,
            read_only,
            filter: String::new(),
            projection: String::new(),
            sort: String::new(),
            page_size: 1,
            skip: 0,
            docs: Vec::new(),
            more: false,
            grid: None,
            count: None,
            loading: false,
            loaded: false,
            error: None,
            mode: 0,
            value: ValuePanel::default(),
            editor: None,
            confirm_delete: None,
        }
    }

    pub fn title(&self) -> String {
        self.object.path.name.clone()
    }

    pub fn documents(&self) -> &[Value] {
        &self.docs
    }

    pub fn count(&self) -> Option<u64> {
        self.count
    }

    pub fn is_busy(&self) -> bool {
        self.loading
    }

    fn limit(&self) -> u64 {
        PAGE_SIZES[self.page_size]
    }

    fn collection(&self) -> &str {
        &self.object.path.name
    }

    pub fn on_event(&mut self, ev: TabEvent, ctx: &egui::Context) {
        let TabEvent::Documents(ev) = ev else { return };
        match *ev {
            DocumentsEvent::Page(r) => {
                self.loading = false;
                self.loaded = true;
                match r {
                    Ok((docs, more)) => {
                        self.grid = Some(Grid::new(mongo::documents_result(&docs, more)));
                        self.docs = docs;
                        self.more = more;
                        self.error = None;
                    }
                    Err(e) => {
                        self.error = Some(e.message);
                        self.grid = None;
                        self.docs.clear();
                    }
                }
            }
            DocumentsEvent::Count(r) => self.count = r.ok(),
            DocumentsEvent::Written(Ok(done)) => {
                self.editor = None;
                self.loaded = false;
                toast(ctx, done, None);
            }
            DocumentsEvent::Written(Err(e)) => match &mut self.editor {
                Some(ed) => ed.error = Some(e.message),
                None => self.error = Some(e.message),
            },
        }
    }

    /// Reads the page at `skip`, and counts the documents the filter finds.
    fn load(&mut self, cx: &TabCtx<'_>, tab: TabId, session: Arc<dyn Connection>) {
        let parsed = (|| {
            Ok::<_, String>((
                parse_box(&self.filter, tr("filter"))?,
                parse_box(&self.projection, tr("projection"))?,
                parse_box(&self.sort, tr("sort"))?,
            ))
        })();
        let (filter, projection, sort) = match parsed {
            Ok(p) => p,
            Err(e) => {
                self.error = Some(e);
                self.loaded = true;
                return;
            }
        };
        self.loading = true;
        self.loaded = true;
        let db = self.object.path.database.clone();
        let limit = self.limit();
        let find =
            dbman_drivers::mongo::find_statement(self.collection(), &filter, &projection, &sort, self.skip, limit);
        let count = Shell::Collection {
            name: self.collection().into(),
            call: Call { method: "countDocuments".into(), args: vec![filter] },
            chain: vec![],
        };
        let s = session.clone();
        let db2 = db.clone();
        cx.tasks.spawn(async move {
            let result = s.mongo(Some(&db), &find, limit as usize).await.map(|r| match r {
                Reply::Documents { docs, more } => (docs, more),
                _ => (Vec::new(), false),
            });
            Event::Tab { tab, event: TabEvent::Documents(Box::new(DocumentsEvent::Page(result))) }
        });
        cx.tasks.spawn(async move {
            let result = session.mongo(Some(&db2), &count, 0).await.map(|r| match r {
                Reply::Value(v) => v.as_u64().unwrap_or(0),
                _ => 0,
            });
            Event::Tab { tab, event: TabEvent::Documents(Box::new(DocumentsEvent::Count(result))) }
        });
    }

    fn write(&self, cx: &TabCtx<'_>, tab: TabId, session: Arc<dyn Connection>, call: Call, done: String) {
        let db = self.object.path.database.clone();
        let statement = Shell::Collection { name: self.collection().into(), call, chain: vec![] };
        cx.tasks.spawn(async move {
            let result = session.mongo(Some(&db), &statement, 0).await.map(|_| done);
            Event::Tab { tab, event: TabEvent::Documents(Box::new(DocumentsEvent::Written(result))) }
        });
    }

    /// The find as a shell statement, for a console.
    fn statement(&self) -> String {
        let or_empty = |s: &str| if s.trim().is_empty() { "{}".to_string() } else { s.trim().to_string() };
        let mut s = format!("{}.find({}", mongo::collection_ref(self.collection()), or_empty(&self.filter));
        if !self.projection.trim().is_empty() {
            s.push_str(&format!(", {}", self.projection.trim()));
        }
        s.push(')');
        if !self.sort.trim().is_empty() {
            s.push_str(&format!(".sort({})", self.sort.trim()));
        }
        if self.skip > 0 {
            s.push_str(&format!(".skip({})", self.skip));
        }
        s.push_str(&format!(".limit({})", self.limit()));
        s
    }

    fn selected(&self) -> Option<usize> {
        self.grid.as_ref().and_then(|g| g.current).map(|(row, _)| row).filter(|r| *r < self.docs.len())
    }

    /// Opens the editor on document `i`, or on a new one.
    pub fn edit(&mut self, i: Option<usize>) {
        let original = i.and_then(|i| self.docs.get(i).cloned());
        let text = original.as_ref().map_or_else(|| "{\n  \n}".to_string(), mongo::to_shell);
        self.editor = Some(Editor { text, original, error: None });
    }

    /// The text of the open editor, if one is open.
    pub fn editor_text(&mut self) -> Option<&mut String> {
        self.editor.as_mut().map(|e| &mut e.text)
    }

    pub fn ui(&mut self, ui: &mut Ui, tab: TabId, mut cx: TabCtx<'_>) {
        let Some(session) = cx.conn.and_then(ConnState::session) else {
            not_connected(ui, &mut cx);
            return;
        };
        if !self.loaded {
            self.load(&cx, tab, session.clone());
        }
        let t = Theme::get(ui.ctx());
        let mut find = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("Filter")).color(t.muted_foreground));
            let w = (ui.available_width() - 260.0).max(200.0);
            let r = ui.add(Input::new(&mut self.filter).placeholder("{ city: 'Berlin', age: { $gt: 30 } }").width(w));
            find |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.add(Button::new(tr("Find")).size(ButtonSize::Sm)).clicked() {
                find = true;
            }
            if toolbar_button(ui, tr("Reset")).clicked() {
                self.filter.clear();
                self.projection.clear();
                self.sort.clear();
                find = true;
            }
            if self.loading {
                ui.add(shadcn_egui::Spinner::new().size(12.0));
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("Projection")).color(t.muted_foreground));
            let r = ui.add(Input::new(&mut self.projection).placeholder("{ name: 1, _id: 0 }").width(200.0));
            find |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.label(RichText::new(tr("Sort")).color(t.muted_foreground));
            let r = ui.add(Input::new(&mut self.sort).placeholder("{ age: -1 }").width(160.0));
            find |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let sizes: Vec<String> = PAGE_SIZES.iter().map(|n| n.to_string()).collect();
            let refs: Vec<&str> = sizes.iter().map(String::as_str).collect();
            let mut size = Some(self.page_size);
            ui.add(Select::new(&mut size, &refs).width(70.0));
            if size.is_some_and(|s| s != self.page_size) {
                self.page_size = size.unwrap_or(1);
                find = true;
            }
            ui.add_space(8.0);
            // Paging.
            let shown = self.docs.len() as u64;
            let range = if shown == 0 {
                tr("no documents").to_string()
            } else {
                let to = self.skip + shown;
                match self.count {
                    Some(n) => trf("{from}–{to} of {n}", &[("from", &(self.skip + 1)), ("to", &to), ("n", &n)]),
                    None => format!("{}–{to}", self.skip + 1),
                }
            };
            if ui.add_enabled(self.skip > 0 && !self.loading, Button::outline("◀").size(ButtonSize::Sm)).clicked() {
                self.skip = self.skip.saturating_sub(self.limit());
                self.loaded = false;
            }
            ui.label(range);
            if ui.add_enabled(self.more && !self.loading, Button::outline("▶").size(ButtonSize::Sm)).clicked() {
                self.skip += self.limit();
                self.loaded = false;
            }
        });
        if find {
            self.skip = 0;
            self.load(&cx, tab, session.clone());
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            toggle_group(ui, &mut self.mode, &[tr("Table"), tr("Documents")]);
            ui.add_space(8.0);
            let selected = self.selected();
            if !self.read_only {
                if toolbar_button(ui, tr("+ Insert")).clicked() {
                    self.edit(None);
                }
                if ui.add_enabled(selected.is_some(), Button::outline(tr("Edit")).size(ButtonSize::Sm)).clicked() {
                    self.edit(selected);
                }
                if ui.add_enabled(selected.is_some(), Button::outline(tr("Delete")).size(ButtonSize::Sm)).clicked() {
                    self.confirm_delete = selected;
                }
            }
            if toolbar_button(ui, tr("Open in console")).clicked() {
                let db = Some(self.object.path.database.clone());
                cx.actions.push(Action::OpenConsoleWith(cx.conn_id, db, self.statement()));
            }
            if self.mode == 0 {
                grid::value_button(ui, &mut self.value.open);
            }
        });
        ui.add_space(6.0);
        if let Some(e) = &self.error {
            error_alert(ui, tr("The documents could not be read"), e);
        }
        if self.mode == 0 {
            let mut open = None;
            if let Some(g) = &mut self.grid {
                self.value.show(ui, egui::Id::new(("documents-value", tab)), g.current_value());
                if g.show(ui, egui::Id::new(("documents-grid", tab))) && !self.read_only {
                    open = Some(g.current.map(|(r, _)| r));
                }
                if g.result.columns.is_empty() && self.docs.is_empty() && !self.loading {
                    ui.label(RichText::new(tr("No documents match.")).color(t.muted_foreground));
                }
            }
            if let Some(row) = open {
                self.edit(row);
            }
        } else {
            self.documents_ui(ui, tab);
        }
        self.dialogs(ui, tab, &cx, session);
    }

    fn documents_ui(&mut self, ui: &mut Ui, tab: TabId) {
        let t = Theme::get(ui.ctx());
        if self.docs.is_empty() && !self.loading {
            ui.label(RichText::new(tr("No documents match.")).color(t.muted_foreground));
            return;
        }
        let mut open = None;
        egui::ScrollArea::vertical().id_salt(("documents-list", tab)).auto_shrink([false, false]).show(ui, |ui| {
            for (i, d) in self.docs.iter().enumerate() {
                egui::Frame::new()
                    .stroke((1.0, t.border))
                    .corner_radius(t.radius_md())
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("#{}", self.skip + i as u64 + 1)).color(t.muted_foreground));
                            if !self.read_only && ui.add(Button::ghost(tr("Edit")).size(ButtonSize::Sm)).clicked() {
                                open = Some(i);
                            }
                        });
                        sql_editor::viewer(
                            ui,
                            egui::Id::new(("documents-doc", tab, i)),
                            &mongo::to_shell(d),
                            DbKind::MongoDb,
                        );
                    });
                ui.add_space(6.0);
            }
        });
        if let Some(i) = open {
            self.edit(Some(i));
        }
    }

    fn dialogs(&mut self, ui: &mut Ui, tab: TabId, cx: &TabCtx<'_>, session: Arc<dyn Connection>) {
        let mut save = None;
        let mut close = false;
        if let Some(ed) = &mut self.editor {
            let title = if ed.original.is_some() { tr("Edit document") } else { tr("Insert document") };
            let r = Dialog::new(("documents-editor", tab), title)
                .description(trf(
                    "In {name}. Values are written as in the shell: ObjectId(\"…\"), ISODate(\"…\").",
                    &[("name", &self.object.path.name)],
                ))
                .width(720.0)
                .show(ui.ctx(), |ui| {
                    sql_editor::editor(
                        ui,
                        egui::Id::new(("documents-editor-text", tab)),
                        &mut ed.text,
                        DbKind::MongoDb,
                        360.0,
                    );
                    if let Some(e) = &ed.error {
                        ui.add_space(6.0);
                        Alert::new(tr("Failed")).icon("!").description(e.as_str()).destructive().show(ui);
                    }
                    footer(ui, |ui| {
                        if ui.add(Button::new(tr("Save"))).clicked() {
                            save = Some(ed.text.clone());
                        }
                        if ui.add(Button::outline(tr("Cancel"))).clicked() {
                            close = true;
                        }
                    });
                });
            close |= r.should_close;
            if let Some(text) = save {
                match mongo::parse_value(&text) {
                    Ok(doc @ Value::Object(_)) => match &ed.original {
                        Some(original) => match dbman_drivers::mongo::id_filter(original) {
                            Ok(filter) => {
                                let call = Call { method: "replaceOne".into(), args: vec![filter, doc] };
                                self.write(cx, tab, session.clone(), call, tr("Document saved").into());
                            }
                            Err(e) => ed.error = Some(e.message),
                        },
                        None => {
                            let call = Call { method: "insertOne".into(), args: vec![doc] };
                            self.write(cx, tab, session.clone(), call, tr("Document inserted").into());
                        }
                    },
                    Ok(_) => ed.error = Some(tr("A document is written in braces: {…}.").into()),
                    Err(e) => ed.error = Some(e),
                }
            }
        }
        if close {
            self.editor = None;
        }
        if let Some(i) = self.confirm_delete {
            let id = self.docs.get(i).and_then(|d| d.get("_id")).map(mongo::cell).map(|c| c.display().into_owned());
            match alert_dialog(
                ui.ctx(),
                ("documents-delete", tab),
                tr("Delete document?"),
                &trf(
                    "The document {id} will be deleted from {name}.",
                    &[("id", &id.unwrap_or_default()), ("name", &self.object.path.name)],
                ),
                tr("Delete"),
            ) {
                Some(true) => {
                    self.confirm_delete = None;
                    match self.docs.get(i).map(dbman_drivers::mongo::id_filter) {
                        Some(Ok(filter)) => {
                            let call = Call { method: "deleteOne".into(), args: vec![filter] };
                            self.write(cx, tab, session, call, tr("Document deleted").into());
                        }
                        Some(Err(e)) => self.error = Some(e.message),
                        None => {}
                    }
                }
                Some(false) => self.confirm_delete = None,
                None => {}
            }
        }
    }
}
