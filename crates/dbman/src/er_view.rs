//! ER diagram tab: the tables of a schema as boxes with their columns, and
//! foreign keys as lines between the key columns. Boxes can be dragged, the
//! canvas panned (drag the background) and zoomed (Ctrl + wheel or pinch).
//! Exports to SVG, Mermaid and Graphviz DOT.

use std::sync::Arc;

use dbman_core::er::{self, Cardinality, Diagram, geometry};
use dbman_core::{Connection, ConnectionConfig, DbKind, DbObject, DbResult, ObjectKind, ObjectPath, TableDetails};
use egui::epaint::CubicBezierShape;
use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2};
use shadcn_egui::{
    Button, ButtonSize, Checkbox, Input, Theme, TooltipExt, dropdown_menu, menu_item, menu_label, menu_separator, toast,
};
use tokio::sync::OnceCell;

use crate::events::{Event, TabEvent, TabId};
use crate::explorer::{Action, Load};
use crate::files;
use crate::i18n::{tr, trf};
use crate::tabs::{TabCtx, error_alert, toolbar_button};
use crate::toolbar::{Side, toolbar};

pub struct ErView {
    pub database: String,
    pub schema: String,
    cfg: ConnectionConfig,
    session: Arc<OnceCell<Arc<dyn Connection>>>,
    tables: Load<Vec<(ObjectPath, TableDetails)>>,
    progress: Option<(usize, usize)>,
    /// The diagram as drawn (all columns or keys only).
    diagram: Diagram,
    sizes: Vec<Vec2>,
    pos: Vec<Pos2>,
    laid_out: bool,
    keys_only: bool,
    offset: Vec2,
    zoom: f32,
    fit: bool,
    selected: Option<usize>,
    dragging: Option<usize>,
    search: String,
}

impl ErView {
    pub fn new(cfg: ConnectionConfig, database: String, schema: String) -> Self {
        Self {
            database,
            schema,
            cfg,
            session: Arc::new(OnceCell::new()),
            tables: Load::Idle,
            progress: None,
            diagram: Diagram::default(),
            sizes: Vec::new(),
            pos: Vec::new(),
            laid_out: false,
            keys_only: false,
            offset: Vec2::ZERO,
            zoom: 1.0,
            fit: true,
            selected: None,
            dragging: None,
            search: String::new(),
        }
    }

    pub fn title(&self) -> String {
        if self.schema == self.database || self.schema.is_empty() {
            trf("{name} (diagram)", &[("name", &self.database)])
        } else {
            trf("{name} (diagram)", &[("name", &format!("{}.{}", self.database, self.schema))])
        }
    }

    /// The diagram once loaded (tests).
    pub fn diagram(&self) -> Option<&Diagram> {
        self.tables.ready().map(|_| &self.diagram)
    }

    pub fn on_event(&mut self, ev: TabEvent) {
        match ev {
            TabEvent::DiagramProgress { done, total } => self.progress = Some((done, total)),
            TabEvent::Diagram(r) => {
                self.progress = None;
                self.tables = Load::from_result(r);
                self.rebuild();
            }
            _ => {}
        }
    }

    fn rebuild(&mut self) {
        let Some(tables) = self.tables.ready() else { return };
        let mut d = Diagram::build(tables);
        if self.keys_only {
            for e in &mut d.entities {
                e.columns.retain(|c| c.primary_key || c.foreign_key);
            }
        }
        self.diagram = d;
        self.laid_out = false;
        self.fit = true;
        self.selected = None;
    }

    fn load(&mut self, cx: &TabCtx<'_>, tab: TabId) {
        self.tables = Load::Loading;
        self.progress = None;
        let own = self.session.clone();
        let mut cfg = self.cfg.clone();
        if cfg.kind == DbKind::MySql {
            cfg.database = self.database.clone();
        }
        let (db, schema) = (self.database.clone(), self.schema.clone());
        let tx = cx.tasks.sender();
        cx.tasks.spawn(async move {
            let result: DbResult<Vec<(ObjectPath, TableDetails)>> = async {
                let s = own.get_or_try_init(|| dbman_drivers::connect(&cfg)).await?;
                let objects = s.objects(&db, &schema).await?;
                let tables: Vec<DbObject> = objects.into_iter().filter(|o| o.kind == ObjectKind::Table).collect();
                let total = tables.len();
                let mut out = Vec::with_capacity(total);
                for (i, t) in tables.into_iter().enumerate() {
                    tx.send(Event::Tab { tab, event: TabEvent::DiagramProgress { done: i, total } });
                    let details = s.table_details(&t.path).await?;
                    out.push((t.path, details));
                }
                Ok(out)
            }
            .await;
            Event::Tab { tab, event: TabEvent::Diagram(result) }
        });
    }

    /// Box sizes from the real fonts, then the automatic layout.
    fn lay_out(&mut self, ui: &Ui) {
        let measure = |text: &str, font: FontId| {
            ui.fonts_mut(|f| f.layout_no_wrap(text.to_string(), font, Color32::WHITE).size().x)
        };
        self.sizes = self
            .diagram
            .entities
            .iter()
            .map(|e| {
                let name = measure(&e.path.name, FontId::proportional(13.0)) + 8.0;
                let cols = e
                    .columns
                    .iter()
                    .map(|c| {
                        measure(&c.name, FontId::proportional(12.0))
                            + measure(&c.data_type, FontId::monospace(11.0))
                            + 34.0
                    })
                    .fold(0.0, f32::max);
                vec2((name.max(cols) + geometry::PAD * 2.0).clamp(140.0, 420.0), geometry::height(e.columns.len()))
            })
            .collect();
        let sizes: Vec<(f32, f32)> = self.sizes.iter().map(|s| (s.x, s.y)).collect();
        self.pos = er::layout(&self.diagram, &sizes).into_iter().map(|(x, y)| pos2(x, y)).collect();
        self.laid_out = true;
    }

    fn bounds(&self) -> Rect {
        let mut r = Rect::NOTHING;
        for (p, s) in self.pos.iter().zip(&self.sizes) {
            r = r.union(Rect::from_min_size(*p, *s));
        }
        r
    }

    fn fit_to(&mut self, view: Rect) {
        let b = self.bounds();
        if !b.is_positive() {
            return;
        }
        let margin = 24.0;
        let zx = (view.width() - margin * 2.0) / b.width();
        let zy = (view.height() - margin * 2.0) / b.height();
        self.zoom = zx.min(zy).clamp(0.1, 1.0);
        let content = b.size() * self.zoom;
        self.offset = (view.size() - content) / 2.0 - b.min.to_vec2() * self.zoom;
        // Big diagrams start at the top rather than in the middle.
        if content.y > view.height() - margin * 2.0 - 1.0 {
            self.offset.y = margin - b.min.y * self.zoom;
        }
    }

    fn center_on(&mut self, i: usize, view: Rect) {
        let c = Rect::from_min_size(self.pos[i], self.sizes[i]).center();
        self.offset = view.size() / 2.0 - c.to_vec2() * self.zoom;
    }

    pub fn ui(&mut self, ui: &mut Ui, tab: TabId, cx: TabCtx<'_>) {
        if self.tables.is_idle() {
            self.load(&cx, tab);
        }
        let mut find = false;
        toolbar(ui, ("er-toolbar", tab), |ui, side| match side {
            Side::Left => {
                if toolbar_button(ui, tr("⟳ Refresh")).clicked() {
                    self.load(&cx, tab);
                }
                if toolbar_button(ui, tr("Fit")).tooltip(tr("Show the whole diagram")).clicked() {
                    self.fit = true;
                }
                if toolbar_button(ui, "−").clicked() {
                    self.zoom = (self.zoom / 1.25).max(0.1);
                }
                ui.label(egui::RichText::new(format!("{:.0}%", self.zoom * 100.0)).size(12.0));
                if toolbar_button(ui, "+").clicked() {
                    self.zoom = (self.zoom * 1.25).min(3.0);
                }
                ui.add_space(8.0);
                let before = self.keys_only;
                ui.add(Checkbox::new(&mut self.keys_only).label(tr("Keys only")));
                if before != self.keys_only {
                    self.rebuild();
                }
                ui.add_space(8.0);
                let r = ui.add(Input::new(&mut self.search).placeholder(tr("Find table")).width(180.0));
                find = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) || r.changed();
                ui.add_space(8.0);
                self.export_menu(ui);
            }
            Side::Right => {
                let t = Theme::get(ui.ctx());
                let text = match &self.tables {
                    Load::Ready(_) => trf(
                        "{tables} tables · {links} relationships",
                        &[
                            ("tables", &self.diagram.entities.iter().filter(|e| !e.external).count()),
                            ("links", &self.diagram.links.len()),
                        ],
                    ),
                    _ => match self.progress {
                        Some((done, total)) => {
                            trf("Reading tables {done}/{total}…", &[("done", &done), ("total", &total)])
                        }
                        None => tr("Reading tables…").into(),
                    },
                };
                ui.label(egui::RichText::new(text).color(t.muted_foreground));
            }
        });
        ui.add_space(6.0);
        match &self.tables {
            Load::Ready(_) => {}
            Load::Failed(e) => return error_alert(ui, tr("Could not read the schema"), e),
            _ => {
                ui.add(shadcn_egui::Spinner::new());
                return;
            }
        }
        if self.diagram.entities.is_empty() {
            let t = Theme::get(ui.ctx());
            ui.label(egui::RichText::new(tr("No tables in this schema.")).color(t.muted_foreground));
            return;
        }
        if !self.laid_out {
            self.lay_out(ui);
        }
        self.canvas(ui, cx, find);
    }

    fn export_menu(&self, ui: &mut Ui) {
        let trigger = ui.add_enabled(self.tables.ready().is_some(), Button::outline(tr("Export")).size(ButtonSize::Sm));
        let d = self.diagram.clone();
        let base = if self.schema == self.database || self.schema.is_empty() {
            self.database.clone()
        } else {
            format!("{}_{}", self.database, self.schema)
        };
        dropdown_menu(&trigger, 220.0, |ui| {
            menu_label(ui, tr("Copy to clipboard"));
            if menu_item(ui, "Mermaid", None).clicked() {
                ui.ctx().copy_text(er::mermaid(&d));
                toast(ui.ctx(), tr("Copied the diagram as Mermaid"), None);
            }
            if menu_item(ui, "Graphviz DOT", None).clicked() {
                ui.ctx().copy_text(er::dot(&d));
                toast(ui.ctx(), tr("Copied the diagram as DOT"), None);
            }
            menu_separator(ui);
            menu_label(ui, tr("Save to file"));
            let save = |ui: &mut Ui, label: &str, filter: &str, ext: &'static str, render: fn(&Diagram) -> String| {
                if menu_item(ui, label, None).clicked() {
                    let dialog =
                        rfd::AsyncFileDialog::new().set_file_name(format!("{base}.{ext}")).add_filter(filter, &[ext]);
                    let d = d.clone();
                    files::save_file(ui.ctx(), dialog, move || render(&d));
                }
            };
            save(ui, tr("SVG image…"), "SVG image", "svg", er::svg);
            save(ui, "Mermaid…", "Mermaid", "mmd", er::mermaid);
            save(ui, "Graphviz DOT…", "Graphviz DOT", "dot", er::dot);
        });
    }

    fn canvas(&mut self, ui: &mut Ui, cx: TabCtx<'_>, find: bool) {
        let t = Theme::get(ui.ctx());
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        if self.fit {
            self.fit_to(rect);
            self.fit = false;
        }
        if find && !self.search.trim().is_empty() {
            let q = self.search.trim().to_lowercase();
            let hit = self
                .diagram
                .entities
                .iter()
                .position(|e| e.path.name.to_lowercase() == q)
                .or_else(|| self.diagram.entities.iter().position(|e| e.path.name.to_lowercase().contains(&q)));
            if let Some(i) = hit {
                self.selected = Some(i);
                self.center_on(i, rect);
            }
        }
        let to_screen = |p: Pos2, offset: Vec2, zoom: f32| rect.min + offset + p.to_vec2() * zoom;
        let hit_box = |p: Pos2, s: &Self| -> Option<usize> {
            (0..s.pos.len())
                .rev()
                .find(|&i| Rect::from_min_size(to_screen(s.pos[i], s.offset, s.zoom), s.sizes[i] * s.zoom).contains(p))
        };

        // Zoom around the pointer; plain wheel scrolls.
        if response.hovered() {
            let (zoom_delta, scroll) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta));
            if zoom_delta != 1.0
                && let Some(p) = response.hover_pos()
            {
                let new = (self.zoom * zoom_delta).clamp(0.1, 3.0);
                let world = (p - rect.min - self.offset) / self.zoom;
                self.offset = p - rect.min - world * new;
                self.zoom = new;
            } else if scroll != Vec2::ZERO {
                self.offset += scroll;
            }
        }
        if response.drag_started()
            && let Some(p) = response.interact_pointer_pos()
        {
            self.dragging = hit_box(p, self);
            if self.dragging.is_some() {
                self.selected = self.dragging;
            }
        }
        if response.dragged() {
            let delta = response.drag_delta();
            match self.dragging {
                Some(i) => self.pos[i] += delta / self.zoom,
                None => self.offset += delta,
            }
        }
        if response.drag_stopped() {
            self.dragging = None;
        }
        if response.clicked()
            && let Some(p) = response.interact_pointer_pos()
        {
            self.selected = hit_box(p, self);
        }
        if response.double_clicked()
            && let Some(p) = response.interact_pointer_pos()
            && let Some(i) = hit_box(p, self)
            && !self.diagram.entities[i].external
        {
            let path = self.diagram.entities[i].path.clone();
            let object = DbObject { kind: ObjectKind::Table, path, detail: None, comment: None };
            cx.actions.push(Action::OpenData(cx.conn_id, object));
        }
        let hovered = response.hover_pos().and_then(|p| hit_box(p, self));
        if hovered.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, t.background);
        // Dot grid.
        let step = 24.0 * self.zoom;
        if step >= 8.0 {
            let dot = t.border;
            let start = rect.min + vec2(self.offset.x.rem_euclid(step), self.offset.y.rem_euclid(step));
            let mut y = start.y;
            while y < rect.max.y {
                let mut x = start.x;
                while x < rect.max.x {
                    painter.circle_filled(pos2(x, y), 0.8, dot);
                    x += step;
                }
                y += step;
            }
        }

        let z = self.zoom;
        let focus = self.selected.or(hovered);
        let sizes: Vec<(f32, f32)> = self.sizes.iter().map(|s| (s.x, s.y)).collect();
        let world: Vec<(f32, f32)> = self.pos.iter().map(|p| (p.x, p.y)).collect();
        for l in &self.diagram.links {
            let lit = focus.is_some_and(|f| l.from == f || l.to == f);
            let color = if lit { t.primary } else { t.muted_foreground.gamma_multiply(0.7) };
            let stroke = Stroke::new(if lit { 2.0 } else { 1.2 }, color);
            let ((x1, y1, s1), (x2, y2, s2)) = er::link_ends(&self.diagram, l, &world, &sizes);
            let a = to_screen(pos2(x1, y1), self.offset, z);
            let b = to_screen(pos2(x2, y2), self.offset, z);
            let bend = ((b.x - a.x).abs() / 2.0).max(40.0 * z);
            let curve = CubicBezierShape::from_points_stroke(
                [a, a + vec2(s1 * bend, 0.0), b + vec2(s2 * bend, 0.0), b],
                false,
                Color32::TRANSPARENT,
                stroke,
            );
            if l.optional {
                // Dashed: draw the flattened curve in pieces.
                let pts = curve.flatten(Some(0.5));
                for seg in pts.windows(2).step_by(2) {
                    painter.line_segment([seg[0], seg[1]], stroke);
                }
            } else {
                painter.add(curve);
            }
            if l.cardinality == Cardinality::OneToMany {
                let f = a + vec2(s1 * 10.0 * z, 0.0);
                painter.line_segment([f, a + vec2(0.0, -5.0 * z)], stroke);
                painter.line_segment([f, a + vec2(0.0, 5.0 * z)], stroke);
            }
            let bx = b + vec2(s2 * 6.0 * z, 0.0);
            painter.line_segment([bx + vec2(0.0, -5.0 * z), bx + vec2(0.0, 5.0 * z)], stroke);
        }

        let related: Vec<usize> = focus.map(|f| self.diagram.neighbors(f)).unwrap_or_default();
        for (i, e) in self.diagram.entities.iter().enumerate() {
            let r = Rect::from_min_size(to_screen(self.pos[i], self.offset, z), self.sizes[i] * z);
            if !r.intersects(rect) {
                continue;
            }
            let selected = self.selected == Some(i);
            let border = if selected {
                Stroke::new(2.0, t.primary)
            } else if related.contains(&i) || hovered == Some(i) {
                Stroke::new(1.5, t.primary.gamma_multiply(0.6))
            } else {
                Stroke::new(1.0, t.border)
            };
            let radius = 6.0 * z;
            painter.rect_filled(r, radius, t.card);
            let header = Rect::from_min_size(r.min, vec2(r.width(), geometry::HEADER * z));
            painter.rect_filled(
                header,
                egui::CornerRadius { nw: radius as u8, ne: radius as u8, sw: 0, se: 0 },
                t.muted,
            );
            if e.external {
                // Dashed outline for a table of another schema.
                let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
                for w in pts.windows(2) {
                    let (p, q) = (w[0], w[1]);
                    let n = ((q - p).length() / (8.0 * z)).max(1.0) as usize;
                    for k in (0..n).step_by(2) {
                        let s = p + (q - p) * (k as f32 / n as f32);
                        let e = p + (q - p) * (((k + 1) as f32 / n as f32).min(1.0));
                        painter.line_segment([s, e], Stroke::new(1.0, t.muted_foreground));
                    }
                }
            } else {
                painter.rect_stroke(r, radius, border, StrokeKind::Inside);
            }
            if z < 0.35 {
                continue;
            }
            let name = if e.external { format!("{}.{}", e.path.schema, e.path.name) } else { e.path.name.clone() };
            painter.text(
                header.left_center() + vec2(geometry::PAD * z, 0.0),
                Align2::LEFT_CENTER,
                name,
                FontId::proportional(13.0 * z),
                t.foreground,
            );
            for (k, c) in e.columns.iter().enumerate() {
                let y = r.min.y + geometry::row_y(Some(k)) * z;
                let dot = if c.primary_key {
                    Color32::from_rgb(0xd9, 0x77, 0x06)
                } else if c.foreign_key {
                    Color32::from_rgb(0x25, 0x63, 0xeb)
                } else {
                    t.border
                };
                painter.circle_filled(pos2(r.min.x + (geometry::PAD + 3.0) * z, y), 3.0 * z, dot);
                let color =
                    if (c.nullable || e.external) && !c.primary_key { t.muted_foreground } else { t.foreground };
                painter.text(
                    pos2(r.min.x + (geometry::PAD + 12.0) * z, y),
                    Align2::LEFT_CENTER,
                    &c.name,
                    FontId::proportional(12.0 * z),
                    color,
                );
                painter.text(
                    pos2(r.max.x - geometry::PAD * z, y),
                    Align2::RIGHT_CENTER,
                    &c.data_type,
                    FontId::monospace(11.0 * z),
                    t.muted_foreground,
                );
            }
        }
        if let Some(i) = hovered
            && self.dragging.is_none()
        {
            let e = &self.diagram.entities[i];
            let mut lines = vec![format!("{}.{}", e.path.schema, e.path.name)];
            if let Some(c) = &e.comment {
                lines.push(c.clone());
            }
            for l in self.diagram.links.iter().filter(|l| l.from == i) {
                lines.push(format!(
                    "{} → {} ({})",
                    l.from_columns.join(", "),
                    self.diagram.entities[l.to].path.name,
                    l.to_columns.join(", ")
                ));
            }
            if !e.external {
                lines.push(tr("Double-click to open the data").into());
            }
            response.on_hover_text_at_pointer(lines.join("\n"));
        }
    }
}
