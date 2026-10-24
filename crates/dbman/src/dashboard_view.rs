//! Dashboard tab of a server: its connections, throughput and long-running
//! queries read every few seconds and drawn as they change, the size of the
//! database and its largest tables.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use dbman_core::chart::{ChartKind, ChartSpec};
use dbman_core::dashboard::{self, Sample};
use dbman_core::{Cell, ColumnMeta, DbKind, DbResult, Product, QueryResult, StatementOutcome};
use egui::{RichText, Ui};
use shadcn_egui::{Column, DataTable, Select, Switch, Theme};

use crate::chart_view;
use crate::events::{Event, TabEvent, TabId};
use crate::explorer::{ConnState, Load};
use crate::i18n::{tr, trf};
use crate::tabs::{TabCtx, error_alert, not_connected, toolbar_button};

/// Readings kept: ten minutes at the shortest interval.
const KEEP: usize = 600;
const INTERVALS: [u64; 4] = [1, 2, 5, 10];
/// How often the largest tables are read again.
const TABLES_EVERY: Duration = Duration::from_secs(60);

pub enum DashboardEvent {
    Sample(DbResult<Sample>),
    Tables(DbResult<QueryResult>),
}

/// Reads one value of a reading.
type Pick = fn(&Point) -> Option<f64>;

struct Point {
    /// Wall-clock time of the reading, `12:30:05`.
    at: String,
    taken: Instant,
    sample: Sample,
    /// Throughput since the reading before.
    rate: Option<f64>,
}

pub struct DashboardView {
    kind: DbKind,
    product: Product,
    points: VecDeque<Point>,
    in_flight: bool,
    last_poll: Option<Instant>,
    /// Index into [`INTERVALS`].
    interval: usize,
    pub live: bool,
    error: Option<String>,
    tables: Load<QueryResult>,
    tables_at: Option<Instant>,
}

impl DashboardView {
    pub fn new(kind: DbKind, product: Product) -> Self {
        Self {
            kind,
            product,
            points: VecDeque::new(),
            in_flight: false,
            last_poll: None,
            interval: 1,
            live: true,
            error: None,
            tables: Load::Idle,
            tables_at: None,
        }
    }

    pub fn title(&self) -> String {
        tr("Dashboard").to_string()
    }

    /// Readings taken so far.
    pub fn samples(&self) -> Vec<&Sample> {
        self.points.iter().map(|p| &p.sample).collect()
    }

    pub fn tables(&self) -> Option<&QueryResult> {
        self.tables.ready()
    }

    pub fn on_event(&mut self, ev: TabEvent) {
        let TabEvent::Dashboard(ev) = ev else { return };
        match *ev {
            DashboardEvent::Sample(Ok(sample)) => {
                self.in_flight = false;
                self.error = None;
                let now = Instant::now();
                let rate = self.points.back().and_then(|p| {
                    let secs = now.duration_since(p.taken).as_secs_f64();
                    dashboard::rate(p.sample.total_queries, sample.total_queries, secs)
                });
                let at = chrono::Local::now().format("%H:%M:%S").to_string();
                self.points.push_back(Point { at, taken: now, sample, rate });
                while self.points.len() > KEEP {
                    self.points.pop_front();
                }
            }
            DashboardEvent::Sample(Err(e)) => {
                self.in_flight = false;
                self.error = Some(e.message);
            }
            DashboardEvent::Tables(r) => self.tables = Load::from_result(r),
        }
    }

    fn poll(&mut self, cx: &TabCtx<'_>, tab: TabId, session: std::sync::Arc<dyn dbman_core::Connection>) {
        self.in_flight = true;
        self.last_poll = Some(Instant::now());
        let (kind, queries) = (self.kind, dashboard::queries(self.kind, self.product));
        cx.tasks.spawn(async move {
            let mut results = Vec::new();
            let mut failed = None;
            for q in &queries {
                match session.execute(None, q, 100).await {
                    Ok(StatementOutcome::Rows(r)) => results.push(r),
                    Ok(_) => {}
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            let result = match failed {
                Some(e) => Err(e),
                None => Ok(dashboard::read(kind, &results)),
            };
            Event::Tab { tab, event: TabEvent::Dashboard(Box::new(DashboardEvent::Sample(result))) }
        });
    }

    fn load_tables(&mut self, cx: &TabCtx<'_>, tab: TabId, session: std::sync::Arc<dyn dbman_core::Connection>) {
        let Some(sql) = dashboard::tables_sql(self.kind, self.product) else { return };
        self.tables_at = Some(Instant::now());
        if !matches!(self.tables, Load::Ready(_)) {
            self.tables = Load::Loading;
        }
        cx.tasks.spawn(async move {
            let result = session.execute(None, sql, 10).await.and_then(|o| match o {
                StatementOutcome::Rows(r) => Ok(r),
                other => Err(dbman_core::DbError::new(format!("unexpected result: {other:?}"))),
            });
            Event::Tab { tab, event: TabEvent::Dashboard(Box::new(DashboardEvent::Tables(result))) }
        });
    }

    pub fn ui(&mut self, ui: &mut Ui, tab: TabId, mut cx: TabCtx<'_>) {
        let Some(session) = cx.conn.and_then(ConnState::session) else {
            not_connected(ui, &mut cx);
            return;
        };
        let t = Theme::get(ui.ctx());
        if !dashboard::supported(self.kind, self.product) {
            ui.label(RichText::new(tr("This server has no dashboard.")).color(t.muted_foreground));
            return;
        }
        let every = Duration::from_secs(INTERVALS[self.interval]);
        let due = self.last_poll.is_none_or(|p| p.elapsed() >= every);
        if !self.in_flight && (self.points.is_empty() && self.last_poll.is_none() || self.live && due) {
            self.poll(&cx, tab, session.clone());
        }
        if self.tables.is_idle() || (self.live && self.tables_at.is_some_and(|t| t.elapsed() >= TABLES_EVERY)) {
            self.load_tables(&cx, tab, session.clone());
        }
        if self.live {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }

        ui.horizontal(|ui| {
            ui.add(Switch::new(&mut self.live).label(tr("Live")));
            ui.label(RichText::new(tr("every")).color(t.muted_foreground));
            let labels: Vec<String> = INTERVALS.iter().map(|s| trf("{n} s", &[("n", s)])).collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let mut i = Some(self.interval);
            ui.add(Select::new(&mut i, &refs).width(90.0));
            self.interval = i.unwrap_or(1);
            if toolbar_button(ui, tr("⟳ Refresh")).clicked() && !self.in_flight {
                self.poll(&cx, tab, session.clone());
                self.load_tables(&cx, tab, session.clone());
            }
            if self.in_flight {
                ui.add(shadcn_egui::Spinner::new().size(12.0));
            }
        });
        ui.add_space(8.0);
        if let Some(e) = &self.error {
            error_alert(ui, tr("The server could not be read"), e);
        }
        let Some(last) = self.points.back() else { return };

        // The latest values.
        let s = &last.sample;
        let num = |v: Option<f64>| v.map_or_else(|| "—".to_string(), |v| dbman_core::pivot::format(v.round()));
        let mut tiles: Vec<(&str, String)> = vec![
            (tr("Connections"), num(s.connections)),
            (tr("Running now"), num(s.active)),
            (tr(dashboard::throughput_label(self.kind)), last.rate.map_or("—".into(), |r| format!("{r:.1}"))),
        ];
        if self.kind != DbKind::Redis {
            tiles.push((tr("Long-running"), num(s.slow)));
        }
        let size_label = if self.kind == DbKind::Redis { tr("Memory used") } else { tr("Database size") };
        tiles.push((size_label, s.size.map_or("—".into(), dashboard::bytes_label)));
        ui.horizontal_wrapped(|ui| {
            for (name, value) in tiles {
                egui::Frame::new()
                    .stroke((1.0, t.border))
                    .corner_radius(t.radius_md())
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.set_min_width(150.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new(name).size(12.0).color(t.muted_foreground));
                            ui.label(RichText::new(value).size(22.0).strong().color(t.foreground));
                        });
                    });
            }
        });
        ui.add_space(10.0);

        // Charts over the readings kept.
        let connections =
            self.series(&[(tr("Connections"), |p| p.sample.connections), (tr("Running now"), |p| p.sample.active)]);
        let throughput = self.series(&[(tr(dashboard::throughput_label(self.kind)), |p| p.rate)]);
        let slow = self.series(&[(tr("Long-running"), |p| p.sample.slow)]);
        let height = 170.0;
        let chart = |ui: &mut Ui, title: &str, r: &QueryResult| {
            ui.label(RichText::new(title).strong());
            let spec = ChartSpec { kind: ChartKind::Line, x: 0, ys: (1..r.columns.len()).collect() };
            chart_view::show(ui, r, &spec, height);
        };
        ui.columns(2, |cols| {
            chart(&mut cols[0], tr("Connections"), &connections);
            chart(&mut cols[1], tr(dashboard::throughput_label(self.kind)), &throughput);
        });
        if self.kind != DbKind::Redis {
            ui.add_space(8.0);
            let slow_title = trf("Queries running over {n} s", &[("n", &dashboard::SLOW_SECONDS)]);
            if dashboard::tables_sql(self.kind, self.product).is_some() {
                ui.columns(2, |cols| {
                    chart(&mut cols[0], &slow_title, &slow);
                    self.tables_ui(&mut cols[1], tab);
                });
            } else {
                ui.columns(2, |cols| chart(&mut cols[0], &slow_title, &slow));
            }
        }
    }

    /// The readings as a result to chart: the time, then one column per series.
    fn series(&self, series: &[(&str, Pick)]) -> QueryResult {
        let mut columns = vec![ColumnMeta::new(tr("Time"), "text")];
        columns.extend(series.iter().map(|(name, _)| ColumnMeta::new(*name, "float8")));
        let cell = |v: Option<f64>| v.map_or(Cell::Null, |v| Cell::Text(v.to_string()));
        let rows = self
            .points
            .iter()
            .map(|p| std::iter::once(Cell::Text(p.at.clone())).chain(series.iter().map(|(_, f)| cell(f(p)))).collect())
            .collect();
        QueryResult { columns, rows, truncated: false }
    }

    fn tables_ui(&self, ui: &mut Ui, tab: TabId) {
        let t = Theme::get(ui.ctx());
        ui.label(RichText::new(tr("Largest tables")).strong());
        match &self.tables {
            Load::Ready(r) if r.rows.is_empty() => {
                ui.label(RichText::new(tr("No tables.")).color(t.muted_foreground));
            }
            Load::Ready(r) => {
                let rows: Vec<Vec<String>> = r
                    .rows
                    .iter()
                    .map(|row| {
                        let text = |i: usize| row.get(i).map(|c| c.display().into_owned()).unwrap_or_default();
                        let bytes = row.get(1).and_then(|c| c.display().parse::<f64>().ok());
                        vec![text(0), bytes.map(dashboard::bytes_label).unwrap_or_default(), text(2)]
                    })
                    .collect();
                let columns = vec![
                    Column::new(tr("Table")).weight(2.0),
                    Column::new(tr("Size")).right(),
                    Column::new(tr("Rows")).right(),
                ];
                DataTable::new(("dashboard-tables", tab), columns).row_height(26.0).show(ui, &rows);
            }
            Load::Failed(e) => error_alert(ui, tr("The tables could not be read"), e),
            Load::Idle | Load::Loading => {
                ui.add(shadcn_egui::Spinner::new());
            }
        }
    }
}
