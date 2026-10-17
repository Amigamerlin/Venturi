use super::*;

#[test]
fn the_name_column_fills_what_the_duration_leaves_unless_resized() {
    assert_eq!(name_col_width(None, 300.0), 300.0 - DURATION_COL_W);
    assert_eq!(name_col_width(Some(150.0), 300.0), 150.0);
    assert_eq!(
        name_col_width(Some(1000.0), 300.0),
        300.0 - DURATION_COL_W,
        "the duration stays inside the panel"
    );
    assert_eq!(name_col_width(Some(10.0), 300.0), MIN_NAME_COL_W);
    assert_eq!(name_col_width(Some(150.0), 100.0), 100.0 - DURATION_COL_W);
}
