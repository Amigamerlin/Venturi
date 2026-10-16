//! Attribute presets of the library: saved from a clip with the "paste
//! attributes" checkboxes, applied by dropping them on clips, double
//! clicking them or with their "Apply preset N" shortcut.

use super::*;
use std::collections::HashSet;
use vv_core::Clip;

use crate::library::AttributePreset;
use crate::paste_attributes::{
    Attribute, Conversion, KeyframeAnchor, Section, TimelineFormat, attribute_checkboxes,
};

pub(crate) struct SavePresetDialog {
    name: String,
    preset: AttributePreset,
    selected: HashSet<Attribute>,
    focus_name: bool,
}

/// Slots of the "Apply preset N" shortcuts.
pub(crate) const HOTKEY_SLOTS: std::ops::RangeInclusive<u8> = 1..=9;

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
        let selected = self
            .paste_attributes_selection
            .iter()
            .copied()
            .filter(|a| match a.section().kind() {
                Some(TrackKind::Video) => video.is_some(),
                Some(TrackKind::Audio) => audio.is_some(),
                None => true,
            })
            .collect();
        self.save_preset = Some(SavePresetDialog {
            name,
            preset: AttributePreset::new(String::new(), format, video, audio),
            selected,
            focus_name: true,
        });
    }

    pub(crate) fn show_save_preset_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.save_preset else {
            return;
        };
        let mut open = true;
        let (mut save, mut cancel) = (false, false);
        egui::Window::new(t!("preset.save_title"))
            .id(egui::Id::new("save_preset_dialog"))
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
                hotkey_picker(ui, &mut dialog.preset.hotkey);

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(t!("common.cancel")).clicked() {
                        cancel = true;
                    }
                    let ready = !dialog.selected.is_empty() && !dialog.name.trim().is_empty();
                    if ui
                        .add_enabled(ready, egui::Button::new(t!("preset.save")))
                        .clicked()
                        || (ready && ui.input(|i| i.key_pressed(egui::Key::Enter)))
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
        let dialog = self.save_preset.take().expect("just checked");
        if !save {
            return;
        }
        let mut preset = dialog.preset;
        preset.name = dialog.name.trim().to_owned();
        preset.attributes = Attribute::ALL
            .into_iter()
            .filter(|a| dialog.selected.contains(a))
            .collect();
        if let Err(e) = self.library.add_preset(preset) {
            self.project_error = Some(t!("library.save_failed", error = e).into_owned());
        }
    }

    pub(crate) fn apply_preset_by_id(&mut self, id: &str, targets: &[(usize, ClipId)]) {
        if let Some(preset) = self.library.preset(id).cloned() {
            self.apply_preset(&preset, targets);
        }
    }

    pub(crate) fn apply_preset_hotkey(&mut self, slot: u8) {
        if let Some(preset) = self.library.preset_for_hotkey(slot).cloned() {
            let targets: Vec<(usize, ClipId)> =
                self.timeline_state.selected.iter().copied().collect();
            self.apply_preset(&preset, &targets);
        }
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
    pub(crate) fn show_preset_list(&mut self, ui: &mut egui::Ui) {
        effects_section_header(ui, &t!("effects.presets"));
        if self.library.presets.is_empty() {
            ui.weak(t!("preset.empty_hint"));
            return;
        }
        let mut apply = None;
        let mut changes: Vec<(String, PresetChange)> = Vec::new();
        for (id, preset) in &self.library.presets {
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
                ui.menu_button(t!("preset.rename"), |ui| {
                    let name = self
                        .library_ui
                        .renaming
                        .get_or_insert_with(|| (id.clone(), preset.name.clone()));
                    if name.0 != *id {
                        *name = (id.clone(), preset.name.clone());
                    }
                    let edit = ui.text_edit_singleline(&mut name.1);
                    if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        changes.push((id.clone(), PresetChange::Rename(name.1.clone())));
                        ui.close();
                    }
                });
                ui.menu_button(t!("preset.hotkey"), |ui| {
                    let mut hotkey = preset.hotkey;
                    if hotkey_picker(ui, &mut hotkey) {
                        changes.push((id.clone(), PresetChange::Hotkey(hotkey)));
                        ui.close();
                    }
                });
                ui.separator();
                ui.menu_button(t!("library.delete"), |ui| {
                    if ui.button(t!("library.delete_confirm")).clicked() {
                        changes.push((id.clone(), PresetChange::Delete));
                        ui.close();
                    }
                });
            });
        }
        if let Some(id) = apply {
            let targets: Vec<(usize, ClipId)> =
                self.timeline_state.selected.iter().copied().collect();
            self.apply_preset_by_id(&id, &targets);
        }
        for (id, change) in changes {
            let result = match change {
                PresetChange::Rename(name) if !name.trim().is_empty() => self
                    .library
                    .update_preset(&id, |p| p.name = name.trim().to_owned()),
                PresetChange::Rename(_) => Ok(()),
                PresetChange::Hotkey(hotkey) => {
                    self.library.update_preset(&id, |p| p.hotkey = hotkey)
                }
                PresetChange::Delete => self.library.remove_preset(&id),
            };
            if let Err(e) = result {
                self.project_error = Some(t!("library.save_failed", error = e).into_owned());
            }
        }
    }
}

enum PresetChange {
    Rename(String),
    Hotkey(Option<u8>),
    Delete,
}

/// Returns whether the choice changed.
fn hotkey_picker(ui: &mut egui::Ui, hotkey: &mut Option<u8>) -> bool {
    let before = *hotkey;
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("preset.hotkey"));
        ui.selectable_value(hotkey, None, t!("preset.hotkey_none"));
        for slot in HOTKEY_SLOTS {
            ui.selectable_value(hotkey, Some(slot), slot.to_string());
        }
    })
    .response
    .on_hover_text(t!("preset.hotkey_hint"));
    *hotkey != before
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
    if let Some(slot) = preset.hotkey {
        painter.text(
            rect.right_center() - egui::vec2(8.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            slot.to_string(),
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
