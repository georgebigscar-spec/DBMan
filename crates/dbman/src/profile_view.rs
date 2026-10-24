//! The profile of a table column in a dialog: counts, range, the most frequent
//! values as bars and a histogram of numbers.

use dbman_core::chart::short_number;
use dbman_core::profile::Profile;
use egui::{RichText, Sense, Ui};
use shadcn_egui::{Button, Dialog, Theme};

use crate::explorer::Load;
use crate::i18n::{tr, trf};

pub struct ProfileDialog {
    pub column: String,
    pub result: Load<Profile>,
}

fn percent(n: i64, of: i64) -> String {
    if of <= 0 { "0%".into() } else { format!("{:.1}%", n as f64 * 100.0 / of as f64) }
}

impl ProfileDialog {
    pub fn new(column: String) -> Self {
        Self { column, result: Load::Loading }
    }

    /// Draws the dialog; false once it is closed.
    pub fn show(&self, ctx: &egui::Context) -> bool {
        let title = trf("Profile of {column}", &[("column", &self.column)]);
        let r = Dialog::new("column-profile", title)
            .description(tr("Counted over the rows the WHERE filter selects, not only the page on screen."))
            .width(640.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(520.0).show(ui, |ui| match &self.result {
                    Load::Idle | Load::Loading => {
                        ui.horizontal(|ui| {
                            ui.add(shadcn_egui::Spinner::new());
                            ui.label(tr("Reading the column…"));
                        });
                    }
                    Load::Failed(e) => crate::tabs::error_alert(ui, tr("The profile could not be read"), e),
                    Load::Ready(p) => body(ui, p),
                });
                ui.add_space(8.0);
                shadcn_egui::footer(ui, |ui| ui.add(Button::outline(tr("Close"))).clicked())
            });
        !(r.should_close || r.inner)
    }
}

fn body(ui: &mut Ui, p: &Profile) {
    let t = Theme::get(ui.ctx());
    ui.spacing_mut().item_spacing.y = 4.0;
    let stat = |ui: &mut Ui, name: &str, value: String| {
        ui.label(RichText::new(name).color(t.muted_foreground));
        ui.label(RichText::new(value).monospace());
        ui.end_row();
    };
    // Counts on the left, the range on the right.
    ui.horizontal_top(|ui| {
        egui::Grid::new("profile-counts").num_columns(2).spacing([16.0, 4.0]).min_col_width(70.0).show(ui, |ui| {
            stat(ui, tr("Rows"), p.rows.to_string());
            stat(ui, tr("Not NULL"), format!("{} ({})", p.non_null, percent(p.non_null, p.rows)));
            stat(ui, tr("NULL"), format!("{} ({})", p.nulls(), percent(p.nulls(), p.rows)));
            if let Some(d) = p.distinct {
                stat(ui, tr("Distinct"), format!("{d} ({})", percent(d, p.non_null)));
            }
        });
        ui.add_space(32.0);
        egui::Grid::new("profile-range").num_columns(2).spacing([16.0, 4.0]).min_col_width(70.0).show(ui, |ui| {
            if let Some(v) = &p.min {
                stat(ui, tr("Smallest"), cut(v, 40));
            }
            if let Some(v) = &p.max {
                stat(ui, tr("Largest"), cut(v, 40));
            }
            if let Some(v) = p.avg {
                stat(ui, tr("Average"), format!("{v:.4}").trim_end_matches('0').trim_end_matches('.').to_string());
            }
        });
    });
    if !p.histogram.is_empty() {
        ui.add_space(12.0);
        ui.label(RichText::new(tr("Distribution")).strong());
        histogram(ui, &p.histogram);
    }
    if !p.top.is_empty() {
        ui.add_space(12.0);
        ui.label(RichText::new(tr("Most frequent values")).strong());
        let most = p.top.iter().map(|(_, n)| *n).max().unwrap_or(1).max(1);
        egui::Grid::new("profile-top").num_columns(3).spacing([12.0, 2.0]).show(ui, |ui| {
            for (value, n) in &p.top {
                let label = match value {
                    Some(v) => RichText::new(cut(v, 40)).monospace(),
                    None => RichText::new("NULL").italics().color(t.muted_foreground),
                };
                ui.add(egui::Label::new(label).truncate());
                let (rect, _) = ui.allocate_exact_size(egui::vec2(240.0, 10.0), Sense::hover());
                let w = rect.width() * *n as f32 / most as f32;
                ui.painter().rect_filled(rect, 3.0, t.muted);
                let bar = egui::Rect::from_min_size(rect.min, egui::vec2(w, rect.height()));
                ui.painter().rect_filled(bar, 3.0, t.primary);
                ui.label(RichText::new(format!("{n} ({})", percent(*n, p.rows))).color(t.muted_foreground));
                ui.end_row();
            }
        });
    }
    if !p.errors.is_empty() {
        ui.add_space(8.0);
        ui.label(
            RichText::new(tr("Some figures are missing: the database cannot compute them for this type."))
                .size(12.0)
                .color(t.muted_foreground),
        )
        .on_hover_text(p.errors.join("\n"));
    }
}

fn cut(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn histogram(ui: &mut Ui, bars: &[(f64, f64, i64)]) {
    let t = Theme::get(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width().min(600.0), 120.0), Sense::hover());
    let plot = egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, rect.max.y - 18.0));
    let most = bars.iter().map(|b| b.2).max().unwrap_or(1).max(1);
    let w = plot.width() / bars.len() as f32;
    let painter = ui.painter_at(rect);
    let hover = resp.hover_pos();
    let mut tip = None;
    for (i, (lo, hi, n)) in bars.iter().enumerate() {
        let h = plot.height() * *n as f32 / most as f32;
        let x = plot.min.x + w * i as f32;
        let bar = egui::Rect::from_min_max(egui::pos2(x + 1.0, plot.max.y - h), egui::pos2(x + w - 1.0, plot.max.y));
        let column = egui::Rect::from_min_max(egui::pos2(x, plot.min.y), egui::pos2(x + w, plot.max.y));
        let hot = hover.is_some_and(|p| column.contains(p));
        let color = if hot { t.primary } else { t.primary.gamma_multiply(0.75) };
        painter.rect_filled(bar, 2.0, color);
        if hot {
            tip = Some(trf("{lo} to {hi}: {n}", &[("lo", &short_number(*lo)), ("hi", &short_number(*hi)), ("n", n)]));
        }
    }
    painter.line_segment([plot.left_bottom(), plot.right_bottom()], (1.0, t.border));
    let font = egui::FontId::proportional(11.0);
    let (lo, hi) = (bars[0].0, bars[bars.len() - 1].1);
    painter.text(rect.left_bottom(), egui::Align2::LEFT_BOTTOM, short_number(lo), font.clone(), t.muted_foreground);
    painter.text(rect.right_bottom(), egui::Align2::RIGHT_BOTTOM, short_number(hi), font, t.muted_foreground);
    if let Some(tip) = tip {
        resp.on_hover_text(tip);
    }
}
