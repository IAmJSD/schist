//! A stroked open line must be solid along its whole length, including both
//! end caps.

use photoslop_core::IntRect;
use photoslop_vector::{rasterize, stroke_to_path, FillRule, Path};

fn cov(mask: &[u8], rect: IntRect, x: i32, y: i32) -> u8 {
    let w = rect.width() as usize;
    mask[(y - rect.top) as usize * w + (x - rect.left) as usize]
}

#[test]
fn stroked_line_has_no_hole_at_either_cap() {
    let mut line = Path::default();
    line.subpaths.push(vec![(20.0, 50.0), (80.0, 50.0)]);
    let stroked = stroke_to_path(&line, 20.0);

    let rect = IntRect::new(0, 0, 100, 100);
    let mask = rasterize(&stroked, rect, FillRule::NonZero);

    let start = cov(&mask, rect, 20, 50);
    let middle = cov(&mask, rect, 50, 50);
    let end = cov(&mask, rect, 80, 50);
    println!("start cap = {start}, middle = {middle}, end cap = {end}");

    assert_eq!(middle, 255, "shaft should be solid");
    assert_eq!(start, 255, "start cap has a hole");
    assert_eq!(end, 255, "end cap has a hole");
}

#[test]
fn stroking_an_open_polyline_does_not_close_it() {
    let mut poly = Path::default();
    poly.subpaths
        .push(vec![(20.0, 20.0), (20.0, 80.0), (80.0, 80.0)]);
    let stroked = stroke_to_path(&poly, 6.0);

    let rect = IntRect::new(0, 0, 100, 100);
    let mask = rasterize(&stroked, rect, FillRule::NonZero);

    let across = cov(&mask, rect, 50, 50);
    println!("coverage on the phantom closing segment = {across}");
    assert_eq!(across, 0, "open polyline was closed by the stroker");
}
