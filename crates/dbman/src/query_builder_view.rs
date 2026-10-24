//! Visual query builder: tables dragged (or clicked) from the schema's list
//! onto a canvas, joined on their foreign keys or on columns dragged one onto
//! another, columns ticked for the result, then aggregates, filters and
//! sorting set below. The SQL is shown as it changes and can be run here or
//! opened in a console.

use dbman_core::pivot::Agg;
use dbman_core::query_builder::{Filter, JoinKind, Op, Query, Sort};
use dbman_core::{DbKind, ObjectKind, ObjectPath, QueryResult, StatementOutcome};
use egui::{Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2};
use shadcn_egui::{Button, ButtonSize, Checkbox, Column, DataTable, Input, Select, Theme, tabs, toast, toast_error};

use crate::events::{Event, TabEvent, TabId};
use crate::explorer::{Action, ConnState, Load};
use crate::i18n::{tr, trf};
use crate::sql_editor;
use crate::tabs::{TabCtx, error_alert, not_connected, toolbar_button};

const WIDTH: f32 = 220.0;
const HEADER: f32 = 28.0;
const ROW: f32 = 22.0;
/// Rows fetched when the query is run here without a limit of its own.
const PREVIEW_ROWS: usize = 200;

/// What a drag on the canvas moves.
enum Drag {
    Pan,
    Table(usize),
    /// From a column, to join it with the column it is dropped on.
    Link(usize, usize),
}

/// What is under the pointer on the canvas.
#[derive(Clone, Copy, PartialEq)]
enum Hit {
    Close(usize),
    Header(usize),
    Column(usize, usize),
    Join(usize),
}

pub struct QueryBuilderView {
    pub database: String,
    pub schema: String,
    kind: DbKind,
    query: Query,
    /// World position of each table's box.
    pos: Vec<Pos2>,
    offset: Vec2,
    /// Tables waiting for their structure, with where they go.
    pending: Vec<(ObjectPath, Pos2)>,
    drag: Option<Drag>,
    /// A table dragged from the list.
    list_drag: Option<ObjectPath>,
    canvas: Rect,
    search: String,
    limit: String,
    section: usize,
    result: Load<QueryResult>,
    /// The SQL the result is of.
    ran: String,
}

impl QueryBuilderView {
    pub fn new(kind: DbKind, database: String, schema: String) -> Self {
        Self {
            database,
            schema,
            kind,
            query: Query::default(),
            pos: Vec::new(),
            offset: vec2(16.0, 16.0),
            pending: Vec::new(),
            drag: None,
            list_drag: None,
            canvas: Rect::NOTHING,
            search: String::new(),
            limit: String::new(),
            section: 0,
            result: Load::Idle,
            ran: String::new(),
        }
    }

    pub fn title(&self) -> String {
        if self.schema == self.database || self.schema.is_empty() {
            trf("{name} (query builder)", &[("name", &self.database)])
        } else {
            trf("{name} (query builder)", &[("name", &format!("{}.{}", self.database, self.schema))])
        }
    }

    pub fn query(&self) -> &Query {
        &self.query
    }

    pub fn query_mut(&mut self) -> &mut Query {
        &mut self.query
    }

    pub fn sql(&self) -> String {
        self.query.sql(self.kind)
    }

    pub fn result(&self) -> Option<&QueryResult> {
        self.result.ready()
    }

    /// Adds a table to the canvas at `at` (world position), once its structure is read.
    pub fn add(&mut self, path: ObjectPath, at: Option<Pos2>) {
        let n = self.query.tables.len() + self.pending.len();
        let at = at.unwrap_or_else(|| pos2((n % 4) as f32 * (WIDTH + 60.0), (n / 4) as f32 * 260.0));
        self.pending.push((path, at));
    }

    pub fn on_event(&mut self, ev: TabEvent) {
        if let TabEvent::Builder(r) = ev {
            self.result = Load::from_result(r);
        }
    }

    fn box_rect(&self, i: usize) -> Rect {
        let rows = self.query.tables[i].details.columns.len();
        let size = vec2(WIDTH, HEADER + rows as f32 * ROW + 6.0);
        Rect::from_min_size(self.canvas.min + self.offset + self.pos[i].to_vec2(), size)
    }

    /// Where a column's row is on screen (tests click it).
    pub fn column_pos(&self, table: usize, column: &str) -> Pos2 {
        pos2(self.box_rect(table).center().x, self.column_y(table, column))
    }

    fn column_y(&self, i: usize, column: &str) -> f32 {
        let k = self.query.tables[i].details.columns.iter().position(|c| c.name == column).unwrap_or(0);
        self.box_rect(i).min.y + HEADER + (k as f32 + 0.5) * ROW
    }

    /// The two ends of a join's line and the middle, where its kind is shown.
    fn join_line(&self, j: usize) -> (Pos2, Pos2, Pos2) {
        let join = &self.query.joins[j];
        let (a, b) = (self.box_rect(join.left), self.box_rect(join.right));
        let (ca, cb) = join.on.first().map(|(x, y)| (x.as_str(), y.as_str())).unwrap_or_default();
        let (ya, yb) = (self.column_y(join.left, ca), self.column_y(join.right, cb));
        let (pa, pb) = if a.center().x <= b.center().x {
            (pos2(a.max.x, ya), pos2(b.min.x, yb))
        } else {
            (pos2(a.min.x, ya), pos2(b.max.x, yb))
        };
        (pa, pb, pa + (pb - pa) / 2.0)
    }

    fn hit(&self, p: Pos2) -> Option<Hit> {
        for j in 0..self.query.joins.len() {
            if Rect::from_center_size(self.join_line(j).2, vec2(48.0, 18.0)).contains(p) {
                return Some(Hit::Join(j));
            }
        }
        for i in (0..self.query.tables.len()).rev() {
            let r = self.box_rect(i);
            if !r.contains(p) {
                continue;
            }
            if p.y < r.min.y + HEADER {
                let close = Rect::from_center_size(pos2(r.max.x - 14.0, r.min.y + HEADER / 2.0), Vec2::splat(18.0));
                return Some(if close.contains(p) { Hit::Close(i) } else { Hit::Header(i) });
            }
            let k = ((p.y - r.min.y - HEADER) / ROW).floor() as usize;
            if k < self.query.tables[i].details.columns.len() {
                return Some(Hit::Column(i, k));
            }
            return Some(Hit::Header(i));
        }
        None
    }

    fn run(&mut self, cx: &TabCtx<'_>, tab: TabId) {
        let Some(session) = cx.conn.and_then(ConnState::session) else { return };
        let sql = self.sql();
        if sql.is_empty() {
            return;
        }
        self.result = Load::Loading;
        self.ran = sql.clone();
        self.section = 3;
        let db = self.database.clone();
        cx.tasks.spawn(async move {
            let result = session.execute(Some(&db), &sql, PREVIEW_ROWS).await.and_then(|o| match o {
                StatementOutcome::Rows(r) => Ok(r),
                other => Err(dbman_core::DbError::new(format!("unexpected result: {other:?}"))),
            });
            Event::Tab { tab, event: TabEvent::Builder(result) }
        });
    }

    pub fn ui(&mut self, ui: &mut Ui, tab: TabId, mut cx: TabCtx<'_>) {
        let Some(conn) = cx.conn.filter(|c| c.session().is_some()) else {
            not_connected(ui, &mut cx);
            return;
        };
        // Tables dropped or clicked come in once their structure is read.
        let key = (self.database.clone(), self.schema.clone());
        if !conn.objects.contains_key(&key) {
            cx.actions.push(Action::LoadObjects(cx.conn_id, key.0.clone(), key.1.clone()));
        }
        let mut waiting = Vec::new();
        for (path, at) in std::mem::take(&mut self.pending) {
            match conn.details.get(&path) {
                Some(Load::Ready(d)) => {
                    self.query.add_table(path, d.clone());
                    self.pos.push(at);
                }
                Some(Load::Failed(e)) => toast_error(ui.ctx(), tr("Could not read the table"), Some(e)),
                Some(_) => waiting.push((path, at)),
                None => {
                    cx.actions.push(Action::LoadDetails(cx.conn_id, path.clone()));
                    waiting.push((path, at));
                }
            }
        }
        self.pending = waiting;

        self.toolbar(ui, tab, &mut cx);
        ui.add_space(6.0);
        egui::Panel::left(egui::Id::new(("builder-tables", tab)))
            .resizable(true)
            .default_size(200.0)
            .min_size(140.0)
            .show(ui, |ui| self.table_list(ui, conn.objects.get(&key)));
        egui::Panel::bottom(egui::Id::new(("builder-bottom", tab)))
            .resizable(true)
            .default_size(300.0)
            .min_size(160.0)
            .show(ui, |ui| self.bottom(ui, tab));
        self.canvas_ui(ui);
    }

    fn toolbar(&mut self, ui: &mut Ui, tab: TabId, cx: &mut TabCtx<'_>) {
        let sql = self.sql();
        ui.horizontal(|ui| {
            let run = ui.add_enabled(!sql.is_empty(), Button::new(tr("▶ Run")).size(ButtonSize::Sm));
            if run.clicked() {
                self.run(cx, tab);
            }
            let open = ui.add_enabled(!sql.is_empty(), Button::outline(tr("Open in console")).size(ButtonSize::Sm));
            if open.clicked() {
                cx.actions.push(Action::OpenConsoleWith(cx.conn_id, Some(self.database.clone()), sql.clone()));
            }
            if ui.add_enabled(!sql.is_empty(), Button::outline(tr("Copy SQL")).size(ButtonSize::Sm)).clicked() {
                ui.ctx().copy_text(sql.clone());
                toast(ui.ctx(), tr("Copied"), None);
            }
            ui.add_space(8.0);
            ui.add(Checkbox::new(&mut self.query.distinct).label(tr("Distinct rows")));
            ui.add_space(8.0);
            ui.label(tr("Limit"));
            ui.add(Input::new(&mut self.limit).placeholder(tr("none")).width(70.0));
            self.query.limit = self.limit.trim().parse().ok();
            ui.add_space(8.0);
            if toolbar_button(ui, tr("Clear")).clicked() {
                self.query = Query::default();
                self.pos.clear();
                self.result = Load::Idle;
            }
            if !self.pending.is_empty() {
                ui.add(shadcn_egui::Spinner::new().size(12.0));
            }
        });
    }

    fn table_list(&mut self, ui: &mut Ui, objects: Option<&Load<Vec<dbman_core::DbObject>>>) {
        let t = Theme::get(ui.ctx());
        ui.add(Input::new(&mut self.search).placeholder(tr("Find table")).width(ui.available_width()));
        ui.label(RichText::new(tr("Drag a table onto the canvas, or click it.")).size(11.0).color(t.muted_foreground));
        ui.add_space(4.0);
        let list = match objects {
            Some(Load::Ready(o)) => o,
            Some(Load::Failed(e)) => return error_alert(ui, tr("Could not read the schema"), e),
            _ => {
                ui.add(shadcn_egui::Spinner::new());
                return;
            }
        };
        let q = self.search.trim().to_lowercase();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for o in list.iter().filter(|o| matches!(o.kind, ObjectKind::Table | ObjectKind::View)) {
                if !q.is_empty() && !o.path.name.to_lowercase().contains(&q) {
                    continue;
                }
                let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click_and_drag());
                if r.hovered() {
                    ui.painter().rect_filled(rect, 4.0, t.accent);
                }
                let icon =
                    if o.kind == ObjectKind::View { crate::icons::Icon::View } else { crate::icons::Icon::Table };
                let icon_rect = Rect::from_center_size(rect.left_center() + vec2(12.0, 0.0), Vec2::splat(14.0));
                crate::icons::paint(ui.painter(), icon_rect, icon, t.muted_foreground);
                ui.painter().text(
                    rect.left_center() + vec2(26.0, 0.0),
                    Align2::LEFT_CENTER,
                    &o.path.name,
                    FontId::proportional(13.0),
                    t.foreground,
                );
                let r = r.on_hover_cursor(egui::CursorIcon::Grab);
                r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &o.path.name));
                if r.clicked() {
                    self.add(o.path.clone(), None);
                }
                if r.drag_started() {
                    self.list_drag = Some(o.path.clone());
                }
            }
        });
        // The dragged table follows the pointer and lands where it is let go.
        if let Some(path) = &self.list_drag {
            let pointer = ui.ctx().pointer_interact_pos();
            if let Some(p) = pointer {
                egui::Area::new(egui::Id::new("builder-drag"))
                    .order(egui::Order::Tooltip)
                    .fixed_pos(p + vec2(8.0, 8.0))
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| ui.label(&path.name));
                    });
            }
            if ui.input(|i| i.pointer.any_released()) {
                let path = path.clone();
                self.list_drag = None;
                if let Some(p) = pointer.filter(|p| self.canvas.contains(*p)) {
                    let at = p - self.canvas.min - self.offset - vec2(WIDTH / 2.0, HEADER / 2.0);
                    self.add(path, Some(at.to_pos2()));
                }
            }
        }
    }

    fn canvas_ui(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        self.canvas = rect;
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, t.background);
        let step = 24.0;
        let start = rect.min + vec2(self.offset.x.rem_euclid(step), self.offset.y.rem_euclid(step));
        let mut y = start.y;
        while y < rect.max.y {
            let mut x = start.x;
            while x < rect.max.x {
                painter.circle_filled(pos2(x, y), 0.8, t.border);
                x += step;
            }
            y += step;
        }
        if self.query.tables.is_empty() && self.pending.is_empty() {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                tr("Drag tables here from the list on the left."),
                FontId::proportional(14.0),
                t.muted_foreground,
            );
        }

        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta);
            self.offset += scroll;
        }
        let pointer = response.interact_pointer_pos().or(response.hover_pos());
        if response.drag_started() {
            let origin = ui.input(|i| i.pointer.press_origin()).or(pointer);
            self.drag = match origin.and_then(|p| self.hit(p)) {
                Some(Hit::Header(i)) | Some(Hit::Close(i)) => Some(Drag::Table(i)),
                Some(Hit::Column(i, k)) => Some(Drag::Link(i, k)),
                _ => Some(Drag::Pan),
            };
        }
        if response.dragged() {
            let delta = response.drag_delta();
            match self.drag {
                Some(Drag::Table(i)) => self.pos[i] += delta,
                Some(Drag::Pan) => self.offset += delta,
                _ => {}
            }
        }
        if response.drag_stopped() {
            if let Some(Drag::Link(a, k)) = self.drag
                && let Some(Hit::Column(b, m)) = pointer.and_then(|p| self.hit(p))
                && a != b
            {
                let ca = self.query.tables[a].details.columns[k].name.clone();
                let cb = self.query.tables[b].details.columns[m].name.clone();
                self.query.link(a, &ca, b, &cb);
            }
            self.drag = None;
        }
        if response.clicked() {
            match pointer.and_then(|p| self.hit(p)) {
                Some(Hit::Close(i)) => {
                    self.query.remove_table(i);
                    self.pos.remove(i);
                }
                Some(Hit::Column(i, k)) => {
                    let c = self.query.tables[i].details.columns[k].name.clone();
                    self.query.toggle_output(i, &c);
                }
                Some(Hit::Join(j)) => {
                    let kinds = JoinKind::ALL;
                    let at = kinds.iter().position(|k| *k == self.query.joins[j].kind).unwrap_or(0);
                    self.query.joins[j].kind = kinds[(at + 1) % kinds.len()];
                }
                _ => {}
            }
        }
        let hovered = response.hover_pos().and_then(|p| self.hit(p));
        if let Some(h) = hovered {
            ui.ctx().set_cursor_icon(match h {
                Hit::Header(_) => egui::CursorIcon::Grab,
                _ => egui::CursorIcon::PointingHand,
            });
        }

        // Joins under the boxes.
        for j in 0..self.query.joins.len() {
            let (a, b, mid) = self.join_line(j);
            let lit = hovered == Some(Hit::Join(j));
            let stroke =
                Stroke::new(if lit { 2.0 } else { 1.5 }, t.primary.gamma_multiply(if lit { 1.0 } else { 0.7 }));
            let bend = ((b.x - a.x).abs() / 2.0).max(30.0) * if a.x <= b.x { 1.0 } else { -1.0 };
            painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                [a, a + vec2(bend, 0.0), b - vec2(bend, 0.0), b],
                false,
                Color32::TRANSPARENT,
                stroke,
            ));
            let label = match self.query.joins[j].kind {
                JoinKind::Inner => "INNER",
                JoinKind::Left => "LEFT",
                JoinKind::Right => "RIGHT",
                JoinKind::Full => "FULL",
            };
            let r = Rect::from_center_size(mid, vec2(48.0, 18.0));
            painter.rect_filled(r, 9.0, if lit { t.primary } else { t.card });
            painter.rect_stroke(r, 9.0, Stroke::new(1.0, t.primary), StrokeKind::Inside);
            painter.text(
                mid,
                Align2::CENTER_CENTER,
                label,
                FontId::proportional(10.0),
                if lit { t.primary_foreground } else { t.primary },
            );
        }
        if let Some(Drag::Link(i, k)) = self.drag
            && let Some(p) = pointer
        {
            let r = self.box_rect(i);
            let y = r.min.y + HEADER + (k as f32 + 0.5) * ROW;
            let from = if p.x >= r.center().x { pos2(r.max.x, y) } else { pos2(r.min.x, y) };
            painter.line_segment([from, p], Stroke::new(1.5, t.primary));
        }

        for i in 0..self.query.tables.len() {
            let r = self.box_rect(i);
            if !r.intersects(rect) {
                continue;
            }
            let table = &self.query.tables[i];
            painter.rect_filled(r, 6.0, t.card);
            let header = Rect::from_min_size(r.min, vec2(r.width(), HEADER));
            painter.rect_filled(header, egui::CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 }, t.muted);
            let border = if matches!(hovered, Some(Hit::Header(h)) if h == i) { t.primary } else { t.border };
            painter.rect_stroke(r, 6.0, Stroke::new(1.0, border), StrokeKind::Inside);
            let name = if table.alias == table.path.name {
                table.alias.clone()
            } else {
                format!("{} ({})", table.alias, table.path.name)
            };
            painter.text(
                header.left_center() + vec2(10.0, 0.0),
                Align2::LEFT_CENTER,
                name,
                FontId::proportional(13.0),
                t.foreground,
            );
            let close_color = if hovered == Some(Hit::Close(i)) { t.destructive } else { t.muted_foreground };
            painter.text(
                pos2(r.max.x - 14.0, header.center().y),
                Align2::CENTER_CENTER,
                "×",
                FontId::proportional(15.0),
                close_color,
            );
            for (k, c) in table.details.columns.iter().enumerate() {
                let row = Rect::from_min_size(pos2(r.min.x, r.min.y + HEADER + k as f32 * ROW), vec2(r.width(), ROW));
                if hovered == Some(Hit::Column(i, k)) {
                    painter.rect_filled(row.shrink2(vec2(2.0, 0.0)), 3.0, t.accent);
                }
                let on = self.query.is_output(i, &c.name);
                let tick = Rect::from_center_size(row.left_center() + vec2(14.0, 0.0), Vec2::splat(12.0));
                if on {
                    painter.rect_filled(tick, 3.0, t.primary);
                    let c = tick.center();
                    let mark = Stroke::new(1.6, t.primary_foreground);
                    painter.line_segment([c + vec2(-3.5, 0.0), c + vec2(-1.0, 2.5)], mark);
                    painter.line_segment([c + vec2(-1.0, 2.5), c + vec2(3.5, -2.5)], mark);
                } else {
                    painter.rect_stroke(tick, 3.0, Stroke::new(1.0, t.muted_foreground), StrokeKind::Inside);
                }
                let color = if c.primary_key { Color32::from_rgb(0xd9, 0x77, 0x06) } else { t.foreground };
                painter.text(
                    row.left_center() + vec2(28.0, 0.0),
                    Align2::LEFT_CENTER,
                    &c.name,
                    FontId::proportional(12.0),
                    color,
                );
                painter.text(
                    row.right_center() - vec2(10.0, 0.0),
                    Align2::RIGHT_CENTER,
                    &c.data_type,
                    FontId::monospace(10.5),
                    t.muted_foreground,
                );
            }
        }
        if let Some(Hit::Column(..)) = hovered
            && self.drag.is_none()
        {
            response.on_hover_text_at_pointer(tr(
                "Click to show the column in the result. Drag it onto a column of another table to join them.",
            ));
        } else if let Some(Hit::Join(j)) = hovered {
            let join = &self.query.joins[j];
            let (l, r) = (&self.query.tables[join.left].alias, &self.query.tables[join.right].alias);
            let on: Vec<String> = join.on.iter().map(|(a, b)| format!("{l}.{a} = {r}.{b}")).collect();
            response.on_hover_text_at_pointer(format!(
                "{}\n{}\n{}",
                tr(join.kind.label()),
                on.join("\n"),
                tr("Click to change the kind of join.")
            ));
        }
    }

    fn bottom(&mut self, ui: &mut Ui, tab: TabId) {
        ui.add_space(6.0);
        ui.columns(2, |cols| {
            let ui = &mut cols[0];
            let result = match &self.result {
                Load::Ready(r) => trf("Result ({n})", &[("n", &r.rows.len())]),
                _ => tr("Result").to_string(),
            };
            let labels = [tr("Columns"), tr("Filters"), tr("Joins"), result.as_str()];
            tabs(ui, &mut self.section, &labels);
            ui.add_space(6.0);
            egui::ScrollArea::both().id_salt(("builder-section", tab)).auto_shrink([false, false]).show(ui, |ui| {
                match self.section {
                    0 => self.columns_ui(ui),
                    1 => self.filters_ui(ui),
                    2 => self.joins_ui(ui),
                    _ => self.result_ui(ui, tab),
                }
            });

            let ui = &mut cols[1];
            ui.label(RichText::new("SQL").strong());
            let sql = self.sql();
            egui::ScrollArea::vertical().id_salt(("builder-sql", tab)).show(ui, |ui| {
                if sql.is_empty() {
                    let t = Theme::get(ui.ctx());
                    ui.label(RichText::new(tr("Add a table to start.")).color(t.muted_foreground));
                } else {
                    sql_editor::viewer(ui, egui::Id::new(("builder-sql-text", tab)), &sql, self.kind);
                }
            });
        });
    }

    fn column_names(&self) -> Vec<String> {
        self.query
            .tables
            .iter()
            .flat_map(|t| t.details.columns.iter().map(move |c| format!("{}.{}", t.alias, c.name)))
            .collect()
    }

    fn columns_ui(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        if self.query.outputs.is_empty() {
            ui.label(
                RichText::new(tr("All columns are shown. Tick columns in the tables to pick them."))
                    .color(t.muted_foreground),
            );
            return;
        }
        let aggs: Vec<&str> = std::iter::once(tr("(value)")).chain(Agg::ALL.iter().map(|a| tr(a.label()))).collect();
        let sorts = [tr("(unsorted)"), tr("Ascending"), tr("Descending")];
        let mut remove = None;
        let mut swap = None;
        let n = self.query.outputs.len();
        egui::Grid::new("builder-columns").num_columns(6).spacing([8.0, 6.0]).show(ui, |ui| {
            for (i, o) in self.query.outputs.iter_mut().enumerate() {
                ui.push_id(i, |ui| {
                    ui.label(format!("{}.{}", self.query.tables[o.table].alias, o.column));
                });
                ui.push_id(("agg", i), |ui| {
                    let mut a = Some(o.agg.and_then(|a| Agg::ALL.iter().position(|x| *x == a)).map_or(0, |p| p + 1));
                    ui.add(Select::new(&mut a, &aggs).width(140.0));
                    o.agg = a.and_then(|a| a.checked_sub(1)).map(|a| Agg::ALL[a]);
                });
                ui.push_id(("alias", i), |ui| {
                    ui.add(Input::new(&mut o.alias).placeholder(tr("Name in result")).width(130.0));
                });
                ui.push_id(("sort", i), |ui| {
                    let mut s = Some(match o.sort {
                        Sort::None => 0,
                        Sort::Asc => 1,
                        Sort::Desc => 2,
                    });
                    ui.add(Select::new(&mut s, &sorts).width(120.0));
                    o.sort = match s {
                        Some(1) => Sort::Asc,
                        Some(2) => Sort::Desc,
                        _ => Sort::None,
                    };
                });
                ui.horizontal(|ui| {
                    if ui.add_enabled(i > 0, Button::ghost("↑").size(ButtonSize::Icon)).clicked() {
                        swap = Some((i, i - 1));
                    }
                    if ui.add_enabled(i + 1 < n, Button::ghost("↓").size(ButtonSize::Icon)).clicked() {
                        swap = Some((i, i + 1));
                    }
                });
                if ui.add(Button::ghost("×").size(ButtonSize::Icon)).clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some((a, b)) = swap {
            self.query.outputs.swap(a, b);
        }
        if let Some(i) = remove {
            self.query.outputs.remove(i);
        }
    }

    fn filters_ui(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        let names = self.column_names();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        // Index of each table's first column in `names`.
        let starts: Vec<usize> = self
            .query
            .tables
            .iter()
            .scan(0, |n, t| {
                let s = *n;
                *n += t.details.columns.len();
                Some(s)
            })
            .collect();
        let ops: Vec<&str> = Op::ALL.iter().map(|o| o.label()).collect();
        let mut remove = None;
        if self.query.filters.is_empty() {
            ui.label(RichText::new(tr("No filters: every row is shown.")).color(t.muted_foreground));
        }
        let query = &mut self.query;
        egui::Grid::new("builder-filters").num_columns(4).spacing([8.0, 6.0]).show(ui, |ui| {
            for (i, f) in query.filters.iter_mut().enumerate() {
                let tables = &query.tables;
                ui.push_id(("col", i), |ui| {
                    let at = tables[f.table].details.columns.iter().position(|c| c.name == f.column).unwrap_or(0);
                    let mut c = Some(starts[f.table] + at);
                    ui.add(Select::new(&mut c, &refs).width(200.0));
                    if let Some(c) = c {
                        let table = starts.iter().rposition(|s| *s <= c).unwrap_or(0);
                        f.table = table;
                        f.column = tables[table].details.columns[c - starts[table]].name.clone();
                    }
                });
                ui.push_id(("op", i), |ui| {
                    let mut o = Op::ALL.iter().position(|o| *o == f.op);
                    ui.add(Select::new(&mut o, &ops).width(120.0));
                    if let Some(o) = o {
                        f.op = Op::ALL[o];
                    }
                });
                ui.push_id(("value", i), |ui| {
                    if f.op.takes_value() {
                        let hint = if f.op == Op::In { tr("a, b, c") } else { tr("Value") };
                        ui.add(Input::new(&mut f.value).placeholder(hint).width(180.0));
                    } else {
                        ui.label("");
                    }
                });
                if ui.add(Button::ghost("×").size(ButtonSize::Icon)).clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            self.query.filters.remove(i);
        }
        ui.add_space(4.0);
        let add =
            ui.add_enabled(!self.query.tables.is_empty(), Button::outline(tr("+ Add filter")).size(ButtonSize::Sm));
        if add.clicked()
            && let Some(c) = self.query.tables[0].details.columns.first()
        {
            let column = c.name.clone();
            self.query.filters.push(Filter { table: 0, column, op: Op::Eq, value: String::new() });
        }
    }

    fn joins_ui(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        if self.query.joins.is_empty() {
            ui.label(
                RichText::new(tr(
                    "No joins. Tables with foreign keys between them are joined when added; drag a column onto another table's column to join them by hand.",
                ))
                .color(t.muted_foreground),
            );
            return;
        }
        let kinds: Vec<&str> = JoinKind::ALL.iter().map(|k| tr(k.label())).collect();
        let mut remove = None;
        let query = &mut self.query;
        egui::Grid::new("builder-joins").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
            for (i, j) in query.joins.iter_mut().enumerate() {
                let (l, r) = (&query.tables[j.left].alias, &query.tables[j.right].alias);
                let on: Vec<String> = j.on.iter().map(|(a, b)| format!("{l}.{a} = {r}.{b}")).collect();
                ui.label(on.join(" AND "));
                ui.push_id(("kind", i), |ui| {
                    let mut k = JoinKind::ALL.iter().position(|k| *k == j.kind);
                    ui.add(Select::new(&mut k, &kinds).width(130.0));
                    if let Some(k) = k {
                        j.kind = JoinKind::ALL[k];
                    }
                });
                if ui.add(Button::ghost("×").size(ButtonSize::Icon)).clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            self.query.joins.remove(i);
        }
    }

    fn result_ui(&mut self, ui: &mut Ui, tab: TabId) {
        let t = Theme::get(ui.ctx());
        match &self.result {
            Load::Idle => {
                ui.label(RichText::new(tr("Run the query to see its rows here.")).color(t.muted_foreground));
            }
            Load::Loading => {
                ui.add(shadcn_egui::Spinner::new());
            }
            Load::Failed(e) => error_alert(ui, tr("The query failed"), e),
            Load::Ready(r) => {
                if self.ran != self.sql() {
                    ui.label(
                        RichText::new(tr("The query changed since it was run.")).size(12.0).color(t.muted_foreground),
                    );
                }
                let columns: Vec<Column> = r.columns.iter().map(|c| Column::new(c.name.clone())).collect();
                let rows: Vec<Vec<String>> =
                    r.rows.iter().map(|row| row.iter().map(|c| c.display().into_owned()).collect()).collect();
                DataTable::new(("builder-result", tab), columns).row_height(26.0).resizable().show(ui, &rows);
            }
        }
    }
}
