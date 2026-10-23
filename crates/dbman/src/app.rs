//! The main window: explorer on the left, tabs in the middle, status bar below.

use crate::i18n::{self, Lang, tr, trf};
use std::sync::mpsc::Receiver;

use dbman_core::{ConnectionConfig, DbKind};
use egui::{Key, Modifiers, RichText, Ui};
use shadcn_egui::{Button, ButtonSize, Select, Switch, Theme, Toaster, TooltipExt, alert_dialog, toast, toast_error};
use uuid::Uuid;

use crate::admin_view::{self, AdminView};
use crate::backup_dialog::{BackupDialog, BackupOutcome, Mode, StartJob};
use crate::compare_view::CompareView;
use crate::connection_dialog::{ConnectionDialog, DialogOutcome};
use crate::copy_view::CopyView;
use crate::data_view::DataView;
use crate::er_view::ErView;
use crate::events::{Event, JobEvent, TabId, Tasks};
use crate::exec;
use crate::explorer::{self, Action, ConnState, Load, Node};
use crate::generate_dialog::GenerateDialog;
use crate::icons::{self, Icon};
use crate::jobs::{JobState, Jobs};
use crate::palette::{self, Pick};
use crate::prompt::{Prompt, PromptKind, PromptOutcome};
use crate::sessions_view::SessionsView;
use crate::store::{History, SavedTab, Settings, Workspace};
use crate::table_editor::TableEditor;
use crate::tabs::{Console, ObjectView, Tab, TabCtx, TabKind};
use crate::transfer_dialog::{ExportDialog, ImportDialog, Start, TransferOutcome};

pub struct DbManApp {
    settings: Settings,
    history: History,
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
    /// A console with an open transaction waiting for Close to be confirmed.
    closing: Option<TabId>,
    /// The window title last sent to the OS.
    shown_title: String,
    backup: Option<BackupDialog>,
    export: Option<ExportDialog>,
    import: Option<ImportDialog>,
    generate: Option<GenerateDialog>,
    /// The command palette (Ctrl+K) is open.
    palette: bool,
    pub jobs: Jobs,
    status: String,
    // Keeps the runtime alive for the lifetime of the window.
    runtime: Option<tokio::runtime::Runtime>,
    /// The open tabs as last saved, to save only changes.
    saved_workspace: Workspace,
    workspace_checked: std::time::Instant,
    /// Where the open tabs are kept between runs; `None` keeps nothing.
    workspace_file: Option<std::path::PathBuf>,
}

impl DbManApp {
    pub fn new(cc: &eframe::CreationContext<'_>, runtime: tokio::runtime::Runtime) -> Self {
        let app = Self::with_settings(&cc.egui_ctx, runtime, Settings::load());
        match Workspace::path() {
            Some(path) => app.keep_workspace(path),
            None => app,
        }
    }

    /// Reopens the tabs saved in `path` and saves them there as they change.
    pub fn keep_workspace(mut self, path: std::path::PathBuf) -> Self {
        let workspace = Workspace::load(&path);
        self.restore(&workspace);
        self.saved_workspace = workspace;
        self.workspace_file = Some(path);
        self
    }

    pub fn with_settings(ctx: &egui::Context, runtime: tokio::runtime::Runtime, settings: Settings) -> Self {
        let (tasks, events) = Tasks::new(runtime.handle().clone(), ctx.clone());
        crate::i18n::set(crate::i18n::Lang::from_code(&settings.language));
        if let Some(path) = Settings::known_hosts_path() {
            dbman_drivers::ssh::set_known_hosts_file(path);
        }
        let conns = settings.connections.iter().cloned().map(ConnState::new).collect();
        Self {
            settings,
            history: History::load(),
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
            closing: None,
            shown_title: String::new(),
            backup: None,
            export: None,
            import: None,
            generate: None,
            palette: false,
            jobs: Jobs::default(),
            status: tr("Ready").into(),
            runtime: Some(runtime),
            saved_workspace: Workspace::default(),
            workspace_checked: std::time::Instant::now(),
            workspace_file: None,
        }
    }

    /// The open tabs, as they are saved between runs.
    pub fn workspace(&self) -> Workspace {
        let tabs: Vec<(TabId, SavedTab)> = self
            .tabs
            .iter()
            .filter_map(|t| {
                let conn = t.conn;
                let saved = match &t.kind {
                    TabKind::Console(c) => SavedTab::Console {
                        conn,
                        database: c.database.clone(),
                        title: c.title.clone(),
                        sql: c.sql.clone(),
                        file: c.file.clone(),
                        manual: c.manual,
                    },
                    TabKind::Data(d) => SavedTab::Data { conn, object: d.object.clone() },
                    TabKind::Object(o) => SavedTab::Object { conn, object: o.object.clone() },
                    TabKind::Diagram(d) => {
                        SavedTab::Diagram { conn, database: d.database.clone(), schema: d.schema.clone() }
                    }
                    TabKind::Sessions(_) => SavedTab::Sessions { conn },
                    // Editors and comparisons hold work in progress that does not keep;
                    // the users tab depends on rights checked after connecting.
                    TabKind::TableEditor(_) | TabKind::Compare(_) | TabKind::Copy(_) | TabKind::Admin(_) => {
                        return None;
                    }
                };
                Some((t.id, saved))
            })
            .collect();
        let active = tabs.iter().position(|(id, _)| Some(*id) == self.active);
        Workspace { tabs: tabs.into_iter().map(|(_, s)| s).collect(), active }
    }

    /// Opens the tabs of a saved workspace. Nothing connects until a tab needs it.
    fn restore(&mut self, w: &Workspace) {
        let mut active = None;
        for (i, saved) in w.tabs.iter().enumerate() {
            let conn = match saved {
                SavedTab::Console { conn, .. }
                | SavedTab::Data { conn, .. }
                | SavedTab::Object { conn, .. }
                | SavedTab::Diagram { conn, .. }
                | SavedTab::Sessions { conn } => *conn,
            };
            // The connection may have been deleted since.
            let Some(c) = self.conn(conn) else { continue };
            let cfg = c.cfg.clone();
            let kind = match saved.clone() {
                SavedTab::Console { database, title, sql, file, manual, .. } => {
                    let mut console = Console::new(title, cfg, database);
                    if let Some(path) = file {
                        // The file as it is now, so edits made before closing still show as unsaved.
                        let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
                        console.set_file(path, on_disk);
                    }
                    console.sql = sql;
                    console.manual = manual;
                    TabKind::Console(Box::new(console))
                }
                SavedTab::Data { object, .. } => {
                    let mut view = DataView::new(object, cfg.kind);
                    view.read_only = cfg.read_only;
                    TabKind::Data(Box::new(view))
                }
                SavedTab::Object { object, .. } => TabKind::Object(Box::new(ObjectView::new(object, cfg.kind))),
                SavedTab::Diagram { database, schema, .. } => {
                    TabKind::Diagram(Box::new(ErView::new(cfg, database, schema)))
                }
                SavedTab::Sessions { .. } => TabKind::Sessions(Box::new(SessionsView::new(cfg.kind, cfg.product))),
            };
            self.add_tab(conn, kind);
            if w.active == Some(i) {
                active = self.active;
            }
        }
        if active.is_some() {
            self.active = active;
        }
    }

    /// Saves the open tabs when they changed, at most once a second unless `now`.
    fn save_workspace(&mut self, now: bool) {
        let Some(path) = self.workspace_file.clone() else { return };
        if !now && self.workspace_checked.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        self.workspace_checked = std::time::Instant::now();
        let w = self.workspace();
        if w != self.saved_workspace {
            if let Err(e) = w.save(&path) {
                self.status = trf("Could not save the open tabs: {error}", &[("error", &e)]);
            }
            self.saved_workspace = w;
        }
    }

    pub fn conn(&self, id: Uuid) -> Option<&ConnState> {
        self.conns.iter().find(|c| c.cfg.id == id)
    }

    fn conn_mut(&mut self, id: Uuid) -> Option<&mut ConnState> {
        self.conns.iter_mut().find(|c| c.cfg.id == id)
    }

    fn save_settings(&mut self) {
        self.settings.connections = self.conns.iter().map(|c| c.cfg.clone()).collect();
        match self.settings.save() {
            Ok(None) => {}
            Ok(Some(warning)) => self.status = warning,
            Err(e) => self.status = trf("Could not save settings: {error}", &[("error", &e)]),
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
                            c.session = Load::Ready(session.clone());
                            self.status = trf("Connected: {version}", &[("version", &version)]);
                            self.tasks.spawn(async move {
                                Event::AdminRights { conn, allowed: admin_view::may_manage(&session).await }
                            });
                        }
                        Err(e) => {
                            let name = c.cfg.display_name();
                            c.session = Load::Failed(e.message.clone());
                            toast_error(
                                ctx,
                                trf("Could not connect to {name}", &[("name", &name)]),
                                Some(e.message.lines().next().unwrap_or("")),
                            );
                        }
                    }
                }
                Event::AdminRights { conn, allowed } => {
                    if let Some(c) = self.conn_mut(conn)
                        && c.session().is_some()
                    {
                        c.admin = allowed;
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
                Event::Job { job, event } => {
                    let finished = matches!(event, JobEvent::Finished(_));
                    follow_up.extend(self.jobs.on_event(job, event, ctx));
                    if finished && let Some(j) = self.jobs.list.iter().find(|j| j.id == job) {
                        self.status = match &j.state {
                            JobState::Done => trf("{title}: done", &[("title", &j.title)]),
                            JobState::Cancelled => trf("{title}: cancelled", &[("title", &j.title)]),
                            _ => trf("{title}: failed", &[("title", &j.title)]),
                        };
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
        if let Some(id) = changes(&action)
            && self.conn(id).is_some_and(|c| c.cfg.read_only)
        {
            toast_error(
                ctx,
                tr("The connection is read-only"),
                Some(tr("Turn \"Read-only\" off in its properties to change it.")),
            );
            return;
        }
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
                    self.status = trf("Disconnected from {name}", &[("name", &c.cfg.display_name())]);
                }
            }
            Action::OpenConsole(id, db) => self.open_console(id, db, None),
            Action::OpenData(id, object) => {
                let Some(c) = self.conn(id) else { return };
                let (kind, read_only) = (c.cfg.kind, c.cfg.read_only);
                let existing = self
                    .tabs
                    .iter()
                    .find(|t| t.conn == id && matches!(&t.kind, TabKind::Data(d) if d.object == object));
                match existing {
                    Some(t) => self.active = Some(t.id),
                    None => {
                        let mut view = DataView::new(object, kind);
                        view.read_only = read_only;
                        self.add_tab(id, TabKind::Data(Box::new(view)))
                    }
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
                    None => self.add_tab(id, TabKind::Object(Box::new(ObjectView::new(object, kind)))),
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
                    cfg.name = trf("{name} (copy)", &[("name", &c.cfg.display_name())]);
                    self.dialog = Some(ConnectionDialog::new(cfg, true));
                }
            }
            Action::DeleteConnection(id) => self.confirm_delete = Some(id),
            Action::Backup(id, scope) => self.open_backup(id, Mode::Backup, scope),
            Action::Restore(id, scope) => self.open_backup(id, Mode::Restore, scope),
            Action::OpenDiagram(id, db, schema) => {
                let Some(c) = self.conn(id) else { return };
                let cfg = c.cfg.clone();
                let existing = self.tabs.iter().find(|t| {
                    t.conn == id && matches!(&t.kind, TabKind::Diagram(d) if d.database == db && d.schema == schema)
                });
                match existing {
                    Some(t) => self.active = Some(t.id),
                    None => self.add_tab(id, TabKind::Diagram(Box::new(ErView::new(cfg, db, schema)))),
                }
            }
            Action::OpenSessions(id) => {
                let Some(c) = self.conn(id) else { return };
                let (kind, product) = (c.cfg.kind, c.cfg.product);
                match self.tabs.iter().find(|t| t.conn == id && matches!(t.kind, TabKind::Sessions(_))) {
                    Some(t) => self.active = Some(t.id),
                    None => self.add_tab(id, TabKind::Sessions(Box::new(SessionsView::new(kind, product)))),
                }
            }
            Action::OpenAdmin(id) => {
                let Some(c) = self.conn(id) else { return };
                let (kind, database) = (c.cfg.kind, c.cfg.database.clone());
                match self.tabs.iter().find(|t| t.conn == id && matches!(t.kind, TabKind::Admin(_))) {
                    Some(t) => self.active = Some(t.id),
                    None => self.add_tab(id, TabKind::Admin(Box::new(AdminView::new(kind, database)))),
                }
            }
            Action::OpenCompare(id, db, schema) => {
                let Some(c) = self.conn(id) else { return };
                let cfg = c.cfg.clone();
                self.add_tab(id, TabKind::Compare(Box::new(CompareView::new(cfg, db, schema))));
            }
            Action::OpenCopy(id, db, schema, table) => {
                let Some(c) = self.conn(id) else { return };
                let cfg = c.cfg.clone();
                self.add_tab(id, TabKind::Copy(Box::new(CopyView::new(cfg, db, schema, table))));
            }
            Action::OpenConsoleWith(id, db, sql) => self.open_console(id, db, Some(sql)),
            Action::OpenScript(id, db, path) => match crate::tabs::read_script(&path) {
                Ok(sql) => {
                    self.open_console(id, db, None);
                    if let Some(TabKind::Console(c)) = self.tabs.last_mut().map(|t| &mut t.kind) {
                        c.set_file(path.clone(), sql);
                    }
                    self.handle(Action::RecentScript(path), ctx);
                }
                Err(e) => shadcn_egui::toast_error(ctx, tr("Could not open the file"), Some(&e)),
            },
            Action::RememberParams(values) => {
                self.settings.params.extend(values);
                self.save_settings();
            }
            Action::RecentScript(path) => {
                self.settings.add_recent(path);
                self.save_settings();
            }
            Action::RecordQuery(e) => {
                if let Err(e) = self.history.add(*e) {
                    self.status = trf("Could not save the query history: {error}", &[("error", &e)]);
                }
            }
            Action::StartJob(job) => self.start_transfer(*job),
            Action::ExportData(id, source) => {
                if let Some(c) = self.conn(id) {
                    self.export = Some(ExportDialog::new(id, c.cfg.clone(), source));
                }
            }
            Action::ImportData(id, path) => {
                let Some(c) = self.conn(id) else { return };
                let mut dialog = ImportDialog::new(id, c.cfg.clone(), path.clone());
                match c.details.get(&path) {
                    Some(Load::Ready(d)) => dialog.set_details(d.clone()),
                    Some(Load::Loading) => {}
                    _ => self.handle(Action::LoadDetails(id, path), ctx),
                }
                self.import = Some(dialog);
            }
            Action::ImportNewTable(id, db, schema) => {
                let Some(c) = self.conn(id) else { return };
                self.import = Some(ImportDialog::new_table(id, c.cfg.clone(), &db, &schema));
            }
            Action::GenerateData(id, path) => {
                let Some(c) = self.conn(id) else { return };
                let mut dialog = GenerateDialog::new(id, c.cfg.clone(), path.clone());
                match c.details.get(&path) {
                    Some(Load::Ready(d)) => dialog.set_details(d.clone()),
                    Some(Load::Loading) => {}
                    _ => self.handle(Action::LoadDetails(id, path), ctx),
                }
                self.generate = Some(dialog);
            }
            Action::CopyText(text) => {
                ctx.copy_text(text.clone());
                toast(ctx, tr("Copied"), Some(&text));
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
        self.status = trf("Connecting to {name}…", &[("name", &cfg.display_name())]);
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

    fn open_backup(&mut self, id: Uuid, mode: Mode, scope: crate::backup_dialog::Scope) {
        if let Some(c) = self.conn(id) {
            let tools = self.settings.tools_dir.clone();
            self.backup = Some(BackupDialog::new(id, c.cfg.clone(), &c.version, mode, scope, tools));
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
        self.add_tab(id, TabKind::Console(Box::new(console)));
    }

    fn close_tab(&mut self, id: TabId) {
        let open_tx = self.tabs.iter().any(|t| t.id == id && matches!(&t.kind, TabKind::Console(c) if c.in_tx));
        if open_tx && self.closing != Some(id) {
            self.closing = Some(id);
            return;
        }
        if self.closing == Some(id) {
            self.closing = None;
        }
        if let Some(i) = self.tabs.iter().position(|t| t.id == id) {
            if let TabKind::Console(c) = &self.tabs[i].kind {
                c.roll_back_on_close(&self.tasks);
            }
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

    fn start_transfer(&mut self, job: Start) {
        let Start { title, spec, output, refresh } = job;
        self.jobs.start(&self.tasks, title.clone(), spec, output, refresh);
        self.status = format!("{title}…");
    }

    /// The open export dialog.
    pub fn connection_dialog(&mut self) -> Option<&mut ConnectionDialog> {
        self.dialog.as_mut()
    }

    pub fn export_dialog(&mut self) -> Option<&mut ExportDialog> {
        self.export.as_mut()
    }

    /// The open test data dialog.
    pub fn generate_dialog(&mut self) -> Option<&mut GenerateDialog> {
        self.generate.as_mut()
    }

    /// The open import dialog.
    pub fn import_dialog(&mut self) -> Option<&mut ImportDialog> {
        self.import.as_mut()
    }

    pub fn backup_dialog_open(&self) -> bool {
        self.backup.is_some()
    }

    /// The open backup or restore dialog.
    pub fn backup_dialog(&mut self) -> Option<&mut BackupDialog> {
        self.backup.as_mut()
    }

    /// "DBMan 1.2.3 - database - table": the parts the active tab has.
    pub fn window_title(&self) -> String {
        let mut title = format!("DBMan {}", env!("CARGO_PKG_VERSION"));
        let Some(tab) = self.active.and_then(|id| self.tabs.iter().find(|t| t.id == id)) else {
            return title;
        };
        let cfg = self.conns.iter().find(|c| c.cfg.id == tab.conn).map(|c| &c.cfg);
        // A file database is known by its file, not by "main".
        let database = match cfg {
            Some(cfg) if cfg.kind.is_file_based() => std::path::Path::new(&cfg.database)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .or_else(|| tab.database().map(str::to_owned)),
            _ => tab.database().map(str::to_owned),
        };
        for part in [database, tab.entity()].into_iter().flatten() {
            title.push_str(" - ");
            title.push_str(&part);
        }
        title
    }

    /// Titles of the open tabs, in order.
    pub fn tab_titles(&self) -> Vec<String> {
        self.tabs.iter().map(Tab::title).collect()
    }

    /// The diagram tab on screen, if the active tab is one.
    pub fn active_diagram(&mut self) -> Option<&mut ErView> {
        let active = self.active?;
        match &mut self.tabs.iter_mut().find(|t| t.id == active)?.kind {
            TabKind::Diagram(d) => Some(d.as_mut()),
            _ => None,
        }
    }

    /// The comparison tab on screen, if the active tab is one.
    pub fn active_compare(&mut self) -> Option<&mut CompareView> {
        let active = self.active?;
        match &mut self.tabs.iter_mut().find(|t| t.id == active)?.kind {
            TabKind::Compare(c) => Some(c.as_mut()),
            _ => None,
        }
    }

    /// The copy tab on screen, if the active tab is one.
    pub fn active_copy(&mut self) -> Option<&mut CopyView> {
        let active = self.active?;
        match &mut self.tabs.iter_mut().find(|t| t.id == active)?.kind {
            TabKind::Copy(c) => Some(c.as_mut()),
            _ => None,
        }
    }

    /// The value last given to a query parameter.
    pub fn remembered_param(&self, name: &str) -> Option<&str> {
        self.settings.params.get(name).map(String::as_str)
    }

    /// Brings the tab at `index` (counted from the left) to the front.
    pub fn select_tab(&mut self, index: usize) {
        if let Some(t) = self.tabs.get(index) {
            self.active = Some(t.id);
        }
    }

    /// The sessions tab on screen, if the active tab is one.
    pub fn active_sessions(&mut self) -> Option<&mut SessionsView> {
        let active = self.active?;
        match &mut self.tabs.iter_mut().find(|t| t.id == active)?.kind {
            TabKind::Sessions(s) => Some(s.as_mut()),
            _ => None,
        }
    }

    /// The users and roles tab on screen, if the active tab is one.
    pub fn active_admin(&mut self) -> Option<&mut AdminView> {
        let active = self.active?;
        match &mut self.tabs.iter_mut().find(|t| t.id == active)?.kind {
            TabKind::Admin(a) => Some(a.as_mut()),
            _ => None,
        }
    }

    /// Switches the interface language and remembers it.
    pub fn set_language(&mut self, lang: Lang) {
        i18n::set(lang);
        self.settings.language = lang.code().to_string();
        self.save_settings();
    }

    /// Adds a saved connection, as the connection dialog does.
    pub fn add_connection(&mut self, cfg: ConnectionConfig) {
        self.conns.push(ConnState::new(cfg));
        self.save_settings();
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
    /// The autocomplete items and the text of the first console (tests).
    pub fn console_completion(&self) -> (Vec<String>, String) {
        self.tabs
            .iter()
            .find_map(|t| match &t.kind {
                TabKind::Console(c) => Some((c.completions(), c.sql.clone())),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// The plan the first console shows (tests).
    pub fn console_plan(&self) -> Vec<String> {
        self.tabs
            .iter()
            .find_map(|t| match &t.kind {
                TabKind::Console(c) => Some(c.plan_outline()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// What the first console's history panel lists (tests).
    pub fn console_history(&self) -> Vec<String> {
        self.tabs
            .iter()
            .find_map(|t| match &t.kind {
                TabKind::Console(c) => Some(c.history_shown(&self.history).into_iter().map(String::from).collect()),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub fn recent_scripts(&self) -> &[std::path::PathBuf] {
        &self.settings.recent_scripts
    }

    /// The title, script file and text of the active console (tests).
    pub fn active_console(&self) -> Option<(String, Option<std::path::PathBuf>, String)> {
        self.tabs.iter().filter(|t| Some(t.id) == self.active).find_map(|t| match &t.kind {
            TabKind::Console(c) => Some((c.title(), c.file.clone(), c.sql.clone())),
            _ => None,
        })
    }

    /// Whether the active console has a transaction open (tests).
    pub fn console_in_tx(&self) -> Option<bool> {
        self.tabs.iter().filter(|t| Some(t.id) == self.active).find_map(|t| match &t.kind {
            TabKind::Console(c) => Some(c.in_tx),
            _ => None,
        })
    }

    /// What the active console's value panel shows ("JSON"…), if it is open (tests).
    pub fn console_value_kind(&self) -> Option<String> {
        self.tabs.iter().filter(|t| Some(t.id) == self.active).find_map(|t| match &t.kind {
            TabKind::Console(c) if c.value.open => Some(c.value.kind.clone()),
            _ => None,
        })
    }

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
        self.save_workspace(false);
        let t = Theme::get(&ctx);
        let mut actions = Vec::new();

        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::K)) {
            self.palette = !self.palette;
        }
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
                if ui.add(Button::new(tr("+ New connection")).size(ButtonSize::Sm)).clicked() {
                    actions.push(Action::NewConnection);
                }
                let current = self.current_conn();
                if ui
                    .add_enabled(current.is_some(), Button::outline(tr("New console")).size(ButtonSize::Sm))
                    .tooltip(tr("Open a SQL console for the selected connection (Ctrl+T)"))
                    .clicked()
                    && let Some(id) = current
                {
                    actions.push(Action::OpenConsole(id, None));
                }
                ui.add_space(4.0);
                if ui
                    .add(Button::ghost(tr("Search…")).size(ButtonSize::Sm))
                    .tooltip(tr("Search tables, tabs and commands (Ctrl+K)"))
                    .clicked()
                {
                    self.palette = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut dark = self.settings.dark;
                    if ui.add(Switch::new(&mut dark).label(tr("Dark"))).changed() {
                        self.settings.dark = dark;
                        self.save_settings();
                    }
                    ui.add_space(8.0);
                    let langs: Vec<&str> = Lang::ALL.iter().map(|l| l.name()).collect();
                    let mut lang = Lang::ALL.iter().position(|l| *l == i18n::current());
                    if ui.push_id("language", |ui| ui.add(Select::new(&mut lang, &langs).width(110.0))).inner.changed()
                        && let Some(l) = lang.and_then(|i| Lang::ALL.get(i).copied())
                    {
                        self.set_language(l);
                    }
                });
            });
        });

        let status_frame = egui::Frame::new().fill(t.muted).inner_margin(egui::Margin::symmetric(12, 4));
        egui::Panel::bottom("status").frame(status_frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&self.status).size(12.0).color(t.muted_foreground));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !self.jobs.list.is_empty() {
                        let running = self.jobs.running();
                        let text = if running > 0 {
                            trf("{running} running", &[("running", &running)])
                        } else {
                            tr("Tasks").to_string()
                        };
                        if ui.add(Button::ghost(text).size(ButtonSize::Sm)).tooltip(tr("Background tasks")).clicked() {
                            self.jobs.open = !self.jobs.open;
                        }
                        if running > 0 {
                            ui.add(shadcn_egui::Spinner::new().size(12.0));
                        }
                    }
                    let open = self.conns.iter().filter(|c| c.session.ready().is_some()).count();
                    ui.label(
                        RichText::new(trf("{open} connected", &[("open", &open)])).size(12.0).color(t.muted_foreground),
                    );
                });
            });
        });

        if self.jobs.open && !self.jobs.list.is_empty() {
            let frame = egui::Frame::new().fill(t.background).inner_margin(egui::Margin::symmetric(12, 8));
            egui::Panel::bottom("jobs").resizable(false).frame(frame).show(ui, |ui| self.jobs.ui(ui));
        }

        let side = egui::Frame::new().fill(t.background).inner_margin(egui::Margin::same(8));
        egui::Panel::left("explorer").resizable(true).default_size(300.0).min_size(200.0).frame(side).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(tr("Database Explorer")).strong().color(t.foreground));
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
            // A colored connection draws a line over its tabs' content, so production stands out.
            if let Some(color) = conn.and_then(|c| explorer::conn_color(c.cfg.color)) {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 3.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 1.5, color);
                ui.add_space(4.0);
            }
            tab.ui(
                ui,
                TabCtx {
                    tasks: &self.tasks,
                    conn,
                    actions: &mut actions,
                    page_size,
                    conn_id,
                    conns: &self.conns,
                    history: &self.history,
                    recent_scripts: &self.settings.recent_scripts,
                    params: &self.settings.params,
                },
            );
        });

        let current = self.current_conn();
        let picked = palette::show(
            &ctx,
            &mut self.palette,
            palette::Ctx {
                conns: &self.conns,
                tabs: &self.tabs,
                active: self.active,
                current,
                dark: self.settings.dark,
            },
        );
        match picked {
            Some(Pick::Action(a)) => actions.push(a),
            Some(Pick::Tab(id)) => self.active = Some(id),
            Some(Pick::CloseTab(id)) => self.close_tab(id),
            Some(Pick::Conn(id)) => {
                self.selected = Some(Node::Conn(id));
                if self.conn(id).is_some_and(|c| c.session.is_idle()) {
                    actions.push(Action::Connect(id));
                }
            }
            Some(Pick::ToggleDark) => {
                self.settings.dark = !self.settings.dark;
                self.save_settings();
            }
            None => {}
        }

        for a in actions {
            self.handle(a, &ctx);
        }
        self.dialogs(&ctx);
        Toaster::show(&ctx);
        let title = self.window_title();
        if self.shown_title != title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.shown_title = title;
        }
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
                    let conn = self.conns.iter().find(|c| c.cfg.id == tab.conn);
                    let kind = conn.map(|c| (c.cfg.kind, c.cfg.product));
                    let color = conn.map_or(dbman_core::ConnColor::None, |c| c.cfg.color);
                    let frame = egui::Frame::new()
                        .fill(if active { t.secondary } else { t.background })
                        .corner_radius(t.radius_md())
                        .inner_margin(egui::Margin::symmetric(8, 4));
                    // The tab's own click area goes in first, so the × and the
                    // title on top of it still get their clicks.
                    let r = ui
                        .scope_builder(egui::UiBuilder::new().sense(egui::Sense::click()), |ui| {
                            frame.show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    explorer::color_mark(ui, color);
                                    if let Some((k, p)) = kind {
                                        explorer::kind_badge(ui, k, p);
                                    }
                                    let icon = match tab.kind {
                                        TabKind::Console(_) => Icon::Console,
                                        TabKind::Data(_) => Icon::Table,
                                        TabKind::Object(_) => Icon::Info,
                                        TabKind::TableEditor(_) => Icon::Table,
                                        TabKind::Diagram(_) => Icon::ForeignKey,
                                        TabKind::Compare(_) => Icon::Schema,
                                        TabKind::Copy(_) => Icon::Table,
                                        TabKind::Sessions(_) => Icon::Console,
                                        TabKind::Admin(_) => Icon::Key,
                                    };
                                    let color = if active { t.foreground } else { t.muted_foreground };
                                    icons::icon(ui, icon, color);
                                    ui.add(egui::Label::new(RichText::new(tab.title()).color(color)).selectable(false));
                                    if ui
                                        .add(Button::ghost("×").size(ButtonSize::Icon))
                                        .tooltip(tr("Close (Ctrl+W)"))
                                        .clicked()
                                    {
                                        close = Some(tab.id);
                                    }
                                });
                            })
                        })
                        .response;
                    if r.middle_clicked() {
                        close = Some(tab.id);
                    } else if r.clicked() {
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
            if ui.add(Button::new(tr("+ New connection"))).clicked() {
                actions.push(Action::NewConnection);
            }
            ui.add_space(16.0);
            for (keys, what) in [
                ("Double-click a table", "open its data"),
                ("Ctrl+K", "search tables, tabs and commands"),
                ("Ctrl+T", "new SQL console"),
                ("Ctrl+Enter", "run the statement under the cursor"),
                ("Ctrl+Shift+Enter", "run the whole script"),
                ("Ctrl+W", "close the tab"),
            ] {
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 360.0).max(0.0) / 2.0);
                    shadcn_egui::kbd(ui, tr(keys));
                    ui.label(RichText::new(tr(what)).color(t.muted_foreground));
                });
            }
        });
    }

    /// Folders the connections are in, by name.
    fn folders(&self) -> Vec<String> {
        let mut f: Vec<String> = self.conns.iter().map(|c| c.cfg.folder.clone()).filter(|f| !f.is_empty()).collect();
        f.sort_unstable_by_key(|f| f.to_lowercase());
        f.dedup();
        f
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let folders = if self.dialog.is_some() { self.folders() } else { Vec::new() };
        if let Some(d) = &mut self.dialog {
            d.folders = folders;
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
        if let Some(id) = self.closing {
            let description =
                tr("Its transaction is still open. Closing rolls back the changes that were not committed.");
            match shadcn_egui::alert_dialog(
                ctx,
                "close-open-tx",
                tr("Close the console?"),
                description,
                tr("Roll back and close"),
            ) {
                Some(true) => self.close_tab(id),
                Some(false) => self.closing = None,
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
        if let Some(b) = &mut self.backup {
            match b.show(ctx) {
                Some(BackupOutcome::Close) => self.backup = None,
                Some(BackupOutcome::Start(job)) => {
                    let StartJob { title, spec, output, refresh, tools_dir } = *job;
                    if tools_dir != self.settings.tools_dir {
                        self.settings.tools_dir = tools_dir;
                        self.save_settings();
                    }
                    self.jobs.start(&self.tasks, title.clone(), spec, output, refresh);
                    self.status = format!("{title}…");
                    self.backup = None;
                }
                None => {}
            }
        }
        if let Some(d) = &mut self.export {
            match d.show(ctx) {
                Some(TransferOutcome::Close) => self.export = None,
                Some(TransferOutcome::Start(job)) => {
                    self.start_transfer(*job);
                    self.export = None;
                }
                None => {}
            }
        }
        if let Some(d) = &mut self.import {
            if d.needs_details()
                && let Some(Load::Ready(details)) =
                    self.conns.iter().find(|c| c.cfg.id == d.conn()).and_then(|c| c.details.get(d.table()))
            {
                d.set_details(details.clone());
            }
            match d.show(ctx) {
                Some(TransferOutcome::Close) => self.import = None,
                Some(TransferOutcome::Start(job)) => {
                    self.start_transfer(*job);
                    self.import = None;
                }
                None => {}
            }
        }
        if let Some(d) = &mut self.generate {
            if d.needs_details()
                && let Some(Load::Ready(details)) =
                    self.conns.iter().find(|c| c.cfg.id == d.conn()).and_then(|c| c.details.get(d.table()))
            {
                d.set_details(details.clone());
            }
            match d.show(ctx) {
                Some(TransferOutcome::Close) => self.generate = None,
                Some(TransferOutcome::Start(job)) => {
                    self.start_transfer(*job);
                    self.generate = None;
                }
                None => {}
            }
        }
        if let Some(id) = self.confirm_delete {
            let name = self.conn(id).map(|c| c.cfg.display_name()).unwrap_or_default();
            match alert_dialog(
                ctx,
                "delete-connection",
                tr("Delete connection?"),
                &trf(
                    "\"{name}\" will be removed from the list. The database itself is not touched.",
                    &[("name", &name)],
                ),
                tr("Delete"),
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
        self.jobs.cancel_all();
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

impl eframe::App for DbManApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        DbManApp::ui(self, ui);
    }

    fn on_exit(&mut self) {
        // Typing from the last second has not been saved yet.
        self.save_workspace(true);
    }
}

/// The connection an action would change, for actions a read-only connection refuses.
fn changes(action: &Action) -> Option<Uuid> {
    match action {
        Action::NewTable(id, ..)
        | Action::EditTable(id, _)
        | Action::NewFromTemplate(id, ..)
        | Action::NewDatabase(id)
        | Action::NewSchema(id, _)
        | Action::DropDatabase(id, _)
        | Action::DropSchema(id, ..)
        | Action::DropObject(id, _)
        | Action::RenameObject(id, _)
        | Action::Truncate(id, _)
        | Action::Restore(id, _)
        | Action::ImportData(id, _)
        | Action::ImportNewTable(id, ..)
        | Action::GenerateData(id, _) => Some(*id),
        _ => None,
    }
}
