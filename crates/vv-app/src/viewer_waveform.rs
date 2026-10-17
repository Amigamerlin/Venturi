//! The viewer of an audio-only media: its waveform, one lane per audio
//! stream, with the playhead and the marked range.

use vv_core::FrameIdx;

pub(crate) struct AudioPreview<'a> {
    /// One per stream; `None` while still being generated.
    pub waveforms: Vec<Option<&'a vv_media::Waveform>>,
    pub total_frames: FrameIdx,
    pub fps: f64,
    pub playhead: FrameIdx,
    /// In and out, as `transport::MarkRange::resolve` gives them.
    pub marks: (FrameIdx, FrameIdx),
}

const LANE_GAP: f32 = 6.0;

pub(crate) fn paint(ui: &egui::Ui, rect: egui::Rect, preview: &AudioPreview) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
    let total = preview.total_frames.max(1);
    let total_secs = total as f64 / preview.fps.max(1e-9);
    let lanes = preview.waveforms.len().max(1);
    let lane_h = (rect.height() - LANE_GAP * (lanes - 1) as f32) / lanes as f32;
    let flat = vv_core::Keyframed::constant(0.0);
    for (i, waveform) in preview.waveforms.iter().enumerate() {
        let lane = egui::Rect::from_min_size(
            rect.left_top() + egui::vec2(0.0, i as f32 * (lane_h + LANE_GAP)),
            egui::vec2(rect.width(), lane_h),
        );
        match waveform {
            Some(waveform) => crate::timeline_ui::draw_clip_waveform(
                &painter,
                lane,
                &waveform.peaks,
                0.0,
                total_secs,
                preview.fps,
                waveform.audio_duration_secs,
                lane,
                &flat,
            ),
            None => {
                painter.text(
                    lane.center(),
                    egui::Align2::CENTER_CENTER,
                    t!("viewer.decoding"),
                    egui::FontId::proportional(14.0),
                    ui.visuals().weak_text_color(),
                );
            }
        }
    }
    let x_of = |frame: FrameIdx| rect.left() + rect.width() * frame as f32 / total as f32;
    let shade = egui::Color32::from_black_alpha(140);
    let (mark_in, mark_out) = preview.marks;
    painter.rect_filled(rect.with_max_x(x_of(mark_in)), 0.0, shade);
    painter.rect_filled(rect.with_min_x(x_of(mark_out)), 0.0, shade);
    painter.vline(
        x_of(preview.playhead),
        rect.y_range(),
        egui::Stroke::new(2.0, crate::theme::PLAYHEAD),
    );
}

/// The frame under screen `x`.
pub(crate) fn frame_at(rect: egui::Rect, x: f32, total_frames: FrameIdx) -> FrameIdx {
    let frac = ((x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
    ((frac * total_frames as f32) as FrameIdx).min((total_frames - 1).max(0))
}

#[cfg(test)]
#[path = "tests/viewer_waveform.rs"]
mod tests;
