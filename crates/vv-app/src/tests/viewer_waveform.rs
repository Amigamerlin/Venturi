use super::*;

#[test]
fn a_click_maps_to_the_frame_under_it() {
    let rect = egui::Rect::from_min_size(egui::pos2(100.0, 0.0), egui::vec2(200.0, 50.0));
    assert_eq!(frame_at(rect, 100.0, 50), 0);
    assert_eq!(frame_at(rect, 200.0, 50), 25);
    assert_eq!(frame_at(rect, 300.0, 50), 49, "the end is the last frame");
    assert_eq!(frame_at(rect, 0.0, 50), 0);
    assert_eq!(frame_at(rect, 999.0, 50), 49);
}
