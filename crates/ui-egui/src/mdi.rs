//! Edit and Mix as windows inside the main window, the way the incumbent shows them on Windows:
//! each can be minimized to a bar at the bottom, restored to a window you can move and resize,
//! maximized to fill the main window, closed, and brought to the front. When the window in front
//! is maximized its minimize, restore and close buttons sit at the right of the menu bar.

use crate::theme::{Tokens, bold, regular};
use crate::{MainWindow, SoundApp};
use egui::{Align2, Color32, CornerRadius, Id, LayerId, Order, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use serde::{Deserialize, Serialize};

/// Height of a window's title bar, and of a minimized window's bar.
const TITLE_H: f32 = 22.0;
/// Width of a minimized window's bar.
const MIN_BAR_W: f32 = 190.0;
/// The smallest a restored window gets.
const MIN_SIZE: egui::Vec2 = egui::vec2(360.0, 220.0);
/// The width of the edges that resize a restored window.
const GRIP: f32 = 6.0;

/// One window's state; `rect` is its restored placement relative to the main window's area
/// ([x, y, width, height]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChildWindow {
    pub open: bool,
    pub minimized: bool,
    pub maximized: bool,
    pub rect: [f32; 4],
}

impl Default for ChildWindow {
    fn default() -> Self {
        ChildWindow { open: true, minimized: false, maximized: true, rect: [24.0, 24.0, 1000.0, 620.0] }
    }
}

/// Edit and Mix window states.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Windows {
    /// Off: Edit and Mix each fill the main window in turn, with no window controls.
    pub enabled: bool,
    pub edit: ChildWindow,
    pub mix: ChildWindow,
}

impl Default for Windows {
    fn default() -> Self {
        Windows { enabled: true, edit: ChildWindow::default(), mix: ChildWindow { rect: [64.0, 56.0, 1000.0, 620.0], ..ChildWindow::default() } }
    }
}

impl Windows {
    pub fn get(&self, w: MainWindow) -> &ChildWindow {
        match w {
            MainWindow::Edit => &self.edit,
            MainWindow::Mix => &self.mix,
        }
    }
    pub fn get_mut(&mut self, w: MainWindow) -> &mut ChildWindow {
        match w {
            MainWindow::Edit => &mut self.edit,
            MainWindow::Mix => &mut self.mix,
        }
    }
}

fn other(w: MainWindow) -> MainWindow {
    match w {
        MainWindow::Edit => MainWindow::Mix,
        MainWindow::Mix => MainWindow::Edit,
    }
}

fn layer(w: MainWindow) -> LayerId {
    LayerId::new(Order::Background, Id::unique(("soundcraft.mdi", w == MainWindow::Edit)))
}

fn title(app: &SoundApp, w: MainWindow) -> String {
    let what = crate::localization::text(if w == MainWindow::Edit { "Edit" } else { "Mix" });
    format!("{what}: {}{}", app.engine.session().name, if app.engine.is_dirty() { " *" } else { "" })
}

/// Opens `w`, restores it if minimized and brings it to the front.
pub fn reveal(app: &mut SoundApp, w: MainWindow) {
    let c = app.ui.mdi.get_mut(w);
    c.open = true;
    c.minimized = false;
    app.ui.window = w;
    app.mdi_raise = Some(w);
}

/// What a window button asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    Minimize,
    Maximize,
    Restore,
    Close,
}

fn apply(app: &mut SoundApp, w: MainWindow, act: Act) {
    let c = app.ui.mdi.get_mut(w);
    match act {
        Act::Minimize => c.minimized = true,
        Act::Maximize => c.maximized = true,
        Act::Restore => {
            if c.minimized {
                c.minimized = false;
            } else {
                c.maximized = false;
            }
        }
        Act::Close => {
            c.open = false;
            c.minimized = false;
        }
    }
    match act {
        Act::Minimize | Act::Close => {
            // The other window comes forward when it is showing.
            let o = other(w);
            let oc = app.ui.mdi.get(o);
            if oc.open && !oc.minimized {
                app.ui.window = o;
                app.mdi_raise = Some(o);
            }
        }
        Act::Maximize | Act::Restore => {
            app.ui.window = w;
            app.mdi_raise = Some(w);
        }
    }
}

/// A title-bar button: a minimize bar, a restore square pair, a maximize square or a close cross.
fn button(ui: &mut Ui, r: Rect, id: Id, act: Act, name: &str) -> bool {
    let t = Tokens::current();
    let resp = ui.interact(r, id, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, name));
    let resp = resp.on_hover_text(crate::localization::text(name));
    let p = ui.painter();
    if resp.hovered() {
        let fill = if act == Act::Close { Color32::from_rgb(196, 43, 28) } else { Color32::from_white_alpha(28) };
        p.rect_filled(r, CornerRadius::same(2), fill);
    }
    let ink = Stroke::new(1.2, if resp.hovered() && act == Act::Close { Color32::WHITE } else { t.text });
    let c = r.center();
    let s = 4.0;
    match act {
        Act::Minimize => {
            p.line_segment([pos2(c.x - s, c.y + 3.0), pos2(c.x + s, c.y + 3.0)], ink);
        }
        Act::Maximize => {
            p.rect_stroke(Rect::from_center_size(c, vec2(2.0 * s, 2.0 * s)), 0.0, ink, StrokeKind::Inside);
        }
        Act::Restore => {
            let back = Rect::from_center_size(c + vec2(1.5, -1.5), vec2(2.0 * s - 1.0, 2.0 * s - 1.0));
            let front = Rect::from_center_size(c + vec2(-1.0, 1.0), vec2(2.0 * s - 1.0, 2.0 * s - 1.0));
            p.rect_stroke(back, 0.0, ink, StrokeKind::Inside);
            p.rect_filled(front, 0.0, t.menu_bar);
            p.rect_stroke(front, 0.0, ink, StrokeKind::Inside);
        }
        Act::Close => {
            p.line_segment([c + vec2(-s, -s), c + vec2(s, s)], ink);
            p.line_segment([c + vec2(-s, s), c + vec2(s, -s)], ink);
        }
    }
    resp.clicked()
}

/// Three window buttons right-aligned in `bar`; returns the one clicked.
fn buttons(ui: &mut Ui, bar: Rect, salt: (bool, &str), maximized: bool) -> Option<Act> {
    let size = vec2(26.0, bar.height().min(20.0));
    let mut x = bar.max.x - 2.0;
    let mut clicked = None;
    for (act, name) in
        [(Act::Close, "Close"), if maximized { (Act::Restore, "Restore Down") } else { (Act::Maximize, "Maximize") }, (Act::Minimize, "Minimize")]
    {
        let r = Rect::from_min_size(pos2(x - size.x, bar.center().y - size.y / 2.0), size);
        if button(ui, r, Id::unique(("mdi-button", salt.0, salt.1, name)), act, name) {
            clicked = Some(act);
        }
        x -= size.x + 2.0;
    }
    clicked
}

/// The front window's buttons at the right of the menu bar, while it is maximized. Returns the
/// width they take, so the title can sit to their left.
pub fn menu_bar_buttons(app: &mut SoundApp, ui: &mut Ui) -> f32 {
    if !app.ui.mdi.enabled || app.ui.mix_separate {
        return 0.0;
    }
    let w = app.ui.window;
    let c = *app.ui.mdi.get(w);
    if !(c.open && c.maximized && !c.minimized) {
        return 0.0;
    }
    let (r, _) = ui.allocate_exact_size(vec2(84.0, ui.available_height().min(20.0)), Sense::hover());
    if let Some(act) = buttons(ui, r, (w == MainWindow::Edit, "menu"), true) {
        apply(app, w, act);
    }
    r.width()
}

/// Draws Edit and Mix inside the rest of the main window.
pub fn show(app: &mut SoundApp, ui: &mut Ui) {
    let t = Tokens::current();
    let desk = ui.available_rect_before_wrap();
    ui.allocate_rect(desk, Sense::hover());
    ui.painter().rect_filled(desk, 0.0, t.panel_bg2.gamma_multiply(0.6));
    let ctx = ui.ctx().clone();
    if let Some(w) = app.mdi_raise.take() {
        ctx.move_to_top(layer(w));
    }
    // A window chosen elsewhere (the Window menu, a shortcut, an agent) comes forward.
    let front = app.ui.window;
    {
        let c = app.ui.mdi.get_mut(front);
        if (!c.open || c.minimized) && app.mdi_seen_front != Some(front) {
            c.open = true;
            c.minimized = false;
            ctx.move_to_top(layer(front));
        }
    }
    if app.mdi_seen_front != Some(front) {
        ctx.move_to_top(layer(front));
    }
    app.mdi_seen_front = Some(front);

    let shown = |app: &SoundApp, w: MainWindow| {
        let c = app.ui.mdi.get(w);
        c.open && !c.minimized
    };
    let front_covers = shown(app, front) && app.ui.mdi.get(front).maximized;
    let back = other(front);
    if front_covers {
        // A maximized window fills the main window as Edit and Mix always did: drawn straight
        // into it, and the window behind is hidden, so it costs nothing to draw.
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(desk).layout(egui::Layout::top_down(egui::Align::Min)));
        match front {
            MainWindow::Edit => crate::edit_window::show(app, &mut child),
            MainWindow::Mix => crate::mix_window::show(app, &mut child),
        }
    } else {
        if shown(app, back) {
            window(app, &ctx, desk, back);
        }
        if shown(app, front) {
            window(app, &ctx, desk, front);
        }
    }
    minimized_bars(app, &ctx, desk);
    if !shown(app, MainWindow::Edit) && !shown(app, MainWindow::Mix) {
        ui.painter().text(
            desk.center(),
            Align2::CENTER_CENTER,
            crate::localization::text("Window › Edit or Window › Mix opens a window"),
            regular(13.0),
            t.text_dim,
        );
    }
}

fn window(app: &mut SoundApp, ctx: &egui::Context, desk: Rect, w: MainWindow) {
    let t = Tokens::current();
    let c = *app.ui.mdi.get(w);
    let rect = if c.maximized {
        desk
    } else {
        let [x, y, wd, ht] = c.rect;
        let size = vec2(wd.max(MIN_SIZE.x).min(desk.width()), ht.max(MIN_SIZE.y).min(desk.height()));
        // Keep the title bar reachable inside the main window.
        let min = desk.min + vec2(x, y);
        let min = pos2(min.x.clamp(desk.min.x - size.x + 80.0, desk.max.x - 80.0), min.y.clamp(desk.min.y, desk.max.y - TITLE_H));
        Rect::from_min_size(min, size)
    };
    let lay = layer(w);
    let edit = w == MainWindow::Edit;
    egui::Area::new(lay.id).order(Order::Background).fixed_pos(rect.min).constrain(false).interactable(true).show(ctx, |ui| {
        ui.set_min_size(rect.size());
        ui.set_max_size(rect.size());
        ui.set_clip_rect(rect.intersect(desk));
        let front = app.ui.window == w;
        // A press anywhere in this window, where it is the top layer, brings it forward.
        if ui.input(|i| i.pointer.any_pressed())
            && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
            && rect.contains(pos)
            && ctx.layer_id_at(pos) == Some(lay)
            && !front
        {
            app.ui.window = w;
            app.mdi_seen_front = Some(w);
        }
        let content = if c.maximized {
            rect
        } else {
            // Frame, shadow and title bar.
            let p = ui.painter();
            p.rect_stroke(rect.expand(1.0), 0.0, Stroke::new(1.0, Color32::from_black_alpha(160)), StrokeKind::Outside);
            p.rect_filled(rect, 0.0, t.panel_bg);
            let bar = Rect::from_min_size(rect.min, vec2(rect.width(), TITLE_H));
            let fill = if front { t.menu_bar.lerp_to_gamma(t.accent, 0.18) } else { t.menu_bar };
            p.rect_filled(bar, 0.0, fill);
            p.with_clip_rect(Rect::from_min_max(bar.min, pos2(bar.max.x - 92.0, bar.max.y))).text(
                bar.left_center() + vec2(8.0, 0.0),
                Align2::LEFT_CENTER,
                title(app, w),
                bold(11.5),
                if front { t.text } else { t.text_dim },
            );
            let drag =
                ui.interact(Rect::from_min_max(bar.min, pos2(bar.max.x - 90.0, bar.max.y)), Id::unique(("mdi-title", edit)), Sense::click_and_drag());
            if drag.double_clicked() {
                apply(app, w, Act::Maximize);
            } else if drag.dragged() {
                let d = drag.drag_delta();
                let r = &mut app.ui.mdi.get_mut(w).rect;
                r[0] += d.x;
                r[1] += d.y;
            }
            if let Some(act) = buttons(ui, bar, (edit, "title"), false) {
                apply(app, w, act);
            }
            resize_edges(app, ui, rect, w);
            Rect::from_min_max(pos2(rect.min.x, bar.max.y), rect.max)
        };
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(content).layout(egui::Layout::top_down(egui::Align::Min)));
        child.set_clip_rect(content.intersect(desk));
        match w {
            MainWindow::Edit => crate::edit_window::show(app, &mut child),
            MainWindow::Mix => crate::mix_window::show(app, &mut child),
        }
    });
}

/// The right edge, bottom edge and bottom-right corner resize a restored window.
fn resize_edges(app: &mut SoundApp, ui: &mut Ui, rect: Rect, w: MainWindow) {
    let edit = w == MainWindow::Edit;
    let edges = [
        ("right", Rect::from_min_max(pos2(rect.max.x - GRIP, rect.min.y + TITLE_H), pos2(rect.max.x + GRIP * 0.5, rect.max.y)), (true, false)),
        ("bottom", Rect::from_min_max(pos2(rect.min.x, rect.max.y - GRIP), pos2(rect.max.x, rect.max.y + GRIP * 0.5)), (false, true)),
        ("corner", Rect::from_min_max(rect.max - vec2(14.0, 14.0), rect.max + vec2(GRIP * 0.5, GRIP * 0.5)), (true, true)),
    ];
    for (name, r, (dx, dy)) in edges {
        let resp = ui.interact(r, Id::unique(("mdi-resize", edit, name)), Sense::drag());
        let icon = match (dx, dy) {
            (true, true) => egui::CursorIcon::ResizeNwSe,
            (true, false) => egui::CursorIcon::ResizeHorizontal,
            _ => egui::CursorIcon::ResizeVertical,
        };
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(icon);
        }
        if resp.dragged() {
            let d = resp.drag_delta();
            let c = app.ui.mdi.get_mut(w);
            if dx {
                c.rect[2] = (c.rect[2].max(MIN_SIZE.x) + d.x).max(MIN_SIZE.x);
            }
            if dy {
                c.rect[3] = (c.rect[3].max(MIN_SIZE.y) + d.y).max(MIN_SIZE.y);
            }
        }
    }
    // Corner grip marks.
    let p = ui.painter();
    let ink = Stroke::new(1.0, Tokens::current().text_dim);
    for k in 1..=3 {
        let o = k as f32 * 3.5;
        p.line_segment([rect.max - vec2(o, 2.0), rect.max - vec2(2.0, o)], ink);
    }
}

/// Minimized windows: bars along the bottom of the main window.
fn minimized_bars(app: &mut SoundApp, ctx: &egui::Context, desk: Rect) {
    let t = Tokens::current();
    let mut x = desk.min.x + 4.0;
    for w in [MainWindow::Edit, MainWindow::Mix] {
        let c = *app.ui.mdi.get(w);
        if !(c.open && c.minimized) {
            continue;
        }
        let edit = w == MainWindow::Edit;
        let bar = Rect::from_min_size(pos2(x, desk.max.y - TITLE_H - 4.0), vec2(MIN_BAR_W, TITLE_H));
        x += MIN_BAR_W + 4.0;
        egui::Area::new(Id::unique(("soundcraft.mdi.min", edit))).order(Order::Middle).fixed_pos(bar.min).show(ctx, |ui| {
            ui.set_min_size(bar.size());
            let p = ui.painter();
            p.rect(bar, CornerRadius::same(2), t.menu_bar, Stroke::new(1.0, Color32::from_black_alpha(160)), StrokeKind::Inside);
            p.with_clip_rect(bar.shrink2(vec2(4.0, 0.0)).intersect(Rect::from_min_max(bar.min, pos2(bar.max.x - 60.0, bar.max.y)))).text(
                bar.left_center() + vec2(8.0, 0.0),
                Align2::LEFT_CENTER,
                title(app, w),
                regular(11.0),
                t.text,
            );
            let body =
                ui.interact(Rect::from_min_max(bar.min, pos2(bar.max.x - 56.0, bar.max.y)), Id::unique(("mdi-min-body", edit)), Sense::click());
            if body.clicked() {
                reveal(app, w);
            }
            let size = vec2(24.0, 18.0);
            let restore = Rect::from_min_size(pos2(bar.max.x - 52.0, bar.center().y - 9.0), size);
            let close = Rect::from_min_size(pos2(bar.max.x - 26.0, bar.center().y - 9.0), size);
            if button(ui, restore, Id::unique(("mdi-min-restore", edit)), Act::Restore, "Restore") {
                reveal(app, w);
            }
            if button(ui, close, Id::unique(("mdi-min-close", edit)), Act::Close, "Close") {
                apply(app, w, Act::Close);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> SoundApp {
        SoundApp::new(soundcraft_engine::demo::demo_engine(), None, crate::Services::default())
    }

    #[test]
    fn minimizing_the_front_window_brings_the_other_forward() {
        let mut a = app();
        a.ui.window = MainWindow::Edit;
        apply(&mut a, MainWindow::Edit, Act::Minimize);
        assert!(a.ui.mdi.edit.minimized);
        assert_eq!(a.ui.window, MainWindow::Mix);
        reveal(&mut a, MainWindow::Edit);
        assert!(!a.ui.mdi.edit.minimized);
        assert_eq!(a.ui.window, MainWindow::Edit);
    }

    #[test]
    fn restore_un_maximizes_and_close_hides() {
        let mut a = app();
        assert!(a.ui.mdi.edit.maximized);
        apply(&mut a, MainWindow::Edit, Act::Restore);
        assert!(!a.ui.mdi.edit.maximized && a.ui.mdi.edit.open);
        apply(&mut a, MainWindow::Edit, Act::Maximize);
        assert!(a.ui.mdi.edit.maximized);
        apply(&mut a, MainWindow::Mix, Act::Close);
        assert!(!a.ui.mdi.mix.open);
        // Choosing Mix again opens it.
        reveal(&mut a, MainWindow::Mix);
        assert!(a.ui.mdi.mix.open);
    }

    #[test]
    fn older_settings_without_window_states_get_defaults() {
        let w: Windows = serde_json::from_str("{}").unwrap();
        assert_eq!(w, Windows::default());
    }
}
