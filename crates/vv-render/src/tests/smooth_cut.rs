use super::*;

#[test]
fn the_flow_is_estimated_at_most_640_wide_down_to_small_levels() {
    let sizes = level_sizes((1920, 1080));
    assert_eq!(sizes[0], (640, 360));
    assert_eq!(sizes[1], (320, 180));
    assert!(sizes.last().unwrap().1 >= MIN_LEVEL_SIDE);
    assert!(sizes.len() <= MAX_LEVELS);
}

#[test]
fn a_small_output_is_never_upscaled_for_the_flow() {
    assert_eq!(level_sizes((160, 96))[0], (160, 96));
}
