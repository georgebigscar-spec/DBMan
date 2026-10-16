//! Command palette (Ctrl+K): actions, open tabs, connections and the objects
//! already loaded in the explorer, behind one search field.

use shadcn_egui::{Command, fuzzy_match};
use uuid::Uuid;

use crate::events::TabId;
use crate::explorer::{Action, ConnState};
use crate::i18n::tr;
use crate::tabs::Tab;

/// Objects listed at most, so a huge schema doesn't slow the palette down.
const MAX_OBJECTS: usize = 100;

/// What was picked in the palette.
pub enum Pick {
    Action(Action),
    Tab(TabId),
    CloseTab(TabId),
    /// Selects the connection in the explorer, connecting it if needed.
    Conn(Uuid),
    ToggleDark,
}

pub struct Ctx<'a> {
    pub conns: &'a [ConnState],
    pub tabs: &'a [Tab],
    pub active: Option<TabId>,
    /// The connection a new console opens in.
    pub current: Option<Uuid>,
    pub dark: bool,
}

pub fn show(ctx: &egui::Context, open: &mut bool, cx: Ctx<'_>) -> Option<Pick> {
    let mut pick = None;
    Command::new("command-palette")
        .placeholder(tr("Search tables, tabs and commands…"))
        .empty(tr("Nothing found."))
        .max_height(360.0)
        .show_dialog(ctx, open, |cmd| {
            cmd.group(tr("Actions"), |cmd| {
                if cmd.item(tr("+ New connection"), None) {
                    pick = Some(Pick::Action(Action::NewConnection));
                }
                if let Some(id) = cx.current
                    && cmd.item(tr("New console"), Some("Ctrl+T"))
                {
                    pick = Some(Pick::Action(Action::OpenConsole(id, None)));
                }
                if let Some(id) = cx.active
                    && cmd.item(tr("Close the tab"), Some("Ctrl+W"))
                {
                    pick = Some(Pick::CloseTab(id));
                }
                let theme = if cx.dark { tr("Light theme") } else { tr("Dark theme") };
                if cmd.item_with_keywords(theme, "theme dark light", None) {
                    pick = Some(Pick::ToggleDark);
                }
            });
            if !cx.tabs.is_empty() {
                cmd.group(tr("Tabs"), |cmd| {
                    for tab in cx.tabs {
                        if cmd.item(&tab.title(), None) {
                            pick = Some(Pick::Tab(tab.id));
                        }
                    }
                });
            }
            if !cx.conns.is_empty() {
                cmd.group(tr("Connections"), |cmd| {
                    for c in cx.conns {
                        let name = c.cfg.display_name();
                        if cmd.item_with_keywords(&name, c.cfg.kind.label(), None) {
                            pick = Some(Pick::Conn(c.cfg.id));
                        }
                    }
                });
            }
            // Objects only once something is typed: an empty query would list them all.
            let query = cmd.query().to_owned();
            if !query.trim().is_empty() {
                cmd.group(tr("Objects"), |cmd| {
                    let mut shown = 0;
                    for c in cx.conns {
                        let conn = c.cfg.display_name();
                        for objects in c.objects.values().filter_map(|l| l.ready()) {
                            for o in objects {
                                if shown >= MAX_OBJECTS {
                                    return;
                                }
                                let p = &o.path;
                                let label = format!("{}.{}", p.schema, p.name);
                                let keywords = format!("{} {} {}", p.database, tr(o.kind.label()), conn);
                                if !fuzzy_match(&query, &format!("{label} {keywords}")) {
                                    continue;
                                }
                                shown += 1;
                                if cmd.item_with_keywords(&label, &keywords, Some(&conn)) {
                                    let id = c.cfg.id;
                                    pick = Some(Pick::Action(if o.kind.has_data() {
                                        Action::OpenData(id, o.clone())
                                    } else {
                                        Action::OpenObject(id, o.clone())
                                    }));
                                }
                            }
                        }
                    }
                });
            }
        });
    pick
}
