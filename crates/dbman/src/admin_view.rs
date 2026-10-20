//! Users and roles tab: who may connect and what they may do. Editors create,
//! change and drop users and roles, put users into roles, and grant and
//! revoke privileges; each shows the exact SQL before it runs. The explorer
//! offers the tab only when the connected user may manage users.

use std::sync::Arc;

use dbman_core::admin::{self, Edit, Grant, Membership, Principal, Scope, Target};
use dbman_core::{Connection, ConnectionConfig, DbKind, DbResult, StatementOutcome};
use egui::{Context, RichText, Ui};
use shadcn_egui::{
    Alert, Badge, Button, Checkbox, Column, DataTable, Dialog, Input, Select, Switch, Theme, footer, label, menu_item,
    toast,
};

use crate::events::{Event, TabEvent, TabId};
use crate::exec;
use crate::explorer::{Action, Load};
use crate::i18n::{tr, trf};
use crate::sql_editor;
use crate::tabs::{TabCtx, error_alert, not_connected, toolbar_button};

pub enum AdminEvent {
    Listed(DbResult<(Vec<Principal>, Vec<Membership>)>),
    /// The privileges of a user or role in a database.
    Grants {
        of: (String, String),
        result: DbResult<Vec<Grant>>,
    },
    /// The statements of an editor ran; `done` is the message for success.
    Ran {
        result: DbResult<usize>,
        done: String,
    },
}

/// What an editor dialog edits.
enum Form {
    /// A new user or role (`old` is `None`), or changes to one.
    Principal { old: Option<Principal>, edit: Edit },
    /// Adds a member to a role: `fixed` is the role when `fixed_is_role`,
    /// otherwise the member, and the other side is picked from `candidates`.
    Join { fixed: Principal, fixed_is_role: bool, candidates: Vec<Principal>, pick: Option<usize>, admin: bool },
    /// Grants privileges to a user or role.
    Grant { to: Principal, scope: usize, target: Target, picked: Vec<bool>, grant_option: bool },
    /// Runs prepared statements once the user agrees (drops and revokes).
    Confirm { title: String, description: String, statements: Vec<String>, button: String, done: String },
}

struct Change {
    form: Form,
    running: bool,
    error: Option<String>,
}

pub struct AdminView {
    pub kind: DbKind,
    /// Where privileges are listed and changed (PostgreSQL and SQL Server),
    /// and where SQL Server users live.
    pub database: String,
    list: Load<()>,
    principals: Vec<Principal>,
    memberships: Vec<Membership>,
    grants: Load<Vec<Grant>>,
    /// Whose privileges `grants` holds, and in which database.
    grants_of: Option<(String, String)>,
    /// The [`Principal::id`] shown on the right.
    picked: Option<String>,
    show_system: bool,
    version: u64,
    change: Option<Change>,
}

impl AdminView {
    pub fn new(kind: DbKind, database: String) -> Self {
        let database = match kind {
            _ if !admin::per_database(kind) => String::new(),
            _ if !database.is_empty() => database,
            DbKind::MsSql => "master".into(),
            _ => "postgres".into(),
        };
        Self {
            kind,
            database,
            list: Load::Idle,
            principals: Vec::new(),
            memberships: Vec::new(),
            grants: Load::Idle,
            grants_of: None,
            picked: None,
            show_system: false,
            version: 0,
            change: None,
        }
    }

    pub fn title(&self) -> String {
        tr("Users and roles").into()
    }

    /// The users and roles listed last.
    pub fn principals(&self) -> &[Principal] {
        &self.principals
    }

    /// The privileges shown for the picked user or role.
    pub fn grants(&self) -> &[Grant] {
        self.grants.ready().map_or(&[], Vec::as_slice)
    }

    /// Shows a user or role on the right, as a click on its row does.
    pub fn pick(&mut self, id: &str) {
        self.picked = Some(id.to_string());
    }

    pub fn on_event(&mut self, ev: TabEvent, ctx: &Context) {
        let TabEvent::Admin(ev) = ev else { return };
        match *ev {
            AdminEvent::Listed(result) => match result {
                Ok((principals, memberships)) => {
                    self.principals = principals;
                    self.memberships = memberships;
                    self.version += 1;
                    self.list = Load::Ready(());
                }
                Err(e) => self.list = Load::Failed(e.message),
            },
            AdminEvent::Grants { of, result } => {
                if self.grants_of.as_ref() == Some(&of) {
                    self.grants = Load::from_result(result);
                }
            }
            AdminEvent::Ran { result, done } => match result {
                Ok(_) => {
                    toast(ctx, done, None);
                    self.change = None;
                    self.reload();
                }
                Err(e) => {
                    if let Some(c) = &mut self.change {
                        c.running = false;
                        c.error = Some(e.message);
                    }
                }
            },
        }
    }

    /// Forgets what was loaded, so the next frame loads it again.
    fn reload(&mut self) {
        self.list = Load::Idle;
        self.grants = Load::Idle;
        self.grants_of = None;
    }

    /// The database listings run in (`None` where users are server-wide).
    fn db(&self) -> Option<String> {
        (!self.database.is_empty()).then(|| self.database.clone())
    }

    fn load(&mut self, cx: &TabCtx<'_>, tab: TabId, session: &Arc<dyn Connection>) {
        self.list = Load::Loading;
        let (s, kind, db) = (session.clone(), self.kind, self.db());
        cx.tasks.spawn(async move {
            let result = async {
                let principals = first_working(&s, db.as_deref(), admin::principals_sql(kind)).await?;
                let memberships = first_working(&s, db.as_deref(), admin::memberships_sql(kind)).await?;
                Ok((admin::parse_principals(&principals), admin::parse_memberships(&memberships)))
            }
            .await;
            Event::Tab { tab, event: TabEvent::Admin(Box::new(AdminEvent::Listed(result))) }
        });
    }

    fn load_grants(&mut self, cx: &TabCtx<'_>, tab: TabId, session: &Arc<dyn Connection>, p: &Principal) {
        let Some(sql) = admin::grants_sql(self.kind, p) else { return };
        let of = (p.id(), self.database.clone());
        self.grants = Load::Loading;
        self.grants_of = Some(of.clone());
        let (s, db) = (session.clone(), self.db());
        cx.tasks.spawn(async move {
            let result = s.execute(db.as_deref(), &sql, 100_000).await.map(|o| match o {
                StatementOutcome::Rows(r) => admin::parse_grants(&r),
                StatementOutcome::Done { .. } => Vec::new(),
            });
            Event::Tab { tab, event: TabEvent::Admin(Box::new(AdminEvent::Grants { of, result })) }
        });
    }

    fn find(&self, id: &str) -> Option<&Principal> {
        self.principals.iter().find(|p| p.id() == id)
    }

    pub fn ui(&mut self, ui: &mut Ui, tab: TabId, mut cx: TabCtx<'_>) {
        let Some(conn) = cx.conn else {
            not_connected(ui, &mut cx);
            return;
        };
        let Some(session) = conn.session() else {
            not_connected(ui, &mut cx);
            return;
        };
        let (cfg, read_only) = (conn.cfg.clone(), conn.cfg.read_only);
        if self.list.is_idle() {
            self.load(&cx, tab, &session);
        }
        let picked = self.picked.as_deref().and_then(|id| self.find(id)).cloned();
        if let Some(p) = &picked
            && self.grants_of.as_ref() != Some(&(p.id(), self.database.clone()))
        {
            self.load_grants(&cx, tab, &session, p);
        }
        let t = Theme::get(ui.ctx());

        // Toolbar.
        ui.horizontal(|ui| {
            if toolbar_button(ui, tr("⟳ Refresh")).clicked() {
                self.reload();
            }
            if !read_only {
                if toolbar_button(ui, tr("New user…")).clicked() {
                    self.open(Form::Principal { old: None, edit: Edit::new(self.kind, false) });
                }
                if toolbar_button(ui, tr("New role…")).clicked() {
                    self.open(Form::Principal { old: None, edit: Edit::new(self.kind, true) });
                }
            }
            if admin::per_database(self.kind) {
                ui.add_space(8.0);
                ui.label(RichText::new(tr("Database")).color(t.muted_foreground));
                match conn.databases.ready() {
                    Some(list) => {
                        let names: Vec<&str> = list.iter().map(String::as_str).collect();
                        let mut idx = names.iter().position(|d| *d == self.database);
                        if ui
                            .push_id("admin-db", |ui| ui.add(Select::new(&mut idx, &names).width(180.0)))
                            .inner
                            .changed()
                            && let Some(d) = idx.and_then(|i| names.get(i))
                        {
                            self.database = d.to_string();
                            self.reload();
                        }
                    }
                    None => {
                        if conn.databases.is_idle() {
                            cx.actions.push(Action::LoadDatabases(cx.conn_id));
                        }
                        ui.label(&self.database);
                    }
                }
            }
            ui.add_space(8.0);
            if ui.add(Switch::new(&mut self.show_system).label(tr("Show built-in"))).changed() {
                self.version += 1;
            }
            if matches!(self.list, Load::Loading) {
                ui.add(shadcn_egui::Spinner::new().size(12.0));
            }
            if read_only {
                ui.label(RichText::new(tr("Read-only connection: nothing can be changed")).color(t.muted_foreground));
            }
        });
        ui.add_space(8.0);
        if let Load::Failed(e) = &self.list {
            error_alert(ui, tr("Could not list users and roles"), e);
            return;
        }

        let height = ui.available_height();
        let left = (ui.available_width() * 0.38).clamp(280.0, 520.0);
        let mut new_pick = None;
        let mut open = None;
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(egui::vec2(left, height), egui::Layout::top_down(egui::Align::LEFT), |ui| {
                ui.set_width(left);
                new_pick = self.list_ui(ui, tab, height);
            });
            ui.separator();
            ui.vertical(|ui| {
                egui::ScrollArea::vertical().id_salt(("admin-detail", tab)).auto_shrink([false, false]).show(
                    ui,
                    |ui| match &picked {
                        Some(p) => open = self.detail_ui(ui, tab, p, read_only),
                        None => {
                            ui.add_space(24.0);
                            ui.label(
                                RichText::new(tr("Pick a user or role to see its roles and privileges."))
                                    .color(t.muted_foreground),
                            );
                        }
                    },
                );
            });
        });
        if let Some(id) = new_pick {
            self.picked = Some(id);
        }
        if let Some(form) = open {
            self.open(form);
        }

        self.change_ui(ui.ctx(), &cx, tab, cfg);
    }

    /// The table of users and roles; returns the id of a clicked row.
    fn list_ui(&self, ui: &mut Ui, tab: TabId, height: f32) -> Option<String> {
        let t = Theme::get(ui.ctx());
        let kind = self.kind;
        let rows: Vec<&Principal> = self.principals.iter().filter(|p| self.show_system || !p.system).collect();
        let texts: Vec<Vec<String>> = rows
            .iter()
            .map(|p| {
                let attrs: Vec<&str> = admin::options(kind)
                    .iter()
                    .filter(|o| p.has(o.key) && !(kind == DbKind::Postgres && o.key == "LOGIN"))
                    .filter(|o| !(o.default && kind == DbKind::Postgres))
                    .map(|o| tr(o.label))
                    .collect();
                vec![display_name(p), tr(if p.role { "Role" } else { "User" }).to_string(), attrs.join(", ")]
            })
            .collect();
        let columns = vec![
            Column::new(tr("Name")).sortable().weight(1.4),
            Column::new(tr("Type")).sortable().weight(0.6),
            Column::new(tr("Attributes")),
        ];
        let picked = self.picked.clone();
        let resp = DataTable::new(("admin-principals", tab), columns)
            .filter(tr("Filter users and roles…"))
            .row_height(30.0)
            .resizable()
            .version(self.version)
            .max_height((height - 60.0).max(120.0))
            .cell_ui(|ui, row, col| {
                let p = rows[row];
                if col != 0 {
                    return false;
                }
                let on = picked.as_deref() == Some(p.id().as_str());
                let color = if p.system { t.muted_foreground } else { t.foreground };
                let text = RichText::new(display_name(p)).color(color);
                ui.label(if on { text.strong() } else { text });
                true
            })
            .show(ui, &texts);
        resp.clicked.and_then(|i| rows.get(i)).map(|p| p.id())
    }

    /// The picked user or role: what it is, its roles, its members and its privileges.
    /// Returns the editor a button asked for.
    fn detail_ui(&self, ui: &mut Ui, tab: TabId, p: &Principal, read_only: bool) -> Option<Form> {
        let t = Theme::get(ui.ctx());
        let kind = self.kind;
        let mut open = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new(display_name(p)).size(18.0).strong().color(t.foreground));
            ui.add(Badge::new(tr(if p.role { "Role" } else { "User" })).secondary());
            if p.system {
                ui.add(Badge::new(tr("built-in")).outline());
            }
        });
        let attrs: Vec<&str> = admin::options(kind).iter().filter(|o| p.has(o.key)).map(|o| tr(o.label)).collect();
        let mut facts = attrs.join(", ");
        for extra in [p.note.as_str(), &p.login].into_iter().filter(|s| !s.is_empty() && **s != p.name) {
            if !facts.is_empty() {
                facts.push_str(" · ");
            }
            facts.push_str(extra);
        }
        if !facts.is_empty() {
            ui.label(RichText::new(facts).color(t.muted_foreground));
        }
        if !read_only {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if toolbar_button(ui, tr("Edit…")).clicked() {
                    open = Some(Form::Principal { old: Some(p.clone()), edit: Edit::of(p) });
                }
                if toolbar_button(ui, tr("Grant privileges…")).clicked() {
                    open = Some(self.grant_form(p));
                }
                if toolbar_button(ui, tr("Add to role…")).clicked() {
                    open = Some(self.join_form(p, false));
                }
                if p.role && toolbar_button(ui, tr("Add member…")).clicked() {
                    open = Some(self.join_form(p, true));
                }
                if ui.add(Button::destructive(tr("Drop…")).size(shadcn_egui::ButtonSize::Sm)).clicked() {
                    let what = if p.role { tr("role") } else { tr("user") };
                    let mut description = tr("This cannot be undone.").to_string();
                    if kind == DbKind::MsSql && !p.login.is_empty() {
                        description = trf("The login {login} stays on the server.", &[("login", &p.login)]);
                    }
                    open = Some(Form::Confirm {
                        title: trf("Drop {what} {name}?", &[("what", &what), ("name", &display_name(p))]),
                        description,
                        statements: vec![admin::drop(kind, p)],
                        button: tr("Drop").into(),
                        done: trf("{name} dropped", &[("name", &display_name(p))]),
                    });
                }
            });
        }

        // Roles it belongs to, and members of a role.
        let id = p.id();
        let sections = [
            (tr("Member of"), self.memberships.iter().filter(|m| m.member == id).collect::<Vec<_>>(), false),
            (tr("Members"), self.memberships.iter().filter(|m| m.role == id).collect::<Vec<_>>(), true),
        ];
        for (title, list, of_role) in sections {
            if of_role && !p.role && list.is_empty() {
                continue;
            }
            ui.add_space(14.0);
            ui.label(RichText::new(title).strong().color(t.foreground));
            if list.is_empty() {
                ui.label(RichText::new(tr("none")).color(t.muted_foreground));
            }
            ui.horizontal_wrapped(|ui| {
                for m in list {
                    let other = if of_role { &m.member } else { &m.role };
                    egui::Frame::new()
                        .stroke(egui::Stroke::new(1.0, t.border))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(8, 3))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(other).color(t.foreground));
                                if m.admin {
                                    ui.label(RichText::new(tr("admin")).size(11.0).color(t.muted_foreground));
                                }
                                let (role, member) =
                                    if of_role { (p, self.find(other)) } else { (self.find(other)?, Some(p)) };
                                let member = member?;
                                if !read_only
                                    && ui
                                        .add(
                                            egui::Button::new(RichText::new("×").color(t.muted_foreground))
                                                .frame(false),
                                        )
                                        .on_hover_text(tr("Remove from the role"))
                                        .clicked()
                                {
                                    open = Some(Form::Confirm {
                                        title: trf(
                                            "Remove {member} from {role}?",
                                            &[("member", &display_name(member)), ("role", &display_name(role))],
                                        ),
                                        description: tr("The member loses the privileges it had through the role.")
                                            .into(),
                                        statements: vec![admin::revoke_role(kind, role, member)],
                                        button: tr("Remove").into(),
                                        done: trf(
                                            "{member} removed from {role}",
                                            &[("member", &display_name(member)), ("role", &display_name(role))],
                                        ),
                                    });
                                }
                                Some(())
                            });
                        });
                }
            });
        }

        // Privileges.
        ui.add_space(14.0);
        let heading = if admin::per_database(kind) {
            trf("Privileges in {db}", &[("db", &self.database)])
        } else {
            tr("Privileges").into()
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(heading).strong().color(t.foreground));
            ui.label(RichText::new(tr("(granted directly, not through roles)")).color(t.muted_foreground));
        });
        match &self.grants {
            Load::Failed(e) => error_alert(ui, tr("Could not list the privileges"), e),
            Load::Ready(grants) if grants.is_empty() => {
                ui.label(RichText::new(tr("none")).color(t.muted_foreground));
            }
            Load::Ready(grants) => {
                let texts: Vec<Vec<String>> = grants
                    .iter()
                    .map(|g| {
                        let yes = if g.grantable { tr("yes") } else { "" };
                        vec![tr(&g.scope).to_string(), g.object.clone(), g.privilege.clone(), yes.to_string()]
                    })
                    .collect();
                let columns = vec![
                    Column::new(tr("Object type")).sortable().weight(0.7),
                    Column::new(tr("Object")).sortable().weight(1.6),
                    Column::new(tr("Privilege")).sortable(),
                    Column::new(tr("Can grant")).weight(0.6),
                ];
                DataTable::new(("admin-grants", tab), columns)
                    .filter(tr("Filter privileges…"))
                    .row_height(28.0)
                    .resizable()
                    .version(self.version)
                    .max_height(360.0)
                    .row_context_menu(|ui, row| {
                        let g = &grants[row];
                        ui.set_min_width(180.0);
                        if !read_only && menu_item(ui, tr("Revoke…"), None).clicked() {
                            open = Some(revoke_form(kind, p, g));
                        }
                        if menu_item(ui, tr("Copy object name"), None).clicked() {
                            ui.ctx().copy_text(g.object.clone());
                        }
                    })
                    .show(ui, &texts);
                if !read_only {
                    ui.label(
                        RichText::new(tr("Right-click a privilege to revoke it.")).size(12.0).color(t.muted_foreground),
                    );
                }
            }
            _ => {
                ui.add(shadcn_egui::Spinner::new().size(12.0));
            }
        }
        open
    }

    fn open(&mut self, form: Form) {
        self.change = Some(Change { form, running: false, error: None });
    }

    fn grant_form(&self, to: &Principal) -> Form {
        let scope = 0;
        let schema = match self.kind {
            DbKind::Postgres => "public",
            DbKind::MsSql => "dbo",
            _ => "",
        };
        let database = if admin::per_database(self.kind) { self.database.clone() } else { String::new() };
        let first = admin::scopes(self.kind).first().copied().unwrap_or(Scope::Table);
        Form::Grant {
            to: to.clone(),
            scope,
            target: Target { scope: first, database, schema: schema.into(), name: String::new() },
            picked: vec![false; admin::privileges(self.kind, first).len()],
            grant_option: false,
        }
    }

    fn join_form(&self, p: &Principal, fixed_is_role: bool) -> Form {
        let id = p.id();
        let candidates: Vec<Principal> = self
            .principals
            .iter()
            .filter(|o| o.id() != id && (fixed_is_role || o.role))
            .filter(|o| {
                let (role, member) = if fixed_is_role { (id.clone(), o.id()) } else { (o.id(), id.clone()) };
                !self.memberships.iter().any(|m| m.role == role && m.member == member)
            })
            .cloned()
            .collect();
        Form::Join { fixed: p.clone(), fixed_is_role, candidates, pick: None, admin: false }
    }

    /// The open editor, and running its statements.
    fn change_ui(&mut self, ctx: &Context, cx: &TabCtx<'_>, tab: TabId, cfg: ConnectionConfig) {
        let kind = self.kind;
        let database = self.database.clone();
        let Some(change) = &mut self.change else { return };
        match change.show(ctx, kind) {
            Some(Outcome::Close) => self.change = None,
            Some(Outcome::Run { statements, done }) => {
                change.running = true;
                change.error = None;
                cx.tasks.spawn(async move {
                    let result = exec::run(cfg, database, statements, exec::Mode::Sequential).await;
                    Event::Tab { tab, event: TabEvent::Admin(Box::new(AdminEvent::Ran { result, done })) }
                });
            }
            None => {}
        }
    }
}

enum Outcome {
    Run { statements: Vec<String>, done: String },
    Close,
}

impl Change {
    fn title(&self) -> String {
        match &self.form {
            Form::Principal { old: None, edit } if edit.role => tr("New role").into(),
            Form::Principal { old: None, .. } => tr("New user").into(),
            Form::Principal { old: Some(p), .. } => trf("Edit {name}", &[("name", &display_name(p))]),
            Form::Join { fixed, fixed_is_role: true, .. } => {
                trf("Add a member to {role}", &[("role", &display_name(fixed))])
            }
            Form::Join { fixed, .. } => trf("Add {name} to a role", &[("name", &display_name(fixed))]),
            Form::Grant { to, .. } => trf("Grant privileges to {name}", &[("name", &display_name(to))]),
            Form::Confirm { title, .. } => title.clone(),
        }
    }

    /// The statements to run, or why they cannot be made yet.
    fn statements(&self, kind: DbKind) -> Result<Vec<String>, String> {
        match &self.form {
            Form::Principal { old, edit } => {
                if edit.name.trim().is_empty() {
                    return Err(tr("Enter a name.").into());
                }
                if old.is_none() && kind == DbKind::MsSql && !edit.role && edit.password.is_empty() {
                    return Err(tr("Enter a password for the new login.").into());
                }
                let s = match old {
                    None => admin::create(kind, edit),
                    Some(p) => admin::alter(kind, p, edit),
                };
                if s.is_empty() {
                    return Err(tr("Nothing has changed.").into());
                }
                Ok(s)
            }
            Form::Join { fixed, fixed_is_role, candidates, pick, admin: with_admin } => {
                let Some(other) = pick.and_then(|i| candidates.get(i)) else {
                    return Err(if *fixed_is_role { tr("Pick a member.") } else { tr("Pick a role.") }.into());
                };
                let (role, member) = if *fixed_is_role { (fixed, other) } else { (other, fixed) };
                Ok(vec![admin::grant_role(kind, role, member, *with_admin)])
            }
            Form::Grant { to, target, picked, grant_option, .. } => {
                let (db, schema, name) = target.scope.fields(kind);
                if (db && target.database.trim().is_empty())
                    || (schema && target.schema.trim().is_empty())
                    || (name && target.name.trim().is_empty())
                {
                    return Err(tr("Fill in the object.").into());
                }
                let all = admin::privileges(kind, target.scope);
                let chosen: Vec<&str> = all.iter().zip(picked).filter(|(_, on)| **on).map(|(p, _)| *p).collect();
                if chosen.is_empty() {
                    return Err(tr("Pick at least one privilege.").into());
                }
                let trimmed = Target {
                    scope: target.scope,
                    database: target.database.trim().into(),
                    schema: target.schema.trim().into(),
                    name: target.name.trim().into(),
                };
                let on = admin::target_sql(kind, &trimmed);
                Ok(vec![admin::grant(kind, &chosen, &on, to, *grant_option)])
            }
            Form::Confirm { statements, .. } => Ok(statements.clone()),
        }
    }

    fn done(&self) -> String {
        match &self.form {
            Form::Principal { old: None, edit } => trf("{name} created", &[("name", &edit.name.trim())]),
            Form::Principal { old: Some(p), .. } => trf("{name} changed", &[("name", &display_name(p))]),
            Form::Join { .. } => tr("Role membership added").into(),
            Form::Grant { to, .. } => trf("Privileges granted to {name}", &[("name", &display_name(to))]),
            Form::Confirm { done, .. } => done.clone(),
        }
    }

    fn show(&mut self, ctx: &Context, kind: DbKind) -> Option<Outcome> {
        let mut outcome = None;
        let title = self.title();
        let description = match &self.form {
            Form::Confirm { description, .. } => description.clone(),
            _ => String::new(),
        };
        let mut dialog = Dialog::new("admin-change", title).width(560.0);
        if !description.is_empty() {
            dialog = dialog.description(description);
        }
        let r = dialog.show(ctx, |ui| {
            let t = Theme::get(ui.ctx());
            let width = ui.available_width();
            match &mut self.form {
                Form::Principal { old, edit } => principal_fields(ui, kind, old.as_ref(), edit, width),
                Form::Join { fixed_is_role, candidates, pick, admin: with_admin, .. } => {
                    label(ui, if *fixed_is_role { tr("Member") } else { tr("Role") });
                    let names: Vec<String> = candidates.iter().map(display_name).collect();
                    if names.is_empty() {
                        ui.label(RichText::new(tr("There is nothing to add.")).color(t.muted_foreground));
                    } else {
                        ui.add(
                            shadcn_egui::Combobox::new(pick, &names)
                                .width(width)
                                .placeholder(tr("Select…"))
                                .search_placeholder(tr("Search…")),
                        );
                    }
                    if kind != DbKind::MsSql {
                        ui.add_space(6.0);
                        ui.add(Checkbox::new(with_admin).label(tr("With admin option (may add others to the role)")));
                    }
                }
                Form::Grant { scope, target, picked, grant_option, .. } => {
                    grant_fields(ui, kind, scope, target, picked, width);
                    ui.add_space(6.0);
                    ui.add(Checkbox::new(grant_option).label(tr("With grant option (may grant it to others)")));
                }
                Form::Confirm { .. } => {}
            }
            let statements = self.statements(kind);
            ui.add_space(8.0);
            match &statements {
                Ok(s) => sql_editor::viewer(ui, egui::Id::new("admin-change-sql"), &(s.join(";\n") + ";"), kind),
                Err(e) => {
                    ui.label(RichText::new(e.as_str()).color(t.muted_foreground));
                }
            }
            if let Some(e) = &self.error {
                ui.add_space(8.0);
                Alert::new(tr("Failed")).icon("!").description(e.as_str()).destructive().show(ui);
            }
            ui.add_space(12.0);
            footer(ui, |ui| {
                let (verb, destructive) = match &self.form {
                    Form::Confirm { button, .. } => (button.as_str(), true),
                    Form::Principal { old: None, .. } => (tr("Create"), false),
                    Form::Principal { .. } => (tr("Apply"), false),
                    Form::Join { .. } => (tr("Add"), false),
                    Form::Grant { .. } => (tr("Grant"), false),
                };
                let button = if destructive { Button::destructive(verb) } else { Button::new(verb) };
                if ui.add_enabled(statements.is_ok() && !self.running, button).clicked()
                    && let Ok(s) = statements.clone()
                {
                    outcome = Some(Outcome::Run { statements: s, done: self.done() });
                }
                if ui.add(Button::outline(tr("Cancel"))).clicked() {
                    outcome = Some(Outcome::Close);
                }
                if self.running {
                    ui.add(shadcn_egui::Spinner::new());
                }
            });
        });
        if r.should_close && outcome.is_none() {
            outcome = Some(Outcome::Close);
        }
        outcome
    }
}

fn principal_fields(ui: &mut Ui, kind: DbKind, old: Option<&Principal>, edit: &mut Edit, width: f32) {
    label(ui, tr("Name"));
    let r = ui.add(Input::new(&mut edit.name).width(width));
    if old.is_none() && edit.name.is_empty() && !r.has_focus() {
        r.request_focus();
    }
    // MariaDB roles have no host.
    if kind == DbKind::MySql && !(edit.role && edit.host.is_empty() && old.is_some()) {
        ui.add_space(6.0);
        label(ui, tr("Host (% for any)"));
        ui.add(Input::new(&mut edit.host).width(width));
    }
    if !edit.role {
        ui.add_space(6.0);
        let password_label = if old.is_some() { tr("New password (leave empty to keep it)") } else { tr("Password") };
        label(ui, password_label);
        ui.add(Input::new(&mut edit.password).password().width(width));
    }
    let options: Vec<_> = admin::options(kind).iter().filter(|o| !(o.users_only && edit.role)).collect();
    if !options.is_empty() {
        ui.add_space(8.0);
        for o in options {
            let mut on = edit.options.iter().any(|k| k == o.key);
            if ui.add(Checkbox::new(&mut on).label(tr(o.label))).changed() {
                edit.options.retain(|k| k != o.key);
                if on {
                    edit.options.push(o.key.to_string());
                }
            }
        }
    }
}

fn grant_fields(ui: &mut Ui, kind: DbKind, scope: &mut usize, target: &mut Target, picked: &mut Vec<bool>, width: f32) {
    let scopes = admin::scopes(kind);
    label(ui, tr("On"));
    let labels: Vec<&str> = scopes.iter().map(|s| tr(s.label(kind))).collect();
    let mut idx = Some(*scope);
    if ui.add(Select::new(&mut idx, &labels).width(width)).changed()
        && let Some(i) = idx
    {
        *scope = i;
        target.scope = scopes[i];
        *picked = vec![false; admin::privileges(kind, target.scope).len()];
    }
    let (db, schema, name) = target.scope.fields(kind);
    let field = |ui: &mut Ui, text: &str, value: &mut String| {
        ui.add_space(6.0);
        label(ui, text);
        ui.add(Input::new(value).width(width));
    };
    if db {
        field(ui, tr("Database"), &mut target.database);
    }
    if schema {
        field(ui, tr("Schema"), &mut target.schema);
    }
    if name {
        let what = match target.scope {
            Scope::Sequence => tr("Sequence"),
            Scope::Function => tr("Function"),
            _ if kind == DbKind::MsSql => tr("Object"),
            _ => tr("Table or view"),
        };
        field(ui, what, &mut target.name);
    }
    ui.add_space(8.0);
    label(ui, tr("Privileges"));
    ui.horizontal_wrapped(|ui| {
        for (p, on) in admin::privileges(kind, target.scope).iter().zip(picked.iter_mut()) {
            ui.add(Checkbox::new(on).label(*p));
        }
    });
}

fn revoke_form(kind: DbKind, p: &Principal, g: &Grant) -> Form {
    let target = if g.target.is_empty() { g.object.clone() } else { g.target.clone() };
    Form::Confirm {
        title: trf(
            "Revoke {privilege} on {object} from {name}?",
            &[("privilege", &g.privilege), ("object", &g.object), ("name", &display_name(p))],
        ),
        description: String::new(),
        statements: vec![admin::revoke(kind, &[&g.privilege], &g.target, p, g.grantable)],
        button: tr("Revoke").into(),
        done: trf("{privilege} on {object} revoked", &[("privilege", &g.privilege), ("object", &target)]),
    }
}

/// `name`, or `user@host` for a MySQL account.
fn display_name(p: &Principal) -> String {
    p.id()
}

/// Runs alternative statements until one works.
async fn first_working(s: &Arc<dyn Connection>, db: Option<&str>, list: &[&str]) -> DbResult<dbman_core::QueryResult> {
    let mut last = None;
    for sql in list {
        match s.execute(db, sql, 100_000).await {
            Ok(StatementOutcome::Rows(r)) => return Ok(r),
            Ok(StatementOutcome::Done { .. }) => return Ok(dbman_core::QueryResult::default()),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| dbman_core::DbError::new("no statement lists users on this engine")))
}

/// Whether the connected user may manage users and roles (any error means no).
pub async fn may_manage(s: &Arc<dyn Connection>) -> bool {
    let Some(sql) = admin::check_sql(s.kind()) else { return false };
    matches!(s.execute(None, sql, 1).await, Ok(StatementOutcome::Rows(r)) if admin::parse_check(&r))
}
