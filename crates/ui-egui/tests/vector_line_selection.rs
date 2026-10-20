//! Regression tests for vector line selection primitives.

use pdfcraft_edit::LineSegment;

#[test]
fn line_bbox_and_length_are_consistent() {
    let l = LineSegment { page: 0, index: 0, start: [0.0, 0.0], end: [3.0, 4.0], thickness: 1.0 };
    assert_eq!(l.bbox(), [0.0, 0.0, 3.0, 4.0]);
    assert!((l.length() - 5.0).abs() < 1e-9);
}

#[test]
fn distance_sq_to_handles_degenerate_segment() {
    let l = LineSegment { page: 0, index: 0, start: [2.0, 2.0], end: [2.0, 2.0], thickness: 1.0 };
    assert!((l.distance_sq_to([2.0, 2.0])).abs() < 1e-9);
    assert!((l.distance_sq_to([5.0, 2.0]) - 9.0).abs() < 1e-9);
}

