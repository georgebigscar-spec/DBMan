//! Toolbars whose controls wrap onto more rows when the window is too narrow.

use std::hash::Hash;

use egui::{Align, Layout, Ui};

/// Which part of a [`toolbar`] is being added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Controls laid out from the left, wrapping onto more rows.
    Left,
    /// Controls laid out from the right edge of the first row, right to left.
    Right,
}

/// A row of controls: `add` is called for the left side and then the right
/// one. When the left side does not fit next to the right one it wraps onto
/// more rows below, and the right side stays at the top right.
pub fn toolbar(ui: &mut Ui, id_salt: impl Hash + std::fmt::Debug, mut add: impl FnMut(&mut Ui, Side)) {
    let id = ui.make_persistent_id(id_salt);
    // The right side's width from the last frame: it is laid out after the left.
    let right_width: f32 = ui.data(|d| d.get_temp(id)).unwrap_or(0.0);
    ui.horizontal_top(|ui| {
        let row = ui.spacing().interact_size.y;
        let gap = ui.spacing().item_spacing.x;
        let left = (ui.available_width() - right_width - gap).max(120.0);
        let wrap = Layout::left_to_right(Align::Center).with_main_wrap(true);
        ui.allocate_ui_with_layout(egui::vec2(left, row), wrap, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            add(ui, Side::Left);
        });
        let right = ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), row),
            Layout::right_to_left(Align::Center),
            |ui| add(ui, Side::Right),
        );
        let width = right.response.rect.width();
        if (width - right_width).abs() > 0.5 {
            ui.data_mut(|d| d.insert_temp(id, width));
            ui.ctx().request_discard("toolbar width changed");
        }
    });
}
