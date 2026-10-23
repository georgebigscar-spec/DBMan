//! A chart of a query result: bars, lines or a pie, drawn with the painter,
//! with a tooltip for the values under the pointer.

use dbman_core::QueryResult;
use dbman_core::chart::{self, ChartData, ChartKind, ChartSpec};
use egui::{Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui, Vec2, pos2, vec2};
use shadcn_egui::{Select, Theme, Toggle};

use crate::i18n::{tr, trf};

/// Series colors, picked to read on both themes.
const COLORS: [Color32; 8] = [
    Color32::from_rgb(0x3b, 0x82, 0xf6),
    Color32::from_rgb(0xf5, 0x9e, 0x0b),
    Color32::from_rgb(0x10, 0xb9, 0x81),
    Color32::from_rgb(0xef, 0x44, 0x44),
    Color32::from_rgb(0x8b, 0x5c, 0xf6),
    Color32::from_rgb(0x06, 0xb6, 0xd4),
    Color32::from_rgb(0xec, 0x48, 0x99),
    Color32::from_rgb(0x84, 0xcc, 0x16),
];

fn color(i: usize) -> Color32 {
    COLORS[i % COLORS.len()]
}

/// The chart's settings above it: the kind, the X column and the plotted columns.
pub fn controls(ui: &mut Ui, result: &QueryResult, spec: &mut ChartSpec) {
    let t = Theme::get(ui.ctx());
    let numeric = chart::numeric_columns(result);
    let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    ui.horizontal_wrapped(|ui| {
        let kinds: Vec<&str> = ChartKind::ALL.iter().map(|k| tr(k.label())).collect();
        let mut k = ChartKind::ALL.iter().position(|k| *k == spec.kind);
        ui.add(Select::new(&mut k, &kinds).width(110.0));
        if let Some(k) = k {
            spec.kind = ChartKind::ALL[k];
        }
        ui.add_space(8.0);
        let axis = if spec.kind == ChartKind::Pie { tr("Slices") } else { tr("X axis") };
        ui.label(RichText::new(axis).color(t.muted_foreground));
        let mut x = Some(spec.x);
        ui.add(Select::new(&mut x, &names).width(160.0));
        if let Some(x) = x
            && x != spec.x
        {
            spec.x = x;
            spec.ys.retain(|y| *y != x);
        }
        ui.add_space(8.0);
        let values = if spec.kind == ChartKind::Pie { tr("Value") } else { tr("Values") };
        ui.label(RichText::new(values).color(t.muted_foreground));
        for &i in numeric.iter().filter(|i| **i != spec.x) {
            let mut on = spec.ys.contains(&i);
            if ui.add(Toggle::new(&mut on, &result.columns[i].name).outline()).changed() {
                if spec.kind == ChartKind::Pie {
                    spec.ys = vec![i];
                } else if on {
                    spec.ys.push(i);
                    spec.ys.sort_unstable();
                } else {
                    spec.ys.retain(|y| *y != i);
                }
            }
        }
    });
}

/// Draws the chart of `result` in a `height` tall area of the full width.
pub fn show(ui: &mut Ui, result: &QueryResult, spec: &ChartSpec, height: f32) {
    let t = Theme::get(ui.ctx());
    let data = chart::data(result, spec);
    if data.series.is_empty() || data.labels.is_empty() {
        ui.label(RichText::new(tr("Pick a column with numbers to plot.")).color(t.muted_foreground));
        return;
    }
    if data.skipped > 0 {
        let note = trf("The first {n} rows are shown.", &[("n", &chart::MAX_POINTS)]);
        ui.label(RichText::new(note).size(12.0).color(t.muted_foreground));
    }
    if spec.kind != ChartKind::Pie && data.series.len() > 1 {
        legend(ui, data.series.iter().enumerate().map(|(i, s)| (color(i), s.name.clone())));
    }
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height.max(120.0)), Sense::hover());
    let hover = response.hover_pos();
    let tip = match spec.kind {
        ChartKind::Bar | ChartKind::Line => axes(ui, rect, &data, spec.kind, hover),
        ChartKind::Pie => pie(ui, rect, &data, hover),
    };
    if let Some(lines) = tip {
        response.on_hover_ui_at_pointer(|ui| {
            for (i, (c, text)) in lines.into_iter().enumerate() {
                let text = if i == 0 { RichText::new(text).strong() } else { RichText::new(text) };
                ui.horizontal(|ui| {
                    if let Some(c) = c {
                        swatch(ui, c);
                    }
                    ui.label(text.color(t.foreground));
                });
            }
        });
    }
}

fn swatch(ui: &mut Ui, c: Color32) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter().rect_filled(r, 2.0, c);
}

fn legend(ui: &mut Ui, items: impl Iterator<Item = (Color32, String)>) {
    let t = Theme::get(ui.ctx());
    ui.horizontal_wrapped(|ui| {
        for (c, name) in items {
            swatch(ui, c);
            ui.label(RichText::new(name).size(12.0).color(t.foreground));
            ui.add_space(8.0);
        }
    });
}

/// A tooltip: lines of text, each with the color of its series.
type Tip = Option<Vec<(Option<Color32>, String)>>;

/// Bars or lines over the labels, with a value axis and grid lines.
fn axes(ui: &Ui, rect: Rect, data: &ChartData, kind: ChartKind, hover: Option<Pos2>) -> Tip {
    let t = Theme::get(ui.ctx());
    let p = ui.painter_at(rect);
    let font = FontId::proportional(11.0);
    let (lo, hi) = data.range();
    let ticks = chart::ticks(lo, hi, 5);
    let (lo, hi) = (ticks[0], *ticks.last().unwrap_or(&1.0));
    let tick_width = ticks
        .iter()
        .map(|v| p.layout_no_wrap(chart::short_number(*v), font.clone(), t.muted_foreground).size().x)
        .fold(0.0, f32::max);
    let plot = Rect::from_min_max(
        pos2(rect.left() + tick_width + 10.0, rect.top() + 6.0),
        rect.right_bottom() - vec2(6.0, 22.0),
    );
    let y_of = |v: f64| plot.bottom() - ((v - lo) / (hi - lo)) as f32 * plot.height();
    for v in &ticks {
        let y = y_of(*v);
        p.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, t.border));
        p.text(
            pos2(plot.left() - 6.0, y),
            Align2::RIGHT_CENTER,
            chart::short_number(*v),
            font.clone(),
            t.muted_foreground,
        );
    }
    let n = data.labels.len();
    let slot = plot.width() / n as f32;
    let x_of = |i: usize| plot.left() + slot * (i as f32 + 0.5);

    // Labels under the axis, as many as fit.
    let widest = data.labels.iter().map(|l| l.chars().count().min(16)).max().unwrap_or(1) as f32 * 6.5 + 8.0;
    let every = (widest / slot).ceil().max(1.0) as usize;
    for (i, l) in data.labels.iter().enumerate().step_by(every) {
        let l: String = if l.chars().count() > 16 { l.chars().take(15).chain(['…']).collect() } else { l.clone() };
        p.text(pos2(x_of(i), plot.bottom() + 4.0), Align2::CENTER_TOP, l, font.clone(), t.muted_foreground);
    }

    let hovered = hover.filter(|h| plot.contains(*h)).map(|h| (((h.x - plot.left()) / slot) as usize).min(n - 1));
    if let Some(i) = hovered {
        let band = Rect::from_x_y_ranges(x_of(i) - slot / 2.0..=x_of(i) + slot / 2.0, plot.y_range());
        p.rect_filled(band, 0.0, t.muted.gamma_multiply(0.6));
    }
    let zero = y_of(0.0f64.clamp(lo, hi));
    match kind {
        ChartKind::Bar => {
            let k = data.series.len() as f32;
            let width = (slot * 0.8 / k).min(48.0);
            for (si, s) in data.series.iter().enumerate() {
                for (i, v) in s.values.iter().enumerate() {
                    let Some(v) = v else { continue };
                    let x = x_of(i) - width * k / 2.0 + width * si as f32;
                    let r =
                        Rect::from_x_y_ranges(x..=x + (width - 1.0).max(1.0), y_of(*v).min(zero)..=y_of(*v).max(zero));
                    p.rect_filled(r, 1.0, color(si));
                }
            }
        }
        ChartKind::Line | ChartKind::Pie => {
            for (si, s) in data.series.iter().enumerate() {
                // A gap for NULL: a line per run of values.
                let mut run: Vec<Pos2> = Vec::new();
                let flush = |run: &mut Vec<Pos2>| {
                    if run.len() > 1 {
                        p.add(Shape::line(std::mem::take(run), Stroke::new(2.0, color(si))));
                    }
                    run.clear();
                };
                for (i, v) in s.values.iter().enumerate() {
                    match v {
                        Some(v) => {
                            let at = pos2(x_of(i), y_of(*v));
                            if n <= 60 || hovered == Some(i) {
                                p.circle_filled(at, 3.0, color(si));
                            }
                            run.push(at);
                        }
                        None => flush(&mut run),
                    }
                }
                flush(&mut run);
            }
        }
    }
    p.line_segment([pos2(plot.left(), zero), pos2(plot.right(), zero)], Stroke::new(1.0, t.muted_foreground));

    let i = hovered?;
    let mut lines = vec![(None, data.labels[i].clone())];
    for (si, s) in data.series.iter().enumerate() {
        let v = s.values[i].map_or_else(|| "NULL".to_string(), |v| v.to_string());
        lines.push((Some(color(si)), format!("{}: {v}", s.name)));
    }
    Some(lines)
}

/// A pie of the first series, with its slices listed on the right.
fn pie(ui: &Ui, rect: Rect, data: &ChartData, hover: Option<Pos2>) -> Tip {
    let t = Theme::get(ui.ctx());
    let p = ui.painter_at(rect);
    let values: Vec<f64> = data.series[0].values.iter().map(|v| v.unwrap_or(0.0)).collect();
    let total: f64 = values.iter().sum();
    if total <= 0.0 {
        p.text(
            rect.center(),
            Align2::CENTER_CENTER,
            tr("No positive values to show."),
            FontId::proportional(13.0),
            t.muted_foreground,
        );
        return None;
    }
    let radius = (rect.height() / 2.0 - 8.0).min(rect.width() / 4.0);
    let center = pos2(rect.left() + radius + 16.0, rect.center().y);
    let hovered = hover.and_then(|h| {
        let d = h - center;
        if d.length() > radius {
            return None;
        }
        let mut a = d.y.atan2(d.x) + std::f32::consts::FRAC_PI_2;
        if a < 0.0 {
            a += std::f32::consts::TAU;
        }
        let mut acc = 0.0;
        values.iter().position(|v| {
            acc += (*v / total) as f32 * std::f32::consts::TAU;
            a <= acc
        })
    });
    // One mesh for the fill (triangles drawn apart would show seams), then smooth edges.
    let mut mesh = egui::Mesh::default();
    let mut edges = Vec::new();
    let mut start = -std::f32::consts::FRAC_PI_2;
    for (i, v) in values.iter().enumerate() {
        let sweep = (*v / total) as f32 * std::f32::consts::TAU;
        let r = if hovered == Some(i) { radius + 4.0 } else { radius };
        let steps = ((sweep / 0.04).ceil() as usize).max(1);
        let arc: Vec<Pos2> = (0..=steps)
            .map(|k| {
                let a = start + sweep * k as f32 / steps as f32;
                center + vec2(a.cos(), a.sin()) * r
            })
            .collect();
        let base = mesh.vertices.len() as u32;
        mesh.colored_vertex(center, color(i));
        for at in &arc {
            mesh.colored_vertex(*at, color(i));
        }
        for k in 0..steps as u32 {
            mesh.add_triangle(base, base + 1 + k, base + 2 + k);
        }
        edges.push((arc, color(i)));
        start += sweep;
    }
    p.add(Shape::mesh(mesh));
    for (arc, c) in edges {
        if values.len() > 1 {
            p.line_segment([center, arc[0]], Stroke::new(1.5, t.background));
        }
        p.add(Shape::line(arc, Stroke::new(1.0, c)));
    }
    // The slices' names and shares.
    let font = FontId::proportional(12.0);
    let mut y = rect.top() + 8.0;
    let x = center.x + radius + 32.0;
    for (i, (l, v)) in data.labels.iter().zip(&values).enumerate() {
        p.rect_filled(Rect::from_min_size(pos2(x, y + 2.0), Vec2::splat(10.0)), 2.0, color(i));
        let pct = v / total * 100.0;
        let text = format!("{}  {pct:.1}%", if *l == "Other" { tr("Other") } else { l });
        let fg = if hovered == Some(i) { t.foreground } else { t.muted_foreground };
        p.text(pos2(x + 16.0, y), Align2::LEFT_TOP, text, font.clone(), fg);
        y += 18.0;
    }
    let i = hovered?;
    let pct = values[i] / total * 100.0;
    Some(vec![
        (Some(color(i)), data.labels[i].clone()),
        (None, format!("{}: {} ({pct:.1}%)", data.series[0].name, values[i])),
    ])
}
