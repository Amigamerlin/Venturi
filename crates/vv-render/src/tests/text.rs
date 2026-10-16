use super::*;

fn covered_bounds(mask: &TextMask) -> Option<(u32, u32, u32, u32)> {
    let mut bounds: Option<(u32, u32, u32, u32)> = None;
    for y in 0..mask.height {
        for x in 0..mask.width {
            if mask.data[(y * mask.width + x) as usize] > 0 {
                let b = bounds.get_or_insert((x, y, x, y));
                b.0 = b.0.min(x);
                b.1 = b.1.min(y);
                b.2 = b.2.max(x);
                b.3 = b.3.max(y);
            }
        }
    }
    bounds
}

#[test]
fn default_title_is_drawn_around_the_center() {
    let render = render_title(&TitleParams::default(), (640, 360), (640, 360));
    let (x0, y0, x1, y1) = covered_bounds(render.text()).expect("no pixel drawn");
    let (cx, cy) = ((x0 + x1) / 2, (y0 + y1) / 2);
    assert!((cx as i32 - 320).abs() < 20, "centro x {cx}");
    assert!((cy as i32 - 180).abs() < 30, "centro y {cy}");
}

#[test]
fn left_anchor_starts_the_text_at_the_position() {
    let params = TitleParams {
        anchor: (HAnchor::Left, VAnchor::Middle),
        ..Default::default()
    };
    let render = render_title(&params, (640, 360), (640, 360));
    let (x0, ..) = covered_bounds(render.text()).unwrap();
    assert!((x0 as i32 - 320).abs() < 12, "start x {x0}");
}

#[test]
fn background_is_under_the_text_and_contains_it() {
    let params = TitleParams {
        background: vv_core::TitleBackground {
            enabled: true,
            outline_width: 2.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let render = render_title(&params, (640, 360), (640, 360));
    assert_eq!(render.layers.len(), 3, "background, outline, text");
    let (tx0, ty0, tx1, ty1) = covered_bounds(render.text()).unwrap();
    let (bx0, by0, bx1, by1) = covered_bounds(&render.layers[0].0).unwrap();
    assert!(bx0 < tx0 && by0 < ty0 && bx1 > tx1 && by1 > ty1);
    // The border sits on the perimeter: at the center of the rectangle there is none.
    let ring = &render.layers[1].0;
    let (cx, cy) = ((bx0 + bx1) / 2, (by0 + by1) / 2);
    assert_eq!(ring.data[(cy * ring.width + cx) as usize], 0);
    assert!(ring.data[(cy * ring.width + bx0 + 1) as usize] > 0);
}

#[test]
fn output_smaller_than_timeline_scales_the_text() {
    let full = render_title(&TitleParams::default(), (640, 360), (640, 360));
    let half = render_title(&TitleParams::default(), (640, 360), (320, 180));
    let width = |m: &TextMask| {
        let (x0, _, x1, _) = covered_bounds(m).unwrap();
        (x1 - x0) as f32
    };
    let ratio = width(half.text()) / width(full.text());
    assert!((ratio - 0.5).abs() < 0.08, "ratio {ratio}");
}
