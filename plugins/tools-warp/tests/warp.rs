//! Puppet Warp's deformation and Vanishing Point's perspective maths.

use photoslop_tools_warp::perspective::{unit_square_to_quad, Homography};
use photoslop_tools_warp::puppet::{mls_inverse, Pin};

fn close(a: (f32, f32), b: (f32, f32), tol: f32, what: &str) {
    assert!(
        (a.0 - b.0).abs() < tol && (a.1 - b.1).abs() < tol,
        "{what}: {a:?} != {b:?}"
    );
}

fn pin(from: (f32, f32), to: (f32, f32)) -> Pin {
    Pin { from, to }
}

#[test]
fn no_pins_is_the_identity() {
    assert_eq!(mls_inverse(&[], 12.0, 34.0, 1.0), (12.0, 34.0));
}

#[test]
fn pins_left_alone_leave_the_image_alone() {
    let pins = vec![
        pin((0.0, 0.0), (0.0, 0.0)),
        pin((100.0, 0.0), (100.0, 0.0)),
        pin((50.0, 100.0), (50.0, 100.0)),
    ];
    close(
        mls_inverse(&pins, 40.0, 40.0, 1.0),
        (40.0, 40.0),
        0.5,
        "undragged pins deformed the image",
    );
}

#[test]
fn a_point_lands_exactly_on_its_pin() {
    let pins = vec![
        pin((10.0, 10.0), (30.0, 10.0)),
        pin((90.0, 10.0), (90.0, 10.0)),
        pin((50.0, 90.0), (50.0, 90.0)),
    ];
    // Asked where the pin's new position comes from, the answer is where
    // it used to be.
    close(
        mls_inverse(&pins, 30.0, 10.0, 1.0),
        (10.0, 10.0),
        0.5,
        "a pin did not hold",
    );
}

#[test]
fn moving_every_pin_by_the_same_amount_is_a_translation() {
    let pins: Vec<Pin> = [(10.0, 10.0), (90.0, 20.0), (40.0, 80.0)]
        .iter()
        .map(|&(x, y): &(f32, f32)| pin((x, y), (x + 25.0, y - 5.0)))
        .collect();
    close(
        mls_inverse(&pins, 60.0, 50.0, 1.0),
        (35.0, 55.0),
        0.5,
        "a uniform drag was not a translation",
    );
}

#[test]
fn scaling_every_pin_is_a_similarity() {
    let pins: Vec<Pin> = [(0.0, 0.0), (100.0, 0.0), (0.0, 100.0), (100.0, 100.0)]
        .iter()
        .map(|&(x, y): &(f32, f32)| pin((x, y), (x * 2.0, y * 2.0)))
        .collect();
    close(
        mls_inverse(&pins, 100.0, 100.0, 1.0),
        (50.0, 50.0),
        1.0,
        "a uniform scale was not reproduced",
    );
}

#[test]
fn deformation_is_local_to_the_pin_that_moved() {
    let pins = vec![
        pin((10.0, 50.0), (10.0, 50.0)),
        pin((50.0, 50.0), (50.0, 20.0)),
        pin((190.0, 50.0), (190.0, 50.0)),
    ];
    let near = mls_inverse(&pins, 50.0, 25.0, 1.5);
    let far = mls_inverse(&pins, 190.0, 50.0, 1.5);
    assert!(
        (near.1 - 25.0).abs() > 10.0,
        "the dragged pin had no local effect: {near:?}"
    );
    close(far, (190.0, 50.0), 2.0, "a distant held pin moved");
}

#[test]
fn a_degenerate_pin_set_does_not_produce_nans() {
    let pins = vec![pin((5.0, 5.0), (5.0, 5.0)), pin((5.0, 5.0), (5.0, 5.0))];
    let out = mls_inverse(&pins, 40.0, 40.0, 1.0);
    assert!(out.0.is_finite() && out.1.is_finite(), "{out:?}");
}

#[test]
fn the_unit_square_maps_onto_its_quad() {
    let quad = [(10.0, 20.0), (110.0, 30.0), (100.0, 130.0), (20.0, 120.0)];
    let h = unit_square_to_quad(&quad).expect("solvable");
    close(h.apply(0.0, 0.0), quad[0], 0.01, "top-left");
    close(h.apply(1.0, 0.0), quad[1], 0.01, "top-right");
    close(h.apply(1.0, 1.0), quad[2], 0.01, "bottom-right");
    close(h.apply(0.0, 1.0), quad[3], 0.01, "bottom-left");
}

#[test]
fn a_receding_quad_foreshortens() {
    // A trapezoid: narrow at the top, wide at the bottom, like a wall
    // going away from the camera.
    let quad = [(40.0, 10.0), (60.0, 10.0), (100.0, 100.0), (0.0, 100.0)];
    let h = unit_square_to_quad(&quad).expect("solvable");
    // The same step across the plane covers fewer pixels at the far end.
    let near = h.apply(0.6, 1.0).0 - h.apply(0.4, 1.0).0;
    let far = h.apply(0.6, 0.0).0 - h.apply(0.4, 0.0).0;
    assert!(
        far < near * 0.5,
        "no foreshortening: near {near}, far {far}"
    );
}

#[test]
fn a_homography_and_its_inverse_cancel() {
    let quad = [(5.0, 8.0), (120.0, 15.0), (140.0, 90.0), (0.0, 110.0)];
    let h = unit_square_to_quad(&quad).unwrap();
    let inv = h.invert().unwrap();
    for (x, y) in [(0.25f32, 0.25f32), (0.9, 0.1), (0.5, 0.75)] {
        let (px, py) = h.apply(x, y);
        close(inv.apply(px, py), (x, y), 0.001, "round trip");
    }
}

#[test]
fn a_collapsed_quad_is_rejected_rather_than_producing_nans() {
    let quad = [(0.0, 0.0), (10.0, 0.0), (20.0, 0.0), (30.0, 0.0)];
    if let Some(h) = unit_square_to_quad(&quad) {
        let (x, y) = h.apply(0.5, 0.5);
        assert!(x.is_finite() && y.is_finite(), "produced NaN");
    }
}

#[test]
fn the_identity_homography_is_the_identity() {
    assert_eq!(Homography::IDENTITY.apply(7.0, 9.0), (7.0, 9.0));
}
