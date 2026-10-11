use super::*;

fn block(values: Range<u32>) -> Option<Arc<Vec<f32>>> {
    Some(Arc::new(
        values.flat_map(|v| [v as f32, -(v as f32)]).collect(),
    ))
}

/// Stereo, blocks of 4 frames from frame 8, the second one missing, ending
/// at frame 18.
fn with_hole() -> SampleBlocks {
    SampleBlocks::new(8, 4, 2, vec![block(8..12), None, block(16..18)], 18)
}

#[test]
fn a_run_goes_to_the_end_of_its_block() {
    let blocks = with_hole();
    let Run::Samples(samples) = blocks.run(9) else {
        panic!("frame 9 is decoded");
    };
    assert_eq!(samples, [9.0, -9.0, 10.0, -10.0, 11.0, -11.0]);
    assert!(matches!(blocks.run(13), Run::Silence(3)));
    assert!(matches!(blocks.run(17), Run::Samples([17.0, -17.0])));
    assert!(matches!(blocks.run(18), Run::End));
    assert!(matches!(blocks.run(7), Run::End));
}

#[test]
fn a_frame_is_none_in_a_hole_and_outside() {
    let blocks = with_hole();
    assert_eq!(blocks.frame(16), Some(&[16.0, -16.0][..]));
    assert_eq!(blocks.frame(12), None);
    assert_eq!(blocks.frame(30), None);
}

#[test]
fn to_vec_fills_holes_and_the_tail_with_silence() {
    let samples = with_hole().to_vec(10..20);
    let expected: Vec<f32> = [10.0, 11.0, 0.0, 0.0, 0.0, 0.0, 16.0, 17.0]
        .iter()
        .flat_map(|&v: &f32| [v, if v == 0.0 { 0.0 } else { -v }])
        .collect();
    assert_eq!(samples, expected, "stops at `end`, frame 18");
}

#[test]
fn contiguous_is_one_block_from_its_start() {
    let blocks = SampleBlocks::contiguous(Arc::new(vec![1.0, 2.0, 3.0]), 5, 1);
    assert_eq!((blocks.start(), blocks.end()), (5, 8));
    assert_eq!(blocks.frame(7), Some(&[3.0][..]));
}
