//! Small vector icons for the explorer and tabs, painted so they do not
//! depend on which symbols the bundled fonts happen to contain.

use egui::epaint::{EllipseShape, PathStroke};
use egui::{Color32, FontId, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, Vec2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Database,
    Schema,
    Table,
    View,
    Function,
    Trigger,
    Sequence,
    Type,
    Column,
    Key,
    Index,
    ForeignKey,
    Console,
    Info,
}

pub const SIZE: f32 = 14.0;

pub fn icon(ui: &mut Ui, icon: Icon, color: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(SIZE), Sense::hover());
    paint(ui.painter(), rect, icon, color);
    response
}

pub fn paint(p: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let s = Stroke::new(1.3, color);
    let r = rect.shrink(1.5);
    let c = r.center();
    let at = |x: f32, y: f32| Pos2::new(r.left() + x * r.width(), r.top() + y * r.height());
    let text = |t: &str, size: f32| {
        p.text(c, egui::Align2::CENTER_CENTER, t, FontId::proportional(size), color);
    };
    match icon {
        Icon::Database => {
            let rx = r.width() / 2.0;
            let ry = r.height() * 0.17;
            let top = Pos2::new(c.x, r.top() + ry);
            let bottom = Pos2::new(c.x, r.bottom() - ry);
            p.add(EllipseShape::stroke(top, Vec2::new(rx, ry), s));
            p.line_segment([Pos2::new(r.left(), top.y), Pos2::new(r.left(), bottom.y)], s);
            p.line_segment([Pos2::new(r.right(), top.y), Pos2::new(r.right(), bottom.y)], s);
            // Lower half of the bottom ellipse.
            let pts: Vec<Pos2> = (0..=12)
                .map(|i| {
                    let a = std::f32::consts::PI * i as f32 / 12.0;
                    Pos2::new(c.x + rx * a.cos(), bottom.y + ry * a.sin())
                })
                .collect();
            p.add(Shape::line(pts, PathStroke::new(s.width, color)));
        }
        Icon::Schema => {
            p.add(Shape::closed_line(vec![at(0.5, 0.0), at(1.0, 0.5), at(0.5, 1.0), at(0.0, 0.5)], s));
        }
        Icon::Table => {
            p.rect_stroke(r, 2.0, s, egui::StrokeKind::Inside);
            let header = Rect::from_min_max(r.min, at(1.0, 0.33));
            p.rect_filled(header, egui::CornerRadius { nw: 2, ne: 2, sw: 0, se: 0 }, color);
            p.line_segment([at(0.0, 0.66), at(1.0, 0.66)], Stroke::new(1.0, color));
            p.line_segment([at(0.5, 0.33), at(0.5, 1.0)], Stroke::new(1.0, color));
        }
        Icon::View => {
            p.rect_stroke(r, 2.0, s, egui::StrokeKind::Inside);
            p.line_segment([at(0.0, 0.33), at(1.0, 0.33)], Stroke::new(1.0, color));
            p.circle_stroke(at(0.5, 0.66), r.width() * 0.16, Stroke::new(1.0, color));
        }
        Icon::Function => text("ƒ", 14.0),
        Icon::Sequence => text("#", 12.0),
        Icon::Type => text("T", 12.0),
        Icon::Console => text("›_", 11.0),
        Icon::Trigger => {
            p.add(Shape::line(
                vec![
                    at(0.65, 0.0),
                    at(0.2, 0.55),
                    at(0.55, 0.55),
                    at(0.35, 1.0),
                    at(0.85, 0.4),
                    at(0.5, 0.4),
                    at(0.65, 0.0),
                ],
                PathStroke::new(1.2, color),
            ));
        }
        Icon::Column => {
            p.rect_filled(Rect::from_center_size(c, Vec2::new(r.width() * 0.5, r.height() * 0.5)), 1.5, color);
        }
        Icon::Key => {
            let head = at(0.3, 0.35);
            p.circle_stroke(head, r.width() * 0.25, s);
            p.line_segment([at(0.45, 0.5), at(1.0, 1.0)], s);
            p.line_segment([at(0.78, 0.8), at(0.95, 0.63)], s);
        }
        Icon::Index => {
            for (i, w) in [1.0, 0.75, 0.5].iter().enumerate() {
                let y = 0.2 + i as f32 * 0.3;
                p.line_segment([at(0.0, y), at(*w, y)], s);
            }
        }
        Icon::ForeignKey => {
            p.line_segment([at(0.1, 0.9), at(0.9, 0.1)], s);
            p.line_segment([at(0.45, 0.1), at(0.9, 0.1)], s);
            p.line_segment([at(0.9, 0.1), at(0.9, 0.55)], s);
        }
        Icon::Info => {
            p.circle_stroke(c, r.width() / 2.0, s);
            p.text(c, egui::Align2::CENTER_CENTER, "i", FontId::proportional(10.0), color);
        }
    }
}
