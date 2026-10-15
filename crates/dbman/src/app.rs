//! The main window: explorer on the left, tabs in the middle, status bar below.

use std::sync::mpsc::Receiver;

use dbman_core::{ConnectionConfig, DbKind};
use egui::{Key, Modifiers, RichText, Ui};
use shadcn_egui::{Button, ButtonSize, Switch, Theme, Toaster, TooltipExt, alert_dialog, toast, toast_error};
use uuid::Uuid;

use crate::connection_dialog::{ConnectionDialog, DialogOutcome};
use crate::data_view::DataView;
use crate::events::{Event, TabId, Tasks};
use crate::exec;
use crate::explorer::{self, Action, ConnState, Load, Node};
use crate::icons::{self, Icon};
use crate::prompt::{Prompt, PromptKind, PromptOutcome};
use crate::store::Settings;
use crate::table_editor::TableEditor;
use crate::tabs::{Console, ObjectView, Tab, TabCtx, TabKind};

pub struct DbManApp {
    settings: Settings,
    conns: Vec<ConnState>,
    tasks: Tasks,
    events: Receiver<Event>,
    tabs: Vec<Tab>,
    active: Option<TabId>,
    next_tab: TabId,
    selected: Option<Node>,
    dialog: Option<ConnectionDialog>,
    confirm_delete: Option<Uuid>,
    prompt: Option<Prompt>,
    status: String,
    // Keeps the runtime alive for the lifetime of the window.
    runtime: Option<tokio::runtime::Runtime>,
}

impl DbManApp {
    pub fn new(cc: &eframe::CreationContext<'_>, runtime: tokio::runtime::Runtime) -> Self {
        Self::with_settings(&cc.egui_ctx, runtime, Settings::load())
    }

    pub fn with_settings(ctx: &egui::Context, runtime: tokio::runtime::Runtime, settings: Settings) -> Self {
        let (tasks, events) = Tasks::new(runtime.handle().clone(), ctx.clone());
        let conns = settings.connections.iter().cloned().map(ConnState::new).collect();
        Self {
            settings,
            conns,
            tasks,
            events,
            tabs: Vec::new(),
            active: None,
            next_tab: 1,
            selected: None,
            dialog: None,
            confirm_delete: None,
            prompt: None,
            status: "Ready".into(),
            runtime: Some(runtime),
        }
    }

    fn conn(&self, id: Uuid) -> Option<&ConnState> {
        self.conns.iter().find(|c| c.cfg.id == id)
    }

    fn conn_mut(&mut self, id: Uuid) -> Option<&mut ConnState> {
        self.conns.iter_mut().find(|c| c.cfg.id == id)
    }

    fn save_settings(&mut self) {
        self.settings.connections = self.conns.iter().map(|c| c.cfg.clone()).collect();
        if let Err(e) = self.settings.save() {
            self.status = format!("Could not save settings: {e}");
        }
    }

    // --- background results -------------------------------------------------

    fn drain_events(&mut self, ctx: &egui::Context) {
        let mut follow_up = Vec::new();
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                Event::Connected { conn, result } => {
                    let Some(c) = self.conn_mut(conn) else { continue };
                    match result {
                        Ok((session, version)) => {
                            c.version = version.clone();
                            c.session = Load::Ready(session);
                            self.status = format!("Connected: {version}");
                        }
                        Err(e) => {
                            let name = c.cfg.display_name();
                            c.session = Load::Failed(e.message.clone());
                            toast_error(
                                ctx,
                                format!("Could not connect to {name}"),
                                Some(e.message.lines().next().unwrap_or("")),
                            );
                        }
                    }
                }
                Event::Databases { conn, result } => {
                    if let Some(c) = self.conn_mut(conn) {
                        c.databases = Load::from_result(result);
                    }
                }
                Event::Schemas { conn, db, result } => {
                    if let Some(c) = self.conn_mut(conn) {
                        c.schemas.insert(db, Load::from_result(result));
                    }
                }
                Event::Objects { conn, db, schema, result } => {
                    if let Some(c) = self.conn_mut(conn) {
                        c.objects.insert((db, schema), Load::from_result(result));
                    }
                }
                Event::Details { conn, path, result } => {
                    if let Some(c) = self.conn_mut(conn) {
                        c.details.insert(path, Load::from_result(result));
                    }
                }
                Event::TestedConnection { result } => {
                    if let Some(d) = &mut self.dialog {
                        d.test = Load::from_result(result);
                    }
                }
                Event::Tab { tab, event } => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                        follow_up.extend(t.on_event(event, ctx));
                    }
                }
                Event::PromptDone { result } => {
                    let Some(p) = &mut self.prompt else { continue };
                    p.running = false;
                    match result {
                        Ok(_) => {
                            toast(ctx, p.done_message(), None);
                            self.status = p.done_message();
                            follow_up.push(p.refresh());
                            self.prompt = None;
                        }
                        Err(e) => p.error = Some(e.message),
                    }
                }
            }
        }
        for a in follow_up {
            self.handle(a, ctx);
        }
    }

    // --- actions ------------------------------------------------------------

    /// Performs an explorer or tab action (also used by tests).
    pub fn handle(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::Connect(id) => self.connect(id),
            Action::LoadDatabases(id) => {
                let Some(c) = self.conn_mut(id) else { return };
                let Some(s) = c.session() else { return };
                c.databases = Load::Loading;
                self.tasks.spawn(async move { Event::Databases { conn: id, result: s.databases().await } });
            }
            Action::LoadSchemas(id, db) => {
                let Some(c) = self.conn_mut(id) else { return };
                let Some(s) = c.session() else { return };
                c.schemas.insert(db.clone(), Load::Loading);
                self.tasks.spawn(async move {
                    let result = s.schemas(&db).await;
                    Event::Schemas { conn: id, db, result }
                });
            }
            Action::LoadObjects(id, db, schema) => {
                let Some(c) = self.conn_mut(id) else { return };
                let Some(s) = c.session() else { return };
                c.objects.insert((db.clone(), schema.clone()), Load::Loading);
                self.tasks.spawn(async move {
                    let result = s.objects(&db, &schema).await;
                    Event::Objects { conn: id, db, schema, result }
                });
            }
            Action::LoadDetails(id, path) => {
                let Some(c) = self.conn_mut(id) else { return };
                let Some(s) = c.session() else { return };
                c.details.insert(path.clone(), Load::Loading);
                self.tasks.spawn(async move {
                    let result = s.table_details(&path).await;
                    Event::Details { conn: id, path, result }
                });
            }
            Action::RefreshObjects(id, db, schema) => {
                if let Some(c) = self.conn_mut(id) {
                    c.objects.remove(&(db.clone(), schema.clone()));
                    c.details.retain(|p, _| !(p.database == db && p.schema == schema));
                }
            }
            Action::RefreshSchemas(id, db) => {
                if let Some(c) = self.conn_mut(id) {
                    c.schemas.remove(&db);
                    c.objects.retain(|(d, _), _| *d != db);
                    c.details.retain(|p, _| p.database != db);
                }
            }
            Action::NewTable(id, db, schema) => {
                let Some(c) = self.conn(id) else { return };
                let kind = c.cfg.kind;
                self.add_tab(id, TabKind::TableEditor(Box::new(TableEditor::create(id, kind, &db, &schema))));
            }
            Action::EditTable(id, path) => {
                let Some(c) = self.conn(id) else { return };
                let kind = c.cfg.kind;
                self.add_tab(id, TabKind::TableEditor(Box::new(TableEditor::edit(id, kind, path))));
            }
            Action::NewFromTemplate(id, db, schema, what) => {
                let Some(c) = self.conn(id) else { return };
                let sql = dbman_core::ddl::template(c.cfg.kind, what, &db, &schema);
                self.open_console(id, Some(db), Some(sql));
            }
            Action::NewDatabase(id) => self.open_prompt(id, PromptKind::CreateDatabase),
            Action::NewSchema(id, db) => self.open_prompt(id, PromptKind::CreateSchema { db }),
            Action::DropDatabase(id, db) => self.open_prompt(id, PromptKind::DropDatabase { db }),
            Action::DropSchema(id, db, schema) => self.open_prompt(id, PromptKind::DropSchema { db, schema }),
            Action::DropObject(id, o) => self.open_prompt(id, PromptKind::DropObject(o)),
            Action::RenameObject(id, o) => self.open_prompt(id, PromptKind::Rename(o)),
            Action::Truncate(id, o) => self.open_prompt(id, PromptKind::Truncate(o)),
            Action::Refresh(id) => {
                if let Some(c) = self.conn_mut(id) {
                    c.clear_cache();
                }
            }
            Action::Disconnect(id) => {
                if let Some(c) = self.conn_mut(id) {
                    c.disconnect();
                    self.status = format!("Disconnected from {}", c.cfg.display_name());
                }
            }
            Action::OpenConsole(id, db) => self.open_console(id, db, None),
            Action::OpenData(id, object) => {
                let Some(c) = self.conn(id) else { return };
                let kind = c.cfg.kind;
                let existing = self
                    .tabs
                    .iter()
                    .find(|t| t.conn == id && matches!(&t.kind, TabKind::Data(d) if d.object == object));
                match existing {
                    Some(t) => self.active = Some(t.id),
                    None => self.add_tab(id, TabKind::Data(Box::new(DataView::new(object, kind)))),
                }
            }
            Action::OpenObject(id, object) => {
                let Some(c) = self.conn(id) else { return };
                let kind = c.cfg.kind;
                let existing = self
                    .tabs
                    .iter()
                    .find(|t| t.conn == id && matches!(&t.kind, TabKind::Object(o) if o.object == object));
                match existing {
                    Some(t) => self.active = Some(t.id),
                    None => self.add_tab(id, TabKind::Object(ObjectView::new(object, kind))),
                }
            }
            Action::NewConnection => {
                self.dialog = Some(ConnectionDialog::new(ConnectionConfig::new(DbKind::Postgres), true))
            }
            Action::EditConnection(id) => {
                if let Some(c) = self.conn(id) {
                    self.dialog = Some(ConnectionDialog::new(c.cfg.clone(), false));
                }
            }
            Action::DuplicateConnection(id) => {
                if let Some(c) = self.conn(id) {
                    let mut cfg = c.cfg.clone();
                    cfg.id = Uuid::new_v4();
                    cfg.name = format!("{} (copy)", c.cfg.display_name());
                    self.dialog = Some(ConnectionDialog::new(cfg, true));
                }
            }
            Action::DeleteConnection(id) => self.confirm_delete = Some(id),
            Action::CopyText(text) => {
                ctx.copy_text(text.clone());
                toast(ctx, "Copied", Some(&text));
            }
        }
    }

    fn connect(&mut self, id: Uuid) {
        let Some(c) = self.conn_mut(id) else { return };
        if matches!(c.session, Load::Loading) {
            return;
        }
        c.session = Load::Loading;
        let cfg = c.cfg.clone();
        self.status = format!("Connecting to {}…", cfg.display_name());
        self.tasks.spawn(async move {
            let result = async {
                let s = dbman_drivers::connect(&cfg).await?;
                let v = s.server_version().await?;
                Ok((s, v))
            }
            .await;
            Event::Connected { conn: id, result }
        });
    }

    fn add_tab(&mut self, conn: Uuid, kind: TabKind) {
        let id = self.next_tab;
        self.next_tab += 1;
        self.tabs.push(Tab { id, conn, kind });
        self.active = Some(id);
    }

    fn open_prompt(&mut self, id: Uuid, kind: PromptKind) {
        if let Some(c) = self.conn(id) {
            self.prompt = Some(Prompt::new(id, c.cfg.kind, kind));
        }
    }

    fn open_console(&mut self, id: Uuid, db: Option<String>, sql: Option<String>) {
        let Some(c) = self.conn(id) else { return };
        let n = self.tabs.iter().filter(|t| t.conn == id && matches!(t.kind, TabKind::Console(_))).count();
        let mut title = c.cfg.display_name();
        if n > 0 {
            title.push_str(&format!(" [{}]", n + 1));
        }
        let mut console = Console::new(title, c.cfg.clone(), db);
        if let Some(sql) = sql {
            console.sql = sql;
        }
        let tab = self.next_tab;
        console.open_session(&self.tasks, tab);
        if c.session.is_idle() {
            self.connect(id);
        }
        self.add_tab(id, TabKind::Console(console));
    }

    fn close_tab(&mut self, id: TabId) {
        if let Some(i) = self.tabs.iter().position(|t| t.id == id) {
            self.tabs.remove(i);
            if self.active == Some(id) {
                self.active = self.tabs.get(i.min(self.tabs.len().saturating_sub(1))).map(|t| t.id);
            }
        }
    }

    /// The connection of the selected explorer row, or the active tab's.
    fn current_conn(&self) -> Option<Uuid> {
        let from_tree = match &self.selected {
            Some(
                Node::Conn(id)
                | Node::Database(id, _)
                | Node::Schema(id, ..)
                | Node::Group(id, ..)
                | Node::Object(id, _)
                | Node::Column(id, ..),
            ) => Some(*id),
            _ => None,
        };
        from_tree
            .or_else(|| self.active.and_then(|a| self.tabs.iter().find(|t| t.id == a)).map(|t| t.conn))
            .or_else(|| self.conns.first().map(|c| c.cfg.id))
    }

    /// Titles of the open tabs, in order.
    pub fn tab_titles(&self) -> Vec<String> {
        self.tabs.iter().map(Tab::title).collect()
    }

    /// The data tab on screen, if the active tab is one.
    pub fn active_data_view(&mut self) -> Option<&mut DataView> {
        let active = self.active?;
        match &mut self.tabs.iter_mut().find(|t| t.id == active)?.kind {
            TabKind::Data(d) => Some(d.as_mut()),
            _ => None,
        }
    }

    /// Result summaries of every console ("4 rows in 1 ms"…).
    pub fn console_summaries(&self) -> Vec<String> {
        self.tabs
            .iter()
            .filter_map(|t| match &t.kind {
                TabKind::Console(c) => Some(c.summaries()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    // --- drawing -------------------------------------------------------------

    pub fn ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let theme = if self.settings.dark { Theme::dark() } else { Theme::light() };
        if Theme::installed(&ctx).as_ref() != Some(&theme) {
            theme.install(&ctx);
        }
        self.drain_events(&ctx);
        let t = Theme::get(&ctx);
        let mut actions = Vec::new();

        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::T))
            && let Some(id) = self.current_conn()
        {
            actions.push(Action::OpenConsole(id, None));
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::W))
            && let Some(a) = self.active
        {
            self.close_tab(a);
        }
        if ctx.input(|i| i.key_pressed(Key::F4))
            && let Some(Node::Object(id, o)) = &self.selected
        {
            actions.push(if o.kind.has_data() {
                Action::OpenData(*id, o.clone())
            } else {
                Action::OpenObject(*id, o.clone())
            });
        }

        let bar = egui::Frame::new().fill(t.background).inner_margin(egui::Margin::symmetric(12, 8));
        egui::Panel::top("toolbar").frame(bar).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("DBMan").size(16.0).strong().color(t.foreground));
                ui.add_space(12.0);
                if ui.add(Button::new("+ New connection").size(ButtonSize::Sm)).clicked() {
                    actions.push(Action::NewConnection);
                }
                let current = self.current_conn();
                if ui
                    .add_enabled(current.is_some(), Button::outline("New console").size(ButtonSize::Sm))
                    .tooltip("Open a SQL console for the selected connection (Ctrl+T)")
                    .clicked()
                    && let Some(id) = current
                {
                    actions.push(Action::OpenConsole(id, None));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut dark = self.settings.dark;
                    if ui.add(Switch::new(&mut dark).label("Dark")).changed() {
                        self.settings.dark = dark;
                        self.save_settings();
                    }
                });
            });
        });

        let status_frame = egui::Frame::new().fill(t.muted).inner_margin(egui::Margin::symmetric(12, 4));
        egui::Panel::bottom("status").frame(status_frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&self.status).size(12.0).color(t.muted_foreground));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let open = self.conns.iter().filter(|c| c.session.ready().is_some()).count();
                    ui.label(RichText::new(format!("{open} connected")).size(12.0).color(t.muted_foreground));
                });
            });
        });

        let side = egui::Frame::new().fill(t.background).inner_margin(egui::Margin::same(8));
        egui::Panel::left("explorer").resizable(true).default_size(300.0).min_size(200.0).frame(side).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Database Explorer").strong().color(t.foreground));
            });
            ui.add_space(4.0);
            egui::ScrollArea::both().id_salt("explorer-scroll").auto_shrink([false, false]).show(ui, |ui| {
                explorer::show(ui, &self.conns, &mut self.selected, &mut actions);
            });
        });

        let central = egui::Frame::new().fill(t.background).inner_margin(egui::Margin::same(8));
        egui::CentralPanel::default().frame(central).show(ui, |ui| {
            self.tab_bar(ui);
            ui.add_space(6.0);
            let Some(active) = self.active else {
                self.welcome(ui, &mut actions);
                return;
            };
            let page_size = self.settings.page_size;
            let Some(tab) = self.tabs.iter_mut().find(|t| t.id == active) else { return };
            let conn = self.conns.iter().find(|c| c.cfg.id == tab.conn);
            let conn_id = tab.conn;
            tab.ui(ui, TabCtx { tasks: &self.tasks, conn, actions: &mut actions, page_size, conn_id });
        });

        for a in actions {
            self.handle(a, &ctx);
        }
        self.dialogs(&ctx);
        Toaster::show(&ctx);
    }

    fn tab_bar(&mut self, ui: &mut Ui) {
        if self.tabs.is_empty() {
            return;
        }
        let t = Theme::get(ui.ctx());
        let mut close = None;
        egui::ScrollArea::horizontal().id_salt("tab-bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for tab in &self.tabs {
                    let active = self.active == Some(tab.id);
                    let kind = self.conns.iter().find(|c| c.cfg.id == tab.conn).map(|c| c.cfg.kind);
                    let frame = egui::Frame::new()
                        .fill(if active { t.secondary } else { t.background })
                        .corner_radius(t.radius_md())
                        .inner_margin(egui::Margin::symmetric(8, 4));
                    let r = frame
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if let Some(k) = kind {
                                    explorer::kind_badge(ui, k);
                                }
                                let icon = match tab.kind {
                                    TabKind::Console(_) => Icon::Console,
                                    TabKind::Data(_) => Icon::Table,
                                    TabKind::Object(_) => Icon::Info,
                                    TabKind::TableEditor(_) => Icon::Table,
                                };
                                let color = if active { t.foreground } else { t.muted_foreground };
                                icons::icon(ui, icon, color);
                                let title = ui.add(
                                    egui::Label::new(RichText::new(tab.title()).color(color))
                                        .selectable(false)
                                        .sense(egui::Sense::click()),
                                );
                                if title.clicked() {
                                    self.active = Some(tab.id);
                                }
                                if title.middle_clicked() {
                                    close = Some(tab.id);
                                }
                                if ui.add(Button::ghost("×").size(ButtonSize::Icon)).tooltip("Close (Ctrl+W)").clicked()
                                {
                                    close = Some(tab.id);
                                }
                            });
                        })
                        .response;
                    if r.interact(egui::Sense::click()).clicked() {
                        self.active = Some(tab.id);
                    }
                }
            });
        });
        if let Some(id) = close {
            self.close_tab(id);
        }
        ui.separator();
    }

    fn welcome(&self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let t = Theme::get(ui.ctx());
        ui.add_space(80.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("DBMan").size(28.0).color(t.foreground));
            ui.label(RichText::new("PostgreSQL · MySQL / MariaDB · SQLite").color(t.muted_foreground));
            ui.add_space(16.0);
            if ui.add(Button::new("+ New connection")).clicked() {
                actions.push(Action::NewConnection);
            }
            ui.add_space(16.0);
            for (keys, what) in [
                ("Double-click a table", "open its data"),
                ("Ctrl+T", "new SQL console"),
                ("Ctrl+Enter", "run the statement under the cursor"),
                ("Ctrl+Shift+Enter", "run the whole script"),
                ("Ctrl+W", "close the tab"),
            ] {
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 360.0).max(0.0) / 2.0);
                    shadcn_egui::kbd(ui, keys);
                    ui.label(RichText::new(what).color(t.muted_foreground));
                });
            }
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(d) = &mut self.dialog {
            match d.show(ctx) {
                Some(DialogOutcome::Close) => self.dialog = None,
                Some(DialogOutcome::Test(cfg)) => {
                    d.test = Load::Loading;
                    self.tasks.spawn(async move {
                        let result = async {
                            let s = dbman_drivers::connect(&cfg).await?;
                            s.server_version().await
                        }
                        .await;
                        Event::TestedConnection { result }
                    });
                }
                Some(DialogOutcome::Save(cfg)) => {
                    let id = cfg.id;
                    match self.conn_mut(id) {
                        Some(c) => {
                            let changed = c.cfg != cfg;
                            c.cfg = cfg;
                            if changed {
                                c.disconnect();
                            }
                        }
                        None => {
                            self.conns.push(ConnState::new(cfg));
                            self.selected = Some(Node::Conn(id));
                        }
                    }
                    self.dialog = None;
                    self.save_settings();
                }
                None => {}
            }
        }
        if let Some(p) = &mut self.prompt {
            match p.show(ctx) {
                Some(PromptOutcome::Close) => self.prompt = None,
                Some(PromptOutcome::Run { db, statements }) => {
                    if let Some(cfg) = self.conns.iter().find(|c| c.cfg.id == p.conn).map(|c| c.cfg.clone()) {
                        p.running = true;
                        p.error = None;
                        self.tasks.spawn(async move {
                            let result = exec::run(cfg, db, statements, exec::Mode::Sequential).await;
                            Event::PromptDone { result }
                        });
                    }
                }
                None => {}
            }
        }
        if let Some(id) = self.confirm_delete {
            let name = self.conn(id).map(|c| c.cfg.display_name()).unwrap_or_default();
            match alert_dialog(
                ctx,
                "delete-connection",
                "Delete connection?",
                &format!("\"{name}\" will be removed from the list. The database itself is not touched."),
                "Delete",
            ) {
                Some(true) => {
                    self.conns.retain(|c| c.cfg.id != id);
                    let closing: Vec<TabId> = self.tabs.iter().filter(|t| t.conn == id).map(|t| t.id).collect();
                    for t in closing {
                        self.close_tab(t);
                    }
                    self.confirm_delete = None;
                    self.save_settings();
                }
                Some(false) => self.confirm_delete = None,
                None => {}
            }
        }
    }
}

impl Drop for DbManApp {
    fn drop(&mut self) {
        // Closing the window must not wait for a query that is still running.
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

impl eframe::App for DbManApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        DbManApp::ui(self, ui);
    }
}
