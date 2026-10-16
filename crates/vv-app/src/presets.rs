//! Attribute presets of the library: saved from a clip with the "paste
//! attributes" checkboxes, applied by dropping them on clips, double
//! clicking them or with their own shortcut.

use super::*;
use std::collections::HashSet;
use vv_core::Clip;

use crate::library::{AttributePreset, Library};
use crate::paste_attributes::{
    Attribute, Conversion, KeyframeAnchor, Section, TimelineFormat, attribute_checkboxes,
};
use crate::settings::{Action, Keymap, Shortcut};

/// Saves a new preset or edits one of the library.
pub(crate) struct PresetDialog {
    /// Id of the preset being edited, `None` for a new one.
    editing: Option<String>,
    name: String,
    preset: AttributePreset,
    selected: HashSet<Attribute>,
    focus_name: bool,
    capturing: bool,
    notice: Option<String>,
}

impl PresetDialog {
    fn new(editing: Option<String>, name: String, preset: AttributePreset) -> Self {
        Self {
            editing,
            name,
            selected: preset.attributes.iter().copied().collect(),
            preset,
            focus_name: true,
            capturing: false,
            notice: None,
        }
    }

    /// While waiting for a key, global shortcuts must be suspended.
    pub(crate) fn is_capturing(&self) -> bool {
        self.capturing
    }

    /// Takes the key combination pressed while capturing, unless an action
    /// of the program already uses it. Returns whether a key was taken.
    fn capture_shortcut(
        &mut self,
        ctx: &egui::Context,
        keymap: &Keymap,
        library: &Library,
    ) -> bool {
        if !self.capturing {
            return false;
        }
        let Some(shortcut) = ctx.input(|i| i.events.iter().find_map(Shortcut::captured)) else {
            return false;
        };
        self.capturing = false;
        if shortcut == Shortcut::plain(egui::Key::Escape) {
            return true;
        }
        let actions: Vec<_> = Action::ALL
            .into_iter()
            .filter(|&a| keymap.shortcuts(a).contains(&shortcut))
            .map(|a| a.label())
            .collect();
        if !actions.is_empty() {
            self.notice = Some(
                t!(
                    "preset.shortcut_used",
                    shortcut = shortcut,
                    actions = actions.join(", ")
                )
                .into_owned(),
            );
            return true;
        }
        self.preset.shortcut = Some(shortcut);
        self.notice = library
            .presets
            .iter()
            .find(|(id, p)| p.shortcut == Some(shortcut) && Some(id) != self.editing.as_ref())
            .map(|(_, p)| t!("preset.shortcut_moved", name = p.name).into_owned());
        true
    }
}

impl VenturiApp {
    /// From the first video and the first audio clip of `clips`, in
    /// timeline order.
    pub(crate) fn open_save_preset_dialog(&mut self, clips: &[(usize, ClipId)]) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let tl = &self.session.project.timelines[timeline_id];
        let mut clips: Vec<(usize, &Clip)> = clips
            .iter()
            .filter_map(|&(track, id)| Some((track, tl.clip(track, id)?)))
            .collect();
        clips.sort_by_key(|(track, clip)| (clip.timeline_start, *track));
        let first_of = |kind: TrackKind| {
            clips
                .iter()
                .find(|(track, _)| tl.tracks[*track].kind == kind)
                .map(|(_, clip)| (*clip).clone())
        };
        let (video, audio) = (first_of(TrackKind::Video), first_of(TrackKind::Audio));
        let Some(source) = video.as_ref().or(audio.as_ref()) else {
            return;
        };
        let format = TimelineFormat {
            fps: tl.fps,
            resolution: tl.resolution,
        };
        let name = self.clip_display_name(source);
        let mut preset = AttributePreset::new(String::new(), format, video, audio);
        preset.attributes = Attribute::ALL
            .into_iter()
            .filter(|a| self.paste_attributes_selection.contains(a))
            .filter(|a| match a.section().kind() {
                Some(TrackKind::Video) => preset.video.is_some(),
                Some(TrackKind::Audio) => preset.audio.is_some(),
                None => true,
            })
            .collect();
        self.preset_dialog = Some(PresetDialog::new(None, name, preset));
    }

    fn open_edit_preset_dialog(&mut self, id: &str) {
        if let Some(preset) = self.library.preset(id) {
            let dialog =
                PresetDialog::new(Some(id.to_owned()), preset.name.clone(), preset.clone());
            self.preset_dialog = Some(dialog);
        }
    }

    pub(crate) fn show_preset_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.preset_dialog else {
            return;
        };
        // The key that ends a capture must not also confirm the dialog.
        let captured = dialog.capture_shortcut(ctx, &self.settings.keymap, &self.library);
        let mut open = true;
        let (mut save, mut cancel) = (false, false);
        let title = match dialog.editing {
            Some(_) => t!("preset.edit_title"),
            None => t!("preset.save_title"),
        };
        egui::Window::new(title)
            .id(egui::Id::new("preset_dialog"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(t!("preset.name"));
                    let edit = ui.text_edit_singleline(&mut dialog.name);
                    if std::mem::take(&mut dialog.focus_name) {
                        edit.request_focus();
                    }
                });
                shortcut_field(ui, dialog);

                ui.separator();
                let sections: Vec<Section> = Section::ALL
                    .into_iter()
                    .filter(|s| match s.kind() {
                        Some(TrackKind::Video) => dialog.preset.video.is_some(),
                        Some(TrackKind::Audio) => dialog.preset.audio.is_some(),
                        None => true,
                    })
                    .collect();
                attribute_checkboxes(ui, &mut dialog.selected, &sections);

                ui.separator();
                ui.label(egui::RichText::new(t!("paste_attr.keyframes")).strong());
                for (anchor, label) in [
                    (KeyframeAnchor::Start, t!("preset.anchor_start")),
                    (KeyframeAnchor::End, t!("preset.anchor_end")),
                    (KeyframeAnchor::Stretch, t!("paste_attr.stretch_to_fit")),
                ] {
                    ui.radio_value(&mut dialog.preset.anchor, anchor, label);
                }
                if dialog.selected.contains(&Attribute::Filters) {
                    ui.separator();
                    ui.label(egui::RichText::new(t!("props.filters")).strong());
                    ui.radio_value(
                        &mut dialog.preset.append_filters,
                        false,
                        t!("preset.filters_replace"),
                    );
                    ui.radio_value(
                        &mut dialog.preset.append_filters,
                        true,
                        t!("preset.filters_append"),
                    );
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(t!("common.cancel")).clicked() {
                        cancel = true;
                    }
                    let ready = !dialog.selected.is_empty() && !dialog.name.trim().is_empty();
                    let enter = !captured
                        && !dialog.capturing
                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui
                        .add_enabled(ready, egui::Button::new(t!("preset.save")))
                        .clicked()
                        || (ready && enter)
                    {
                        save = true;
                    }
                });
            });
        if !open {
            cancel = true;
        }
        if !(save || cancel) {
            return;
        }
        let dialog = self.preset_dialog.take().expect("just checked");
        if !save {
            return;
        }
        let mut preset = dialog.preset;
        preset.name = dialog.name.trim().to_owned();
        preset.attributes = Attribute::ALL
            .into_iter()
            .filter(|a| dialog.selected.contains(a))
            .collect();
        let result = match dialog.editing {
            Some(id) => self.library.update_preset(&id, |p| *p = preset),
            None => self.library.add_preset(preset).map(drop),
        };
        if let Err(e) = result {
            self.project_error = Some(t!("library.save_failed", error = e).into_owned());
        }
    }

    pub(crate) fn apply_preset_by_id(&mut self, id: &str, targets: &[(usize, ClipId)]) {
        if let Some(preset) = self.library.preset(id).cloned() {
            self.apply_preset(&preset, targets);
        }
    }

    pub(crate) fn apply_preset_to_selection(&mut self, id: &str) {
        let targets: Vec<(usize, ClipId)> = self.timeline_state.selected.iter().copied().collect();
        self.apply_preset_by_id(id, &targets);
    }

    pub(crate) fn apply_preset(&mut self, preset: &AttributePreset, targets: &[(usize, ClipId)]) {
        let selected: HashSet<Attribute> = preset.attributes.iter().copied().collect();
        let from = preset.format();
        self.apply_attributes(
            targets,
            preset.video.as_ref(),
            preset.audio.as_ref(),
            &selected,
            |source, target, tl| {
                let to = TimelineFormat {
                    fps: tl.fps,
                    resolution: tl.resolution,
                };
                Conversion::preset(
                    source,
                    from,
                    target,
                    to,
                    preset.anchor,
                    preset.append_filters,
                )
            },
            vv_core::CommandLabel::ApplyPreset,
        );
    }

    /// The "Presets" section of the Effects panel.
    pub(crate) fn show_preset_list(&mut self, ui: &mut egui::Ui, query: &str) {
        if self.library.presets.is_empty() {
            ui.weak(t!("preset.empty_hint"));
            return;
        }
        let mut apply = None;
        let mut edit = None;
        let mut delete = None;
        let mut shown = 0;
        for (id, preset) in &self.library.presets {
            if !media_pool::matches_search(&preset.name, query) {
                continue;
            }
            shown += 1;
            let resp = preset_item(ui, id, preset);
            if resp.double_clicked() {
                apply = Some(id.clone());
            }
            resp.context_menu(|ui| {
                if ui
                    .add_enabled(
                        !self.timeline_state.selected.is_empty(),
                        egui::Button::new(t!("preset.apply_to_selection")),
                    )
                    .clicked()
                {
                    apply = Some(id.clone());
                    ui.close();
                }
                if ui.button(t!("preset.edit")).clicked() {
                    edit = Some(id.clone());
                    ui.close();
                }
                ui.separator();
                ui.menu_button(t!("library.delete"), |ui| {
                    if ui.button(t!("library.delete_confirm")).clicked() {
                        delete = Some(id.clone());
                        ui.close();
                    }
                });
            });
        }
        if shown == 0 {
            ui.weak(t!("pool.search_no_results"));
        }
        if let Some(id) = apply {
            self.apply_preset_to_selection(&id);
        }
        if let Some(id) = edit {
            self.open_edit_preset_dialog(&id);
        }
        if let Some(id) = delete
            && let Err(e) = self.library.remove_preset(&id)
        {
            self.project_error = Some(t!("library.save_failed", error = e).into_owned());
        }
    }
}

fn shortcut_field(ui: &mut egui::Ui, dialog: &mut PresetDialog) {
    ui.horizontal(|ui| {
        ui.label(t!("preset.shortcut"));
        let text = match (dialog.capturing, dialog.preset.shortcut) {
            (true, _) => t!("settings.press_a_key").into_owned(),
            (false, Some(shortcut)) => shortcut.to_string(),
            (false, None) => t!("preset.shortcut_none").into_owned(),
        };
        if ui
            .button(text)
            .on_hover_text(t!("preset.shortcut_hint"))
            .clicked()
        {
            dialog.capturing = !dialog.capturing;
            dialog.notice = None;
        }
        if dialog.preset.shortcut.is_some()
            && ui
                .small_button("x")
                .on_hover_text(t!("settings.remove"))
                .clicked()
        {
            dialog.preset.shortcut = None;
            dialog.capturing = false;
            dialog.notice = None;
        }
    });
    if let Some(notice) = &dialog.notice {
        ui.colored_label(egui::Color32::YELLOW, notice);
    }
}

fn preset_item(ui: &mut egui::Ui, id: &str, preset: &AttributePreset) -> egui::Response {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
    let item_id = ui.id().with(("preset_item", id));
    let resp = ui
        .interact(rect, item_id, egui::Sense::click_and_drag())
        .on_hover_text(t!("preset.item_hint"));
    let visuals = ui.visuals();
    let (bg, stroke) = if resp.hovered() || resp.dragged() {
        (
            visuals.widgets.hovered.weak_bg_fill,
            visuals.widgets.hovered.fg_stroke.color,
        )
    } else {
        (
            visuals.widgets.inactive.weak_bg_fill,
            visuals.widgets.noninteractive.bg_stroke.color,
        )
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, bg);
    let thumb = egui::Rect::from_min_size(rect.min, egui::vec2(54.0, rect.height())).shrink(1.0);
    painter.rect_filled(thumb, 2.0, egui::Color32::from_gray(40));
    painter.text(
        thumb.center(),
        egui::Align2::CENTER_CENTER,
        "★",
        egui::FontId::proportional(14.0),
        egui::Color32::from_gray(220),
    );
    painter.rect_stroke(
        rect,
        3.0,
        egui::Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    painter.text(
        egui::pos2(thumb.right() + 14.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        &preset.name,
        egui::FontId::proportional(13.0),
        visuals.text_color(),
    );
    if let Some(shortcut) = preset.shortcut {
        painter.text(
            rect.right_center() - egui::vec2(8.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            shortcut.to_string(),
            egui::FontId::monospace(12.0),
            visuals.weak_text_color(),
        );
    }
    resp.dnd_set_drag_payload(timeline_ui::PresetDrag(id.to_owned()));
    if resp.dragged() {
        show_drag_ghost(ui, item_id, &preset.name);
    }
    resp
}

#[cfg(test)]
#[path = "tests/presets.rs"]
mod tests;
