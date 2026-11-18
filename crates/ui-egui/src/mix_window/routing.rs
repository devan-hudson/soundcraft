use super::plugin_menu::{MenuSearch, menu_row};
use super::{Route, SoundApp, Track, TrackId, Ui, json};

pub(crate) fn route_name(app: &SoundApp, r: &Route) -> String {
    match r {
        Route::None => crate::localization::text("no output").to_string(),
        Route::Main => app.engine.session().outputs.first().map_or_else(|| crate::localization::text("Out 1-2").to_string(), |o| o.name.clone()),
        Route::Bus(b) => app.engine.session().bus(*b).map_or_else(|| "bus?".into(), |b| b.name.clone()),
        Route::Hardware(h) => h.clone(),
    }
}

pub(crate) fn input_name(app: &SoundApp, track: &Track) -> String {
    if track.kind.has_playlist() && track.mixer.input == Route::None {
        let channels = track.format.channels();
        if channels == 1 { crate::localized_format!("In {}", 1) } else { crate::localized_format!("In 1-{}", channels) }
    } else {
        route_name(app, &track.mixer.input)
    }
}

/// The output paths after the main one: the session's own, then every further stereo pair of
/// the open audio device ("Out 3-4" … "Out 15-16" on a 16-channel interface). Picking a pair the
/// session lacks adds it as an output path (`track.output` does that).
fn hardware_outputs(app: &SoundApp) -> Vec<(String, String)> {
    let s = app.engine.session();
    let mut out: Vec<(String, String)> = s.outputs.iter().skip(1).map(|o| (o.name.clone(), o.name.clone())).collect();
    let device = app.player.as_ref().filter(|p| !p.silent).map_or(0, |p| usize::from(p.device_channels));
    for first in (2..device.saturating_sub(1)).step_by(2) {
        let taken = s.outputs.iter().any(|o| usize::from(o.first_channel) == first && o.format.channels() == 2);
        if !taken {
            let name = format!("Out {}-{}", first + 1, first + 2);
            out.push((name.clone(), name));
        }
    }
    out
}

/// The input or output menu under `resp`. It closes only on a click outside, so its bus search
/// field can be typed in.
pub(crate) fn route_popup(app: &mut SoundApp, resp: &egui::Response, id: TrackId, input: bool) {
    egui::Popup::menu(resp).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| route_menu(app, ui, id, input));
}

/// Input or output menu: no route and buses on both, plus the main output (outputs) or inputs 1-8
/// (inputs). Its search field takes typing at once and finds any of them, not just the buses.
fn route_menu(app: &mut SoundApp, ui: &mut Ui, id: TrackId, input: bool) {
    let key = if input { "input" } else { "output" };
    let cmd = if input { "track.input" } else { "track.output" };
    let hint = crate::localization::text(if input { "Search inputs…" } else { "Search outputs…" });
    let search = MenuSearch::show(ui, egui::Id::unique(("route-search", id.0, input)), hint);
    let busses: Vec<String> = app.engine.session().busses.iter().map(|b| b.name.clone()).collect();
    let main_name = app.engine.session().outputs.first().map_or_else(|| "Out 1-2".to_string(), |o| o.name.clone());
    // (label, the route the command is given)
    let mut fixed = vec![(crate::localization::text(if input { "no input" } else { "no output" }).to_string(), "none".to_string())];
    if input {
        fixed.extend((1..=8).map(|i| (crate::localized_format!("In {}", i), format!("In {i}"))));
    } else {
        fixed.push((main_name, "main".to_string()));
        for (label, value) in hardware_outputs(app) {
            fixed.push((label, value));
        }
    }
    if search.typing() {
        let visible: Vec<(&String, &String)> =
            fixed.iter().map(|(l, v)| (l, v)).chain(busses.iter().map(|b| (b, b))).filter(|(l, _)| search.matches(l)).collect();
        let picked = search.rows(ui, visible.len(), 420.0, |ui, i, highlighted| visible.get(i).is_some_and(|(l, _)| menu_row(ui, highlighted, l)));
        if let Some((_, value)) = picked.and_then(|i| visible.get(i)) {
            let _ = app.run(cmd, json!({"track": id.0, key: value}));
            ui.close();
        }
        return;
    }
    for (label, value) in &fixed {
        if ui.button(label).clicked() {
            let _ = app.run(cmd, json!({"track": id.0, key: value}));
            ui.close();
        }
    }
    ui.separator();
    ui.strong(crate::localization::text("Bus"));
    if let Some(bus) = search
        .rows(ui, busses.len(), 420.0, |ui, i, highlighted| busses.get(i).is_some_and(|b| menu_row(ui, highlighted, b)))
        .and_then(|i| busses.get(i))
    {
        let _ = app.run(cmd, json!({"track": id.0, key: bus}));
        ui.close();
    }
    if ui.button(crate::localization::text("new bus...")).clicked() {
        let mut n = busses.len() + 1;
        while busses.contains(&format!("Bus {n}")) || app.engine.session().track_by_name(&format!("Bus {n}")).is_some() {
            n += 1;
        }
        let _ = app.run(cmd, json!({"track": id.0, key: format!("Bus {n}")}));
        ui.close();
    }
}

#[cfg(test)]
mod tests {
    use super::input_name;
    use crate::{Services, SoundApp};
    use serde_json::json;

    #[test]
    fn stereo_track_defaults_to_a_stereo_input_pair() {
        let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        app.run("track.new", json!({"format": "stereo"})).unwrap();
        let track = &app.engine.session().tracks[0];

        assert_eq!(input_name(&app, track), "In 1-2");
    }
}
