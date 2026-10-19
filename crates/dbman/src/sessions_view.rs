//! Sessions tab: who is connected to the server and what they run, with
//! buttons that stop a query or end a session.

use std::time::{Duration, Instant};

use dbman_core::sessions::{self, Session};
use dbman_core::{DbKind, DbResult, StatementOutcome};
use egui::{RichText, Ui};
use shadcn_egui::{Column, DataTable, Switch, Theme, alert_dialog, menu_item, menu_separator, toast, toast_error};

use crate::events::{Event, TabEvent, TabId};
use crate::explorer::{Action, ConnState, Load};
use crate::i18n::{tr, trf};
use crate::sql_editor;
use crate::tabs::{TabCtx, error_alert, not_connected, toolbar_button};

/// How often the list reloads while auto-refresh is on.
const REFRESH_EVERY: Duration = Duration::from_secs(3);

pub enum SessionsEvent {
    Listed(DbResult<Vec<Session>>),
    /// A cancel or kill ran: `true` for kill.
    Killed {
        terminate: bool,
        id: String,
        result: DbResult<()>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kill {
    Cancel,
    Terminate,
}

pub struct SessionsView {
    pub kind: DbKind,
    list: Load<Vec<Session>>,
    /// The rows drawn last; kept while a reload runs so the table does not blink.
    shown: Vec<Session>,
    version: u64,
    loaded_at: Option<Instant>,
    auto_refresh: bool,
    hide_idle: bool,
    /// The id of the session whose query is shown below the table.
    picked: Option<String>,
    /// The session to end once the user confirms.
    confirm: Option<Session>,
}

impl SessionsView {
    pub fn new(kind: DbKind) -> Self {
        Self {
            kind,
            list: Load::Idle,
            shown: Vec::new(),
            version: 0,
            loaded_at: None,
            auto_refresh: true,
            hide_idle: false,
            picked: None,
            confirm: None,
        }
    }

    /// The sessions listed last.
    pub fn sessions(&self) -> &[Session] {
        &self.shown
    }

    /// Shows the query and the buttons of a session under the table, as a click on its row does.
    pub fn pick(&mut self, id: &str) {
        self.picked = Some(id.to_string());
    }

    pub fn title(&self) -> String {
        tr("Sessions").into()
    }

    pub fn on_event(&mut self, ev: TabEvent, ctx: &egui::Context) {
        let TabEvent::Sessions(ev) = ev else { return };
        match *ev {
            SessionsEvent::Listed(result) => {
                if let Ok(rows) = &result {
                    self.shown = rows.clone();
                    self.version += 1;
                }
                self.list = Load::from_result(result);
                self.loaded_at = Some(Instant::now());
            }
            SessionsEvent::Killed { terminate, id, result } => {
                match result {
                    Ok(()) if terminate => toast(ctx, trf("Session {id} was ended", &[("id", &id)]), None),
                    Ok(()) => toast(ctx, trf("The query of session {id} was cancelled", &[("id", &id)]), None),
                    Err(e) => toast_error(ctx, tr("Could not stop the session"), Some(&e.to_string())),
                }
                self.list = Load::Idle;
            }
        }
    }

    fn load(&mut self, cx: &TabCtx<'_>, tab: TabId, session: &std::sync::Arc<dyn dbman_core::Connection>) {
        let Some(sql) = sessions::list_sql(self.kind) else { return };
        self.list = Load::Loading;
        let s = session.clone();
        cx.tasks.spawn(async move {
            let result = s.execute(None, sql, 10_000).await.map(|o| match o {
                StatementOutcome::Rows(r) => sessions::parse(&r),
                StatementOutcome::Done { .. } => Vec::new(),
            });
            Event::Tab { tab, event: TabEvent::Sessions(Box::new(SessionsEvent::Listed(result))) }
        });
    }

    fn kill(
        &self,
        cx: &TabCtx<'_>,
        tab: TabId,
        session: &std::sync::Arc<dyn dbman_core::Connection>,
        how: Kill,
        id: &str,
    ) {
        let sql = match how {
            Kill::Cancel => sessions::cancel_sql(self.kind, id),
            Kill::Terminate => sessions::terminate_sql(self.kind, id),
        };
        let Some(sql) = sql else { return };
        let (s, id) = (session.clone(), id.to_string());
        cx.tasks.spawn(async move {
            let result = s.execute(None, &sql, 1).await.map(drop);
            let ev = SessionsEvent::Killed { terminate: how == Kill::Terminate, id, result };
            Event::Tab { tab, event: TabEvent::Sessions(Box::new(ev)) }
        });
    }

    pub fn ui(&mut self, ui: &mut Ui, tab: TabId, mut cx: TabCtx<'_>) {
        let Some(session) = cx.conn.and_then(ConnState::session) else {
            not_connected(ui, &mut cx);
            return;
        };
        let due = self.auto_refresh && self.loaded_at.is_some_and(|t| t.elapsed() >= REFRESH_EVERY);
        if self.list.is_idle() || (due && !matches!(self.list, Load::Loading)) {
            self.load(&cx, tab, &session);
        }
        if self.auto_refresh {
            ui.ctx().request_repaint_after(REFRESH_EVERY);
        }
        let t = Theme::get(ui.ctx());

        let active = self.shown.iter().filter(|s| !s.is_idle()).count();
        ui.horizontal(|ui| {
            if toolbar_button(ui, tr("⟳ Refresh")).clicked() {
                self.load(&cx, tab, &session);
            }
            ui.add(Switch::new(&mut self.auto_refresh).label(tr("Auto-refresh")));
            if ui.add(Switch::new(&mut self.hide_idle).label(tr("Hide idle"))).changed() {
                self.version += 1;
            }
            ui.add_space(8.0);
            ui.label(
                RichText::new(trf("{n} sessions, {active} active", &[("n", &self.shown.len()), ("active", &active)]))
                    .color(t.muted_foreground),
            );
            if matches!(self.list, Load::Loading) {
                ui.add(shadcn_egui::Spinner::new().size(12.0));
            }
        });
        ui.add_space(8.0);
        if let Load::Failed(e) = &self.list {
            error_alert(ui, tr("Could not list the sessions"), e);
            if self.shown.is_empty() {
                return;
            }
        }

        let hide_idle = self.hide_idle;
        let rows: Vec<&Session> = self.shown.iter().filter(|s| !hide_idle || !s.is_idle()).collect();
        let texts: Vec<Vec<String>> = rows
            .iter()
            .map(|s| {
                vec![
                    s.id.clone(),
                    s.user.clone(),
                    s.database.clone(),
                    s.client.clone(),
                    s.application.clone(),
                    s.state.clone(),
                    s.seconds.map(sessions::duration_label).unwrap_or_default(),
                    s.wait.clone(),
                    one_line(&s.query),
                ]
            })
            .collect();
        let columns = vec![
            Column::new(tr("ID")).sortable().weight(0.8),
            Column::new(tr("User")).sortable().weight(0.8),
            Column::new(tr("Database")).sortable().weight(0.8),
            Column::new(tr("Client")).sortable(),
            Column::new(tr("Application")).sortable().weight(0.8),
            Column::new(tr("State")).sortable().weight(0.8),
            Column::new(tr("Time")).weight(0.6),
            Column::new(tr("Waiting for")).weight(0.8),
            Column::new(tr("Query")).weight(2.5),
        ];
        let kind = self.kind;
        let mut wanted: Option<(Kill, Session)> = None;
        let mut picked = self.picked.clone();
        let detail_height = if self.picked.is_some() { 170.0 } else { 0.0 };
        let resp = DataTable::new(("sessions", tab), columns)
            .filter(tr("Filter sessions…"))
            .row_height(30.0)
            .resizable()
            .version(self.version)
            .max_height((ui.available_height() - 70.0 - detail_height).max(120.0))
            .cell_ui(|ui, row, col| {
                let s = rows[row];
                match col {
                    0 if s.own => {
                        ui.label(&s.id);
                        ui.label(RichText::new(tr("this")).size(12.0).color(t.muted_foreground));
                        true
                    }
                    5 => {
                        let color = if s.is_idle() { t.muted_foreground } else { t.foreground };
                        ui.label(RichText::new(&s.state).color(color));
                        true
                    }
                    _ => false,
                }
            })
            .row_context_menu(|ui, row| {
                let s = rows[row];
                ui.set_min_width(200.0);
                if !s.own && sessions::can_cancel(kind) && menu_item(ui, tr("Cancel query"), None).clicked() {
                    wanted = Some((Kill::Cancel, s.clone()));
                }
                if !s.own && sessions::can_terminate(kind) && menu_item(ui, tr("Kill session…"), None).clicked() {
                    wanted = Some((Kill::Terminate, s.clone()));
                }
                menu_separator(ui);
                if menu_item(ui, tr("Copy query"), None).clicked() {
                    ui.ctx().copy_text(s.query.clone());
                }
                if menu_item(ui, tr("Open query in a console"), None).clicked() {
                    cx.actions.push(Action::OpenConsoleWith(cx.conn_id, None, s.query.clone()));
                }
            })
            .show(ui, &texts);
        if let Some(i) = resp.clicked {
            picked = rows.get(i).map(|s| s.id.clone());
        }

        let detail = picked.as_deref().and_then(|id| self.shown.iter().find(|s| s.id == id)).cloned();
        if let Some(s) = &detail {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(trf("Session {id}", &[("id", &s.id)])).strong().color(t.foreground));
                if !s.own && sessions::can_cancel(kind) && toolbar_button(ui, tr("Cancel query")).clicked() {
                    wanted = Some((Kill::Cancel, s.clone()));
                }
                if !s.own && sessions::can_terminate(kind) && toolbar_button(ui, tr("Kill session…")).clicked() {
                    wanted = Some((Kill::Terminate, s.clone()));
                }
                if toolbar_button(ui, tr("Copy query")).clicked() {
                    ui.ctx().copy_text(s.query.clone());
                }
                if toolbar_button(ui, tr("Open query in a console")).clicked() {
                    cx.actions.push(Action::OpenConsoleWith(cx.conn_id, None, s.query.clone()));
                }
                if toolbar_button(ui, "×").clicked() {
                    picked = None;
                }
            });
            ui.add_space(4.0);
            egui::ScrollArea::vertical().id_salt(("session-query", tab)).max_height(130.0).show(ui, |ui| {
                sql_editor::viewer(ui, egui::Id::new(("session-query-view", tab)), &s.query, kind);
            });
        } else {
            picked = None;
        }
        self.picked = picked;

        match wanted {
            Some((Kill::Cancel, s)) => self.kill(&cx, tab, &session, Kill::Cancel, &s.id),
            Some((Kill::Terminate, s)) => self.confirm = Some(s),
            None => {}
        }
        if let Some(s) = &self.confirm {
            let title = trf("End session {id}?", &[("id", &s.id)]);
            let description = trf(
                "{user} is disconnected from the server, and its open transaction rolls back.",
                &[("user", &s.user)],
            );
            match alert_dialog(ui.ctx(), ("kill-session", tab), &title, &description, tr("Kill session")) {
                Some(true) => {
                    let id = s.id.clone();
                    self.confirm = None;
                    self.kill(&cx, tab, &session, Kill::Terminate, &id);
                }
                Some(false) => self.confirm = None,
                None => {}
            }
        }
    }
}

/// The query on one line for the table.
fn one_line(q: &str) -> String {
    let mut s: String = q.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > 300 {
        s = s.chars().take(300).collect::<String>() + "…";
    }
    s
}
