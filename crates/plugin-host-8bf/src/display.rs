//! `displayPixels`: drawing a plug-in's preview.
//!
//! A filter with a preview pane builds a [`PSPixelMap`] over its own
//! working pixels and asks the *host* to put them on screen — the host
//! owns the colour management, so it is the one that knows how. This is
//! not an optional nicety: every FilterMeister-built plug-in refuses to
//! run at all unless `displayPixels` is present, which between Harry's
//! Filters, Plugin Galaxy and the rest is a large slice of the freeware
//! world.
//!
//! The conversion is a pure function over the pixel map, so it is
//! testable anywhere; only the final blit is platform code.

use crate::abi::{mask_description, PSPixelMap, PSPixelMask, VRect};

/// Image modes this can draw. The API Guide's own words: "Nonsuccess is
/// generally due to unsupported color modes."
const MODE_GRAYSCALE: i32 = crate::abi::mode::GRAY_SCALE as i32;
const MODE_RGB: i32 = crate::abi::mode::RGB_COLOR as i32;

/// A rectangle of 32-bit BGRX pixels, top row first — the layout a
/// Windows top-down DIB wants, and 32 bits per pixel so rows need no
/// padding to a four-byte boundary.
pub struct Surface {
    pub width: i32,
    pub height: i32,
    pub bgrx: Vec<u8>,
}

/// Read `src_rect` out of `map` into a drawable surface.
///
/// Returns `None` for a mode this cannot draw, a malformed map, or a
/// rectangle outside the map's bounds — all of which the caller reports
/// to the plug-in as an error rather than guessing.
///
/// # Safety
///
/// `map` must describe a live buffer: `base_addr` valid for the strides
/// and bounds it declares.
pub unsafe fn read_surface(map: &PSPixelMap, src_rect: VRect) -> Option<Surface> {
    let planes = match map.image_mode {
        MODE_GRAYSCALE => 1usize,
        MODE_RGB => 3,
        _ => return None,
    };
    if map.base_addr.is_null() || src_rect.is_empty() {
        return None;
    }
    // A rectangle outside the map is the plug-in's bug, not something to
    // paper over by clamping: it would draw the wrong pixels silently.
    if src_rect.top < map.bounds.top
        || src_rect.left < map.bounds.left
        || src_rect.bottom > map.bounds.bottom
        || src_rect.right > map.bounds.right
    {
        return None;
    }

    let (w, h) = (src_rect.width(), src_rect.height());
    let base = map.base_addr as *const u8;
    let (row_bytes, col_bytes, plane_bytes) = (
        map.row_bytes as isize,
        map.col_bytes as isize,
        map.plane_bytes as isize,
    );

    // The matting mask, if any, says the colour the data was composited
    // against so the preview can show it un-fringed. Anything in the
    // `masks` chain is a selection mask and does not affect drawing.
    let matte = mat_colour(map.mat);

    let mut bgrx = vec![0u8; (w as usize) * (h as usize) * 4];
    for y in 0..h {
        let sy = (src_rect.top + y - map.bounds.top) as isize;
        for x in 0..w {
            let sx = (src_rect.left + x - map.bounds.left) as isize;
            let px = base.offset(sy * row_bytes + sx * col_bytes);
            let (r, g, b) = if planes == 1 {
                let v = *px;
                (v, v, v)
            } else {
                (*px, *px.offset(plane_bytes), *px.offset(plane_bytes * 2))
            };
            let (r, g, b) = match matte {
                Some(m) => dematte(map, sx, sy, (r, g, b), m),
                None => (r, g, b),
            };
            let o = ((y as usize) * (w as usize) + x as usize) * 4;
            bgrx[o] = b;
            bgrx[o + 1] = g;
            bgrx[o + 2] = r;
            bgrx[o + 3] = 0xff;
        }
    }
    Some(Surface {
        width: w,
        height: h,
        bgrx,
    })
}

/// The constant a matting mask says the colour was composited against.
///
/// # Safety
///
/// `mat` is either null or a live [`PSPixelMask`].
unsafe fn mat_colour(mat: *const PSPixelMask) -> Option<(u8, *const PSPixelMask)> {
    let m = mat.as_ref()?;
    let constant = match m.mask_description {
        mask_description::BLACK_MAT => 0u8,
        mask_description::GRAY_MAT => 128,
        mask_description::WHITE_MAT => 255,
        // A plain or inverted mask describes coverage, not matting.
        _ => return None,
    };
    Some((constant, mat))
}

/// Undo the matte, so a preview of transparent edges does not show the
/// colour they were composited against.
///
/// The API Guide gives the forward operation as
/// `matted = unmatted*a + constant*(1-a)`; this is that, inverted, with
/// a fully transparent pixel left at the matte colour because nothing
/// can be recovered from it.
///
/// # Safety
///
/// `mask` points at a live [`PSPixelMask`] with valid strides.
unsafe fn dematte(
    map: &PSPixelMap,
    sx: isize,
    sy: isize,
    (r, g, b): (u8, u8, u8),
    (constant, mask): (u8, *const PSPixelMask),
) -> (u8, u8, u8) {
    let m = &*mask;
    if m.mask_data.is_null() {
        return (r, g, b);
    }
    let phase_row = (sy + map.mask_phase_row as isize) * m.row_bytes as isize;
    let phase_col = (sx + map.mask_phase_col as isize) * m.col_bytes as isize;
    let alpha = *(m.mask_data as *const u8).offset(phase_row + phase_col);
    if alpha == 0 {
        return (constant, constant, constant);
    }
    let un = |v: u8| {
        let v = v as i32;
        let c = constant as i32;
        let out = c + ((v - c) * 255) / alpha as i32;
        out.clamp(0, 255) as u8
    };
    (un(r), un(g), un(b))
}

/// Put a surface on screen at `(dst_col, dst_row)` in the device
/// context. "The display routines do not scale the pixels", so this is
/// always 1:1.
///
/// # Safety
///
/// `hdc` must be a valid device context.
#[cfg(windows)]
pub unsafe fn blit(hdc: usize, dst_row: i32, dst_col: i32, surface: &Surface) -> bool {
    #[repr(C)]
    struct BitmapInfoHeader {
        size: u32,
        width: i32,
        height: i32,
        planes: u16,
        bit_count: u16,
        compression: u32,
        size_image: u32,
        x_pels_per_meter: i32,
        y_pels_per_meter: i32,
        clr_used: u32,
        clr_important: u32,
    }

    const BI_RGB: u32 = 0;
    const DIB_RGB_COLORS: u32 = 0;
    const SRCCOPY: u32 = 0x00CC_0020;

    #[link(name = "gdi32")]
    extern "system" {
        #[allow(clippy::too_many_arguments)]
        fn StretchDIBits(
            hdc: usize,
            x_dest: i32,
            y_dest: i32,
            dest_width: i32,
            dest_height: i32,
            x_src: i32,
            y_src: i32,
            src_width: i32,
            src_height: i32,
            bits: *const std::ffi::c_void,
            bmi: *const BitmapInfoHeader,
            usage: u32,
            rop: u32,
        ) -> i32;
    }

    let header = BitmapInfoHeader {
        size: std::mem::size_of::<BitmapInfoHeader>() as u32,
        width: surface.width,
        // Negative means top-down, which is the order read_surface
        // produces and saves flipping every preview.
        height: -surface.height,
        planes: 1,
        bit_count: 32,
        compression: BI_RGB,
        size_image: surface.bgrx.len() as u32,
        x_pels_per_meter: 0,
        y_pels_per_meter: 0,
        clr_used: 0,
        clr_important: 0,
    };
    let drawn = StretchDIBits(
        hdc,
        dst_col,
        dst_row,
        surface.width,
        surface.height,
        0,
        0,
        surface.width,
        surface.height,
        surface.bgrx.as_ptr() as *const std::ffi::c_void,
        &header,
        DIB_RGB_COLORS,
        SRCCOPY,
    );
    drawn != 0
}

/// Off Windows there is no device context to draw into, so the pixels
/// are read and dropped. That still exercises everything except the
/// final call, which is what the tests need.
///
/// # Safety
///
/// Trivially safe; the signature matches the Windows one.
#[cfg(not(windows))]
pub unsafe fn blit(_hdc: usize, _dst_row: i32, _dst_col: i32, _surface: &Surface) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;

    /// Build a map over `data` laid out the way a plug-in's own working
    /// buffer usually is: interleaved, one row after another.
    fn interleaved(data: &mut [u8], w: i32, h: i32, planes: i32, mode: i32) -> PSPixelMap {
        PSPixelMap {
            version: 1,
            bounds: VRect {
                top: 0,
                left: 0,
                bottom: h,
                right: w,
            },
            image_mode: mode,
            row_bytes: w * planes,
            col_bytes: planes,
            plane_bytes: 1,
            base_addr: data.as_mut_ptr() as *mut c_void,
            mat: std::ptr::null_mut(),
            masks: std::ptr::null_mut(),
            mask_phase_row: 0,
            mask_phase_col: 0,
        }
    }

    #[test]
    fn rgb_planes_land_in_bgrx_order() {
        let mut data = vec![10u8, 20, 30, 40, 50, 60];
        let map = interleaved(&mut data, 2, 1, 3, MODE_RGB);
        let s = unsafe { read_surface(&map, map.bounds).unwrap() };
        assert_eq!(s.bgrx, vec![30, 20, 10, 255, 60, 50, 40, 255]);
    }

    #[test]
    fn grayscale_replicates_across_the_channels() {
        let mut data = vec![7u8, 200];
        let map = interleaved(&mut data, 2, 1, 1, MODE_GRAYSCALE);
        let s = unsafe { read_surface(&map, map.bounds).unwrap() };
        assert_eq!(s.bgrx, vec![7, 7, 7, 255, 200, 200, 200, 255]);
    }

    #[test]
    fn a_sub_rectangle_reads_from_the_right_place() {
        // 3x2 RGB, so a wrong row stride or a wrong origin both show.
        let mut data: Vec<u8> = (0..18).collect();
        let map = interleaved(&mut data, 3, 2, 3, MODE_RGB);
        let rect = VRect {
            top: 1,
            left: 1,
            bottom: 2,
            right: 3,
        };
        let s = unsafe { read_surface(&map, rect).unwrap() };
        assert_eq!(s.width, 2);
        assert_eq!(s.height, 1);
        // Row 1 starts at byte 9; pixel (1,1) is bytes 12,13,14.
        assert_eq!(s.bgrx, vec![14, 13, 12, 255, 17, 16, 15, 255]);
    }

    #[test]
    fn planar_layouts_are_honoured() {
        // Three separate planes rather than interleaved: colBytes 1,
        // planeBytes one whole plane. A host that assumes interleaving
        // produces stripes.
        let mut data = vec![1u8, 2, /* R */ 3, 4, /* G */ 5, 6 /* B */];
        let map = PSPixelMap {
            col_bytes: 1,
            plane_bytes: 2,
            row_bytes: 2,
            ..interleaved(&mut data, 2, 1, 3, MODE_RGB)
        };
        let s = unsafe { read_surface(&map, map.bounds).unwrap() };
        assert_eq!(s.bgrx, vec![5, 3, 1, 255, 6, 4, 2, 255]);
    }

    #[test]
    fn an_unsupported_mode_is_refused_rather_than_drawn_wrong() {
        let mut data = vec![0u8; 16];
        let map = interleaved(&mut data, 2, 2, 4, crate::abi::mode::CMYK_COLOR as i32);
        assert!(unsafe { read_surface(&map, map.bounds) }.is_none());
    }

    #[test]
    fn a_rectangle_outside_the_map_is_refused() {
        let mut data = vec![0u8; 12];
        let map = interleaved(&mut data, 2, 2, 3, MODE_RGB);
        let past = VRect {
            top: 0,
            left: 0,
            bottom: 3,
            right: 2,
        };
        assert!(unsafe { read_surface(&map, past) }.is_none());
    }

    #[test]
    fn a_white_matte_is_undone() {
        // One pixel, half covered, composited against white.
        let mut data = vec![200u8, 200, 200];
        let mut alpha = vec![128u8];
        let mask = PSPixelMask {
            next: std::ptr::null_mut(),
            mask_data: alpha.as_mut_ptr() as *mut c_void,
            row_bytes: 1,
            col_bytes: 1,
            mask_description: mask_description::WHITE_MAT,
        };
        let map = PSPixelMap {
            mat: &mask as *const _ as *mut PSPixelMask,
            ..interleaved(&mut data, 1, 1, 3, MODE_RGB)
        };
        let s = unsafe { read_surface(&map, map.bounds).unwrap() };
        // 255 + (200-255)*255/128 = 255 - 109 = 146.
        assert_eq!(&s.bgrx[..3], &[146, 146, 146]);
    }
}
