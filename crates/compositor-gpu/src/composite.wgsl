// Tile compositing as a per-pixel stack machine.
//
// The plan builder walks the layer tree once on the CPU and flattens it to
// an op program; every pixel of every tile in the batch then executes that
// program in a single dispatch, keeping its own small stack of RGBA values.
// Compositing has no cross-pixel dependencies, so this maps exactly.
//
// The math here mirrors schist-pixel-ops line for line — same formulas,
// same operand order, same guards — because the CPU compositor is the
// semantic contract and the parity tests hold this shader to it.

const TILE: u32 = 256u;
const TILE_PIXELS: u32 = 65536u;
const MAX_DEPTH: u32 = 12u;

// Op kinds.
const OP_PUSH_LAYER: u32 = 0u;
const OP_PUSH_BLANK: u32 = 1u;
const OP_BLEND: u32 = 2u;
const OP_CLIP_BLEND: u32 = 3u;
const OP_SNAPSHOT_ALPHA: u32 = 4u;
const OP_ADJUST: u32 = 5u;
const OP_MASK_TOP: u32 = 6u;

// BlendMode discriminants, in schist-core enum order.
const M_PASS_THROUGH: u32 = 0u;
const M_NORMAL: u32 = 1u;
const M_DISSOLVE: u32 = 2u;
const M_DARKEN: u32 = 3u;
const M_MULTIPLY: u32 = 4u;
const M_COLOR_BURN: u32 = 5u;
const M_LINEAR_BURN: u32 = 6u;
const M_DARKER_COLOR: u32 = 7u;
const M_LIGHTEN: u32 = 8u;
const M_SCREEN: u32 = 9u;
const M_COLOR_DODGE: u32 = 10u;
const M_LINEAR_DODGE: u32 = 11u;
const M_LIGHTER_COLOR: u32 = 12u;
const M_OVERLAY: u32 = 13u;
const M_SOFT_LIGHT: u32 = 14u;
const M_HARD_LIGHT: u32 = 15u;
const M_VIVID_LIGHT: u32 = 16u;
const M_LINEAR_LIGHT: u32 = 17u;
const M_PIN_LIGHT: u32 = 18u;
const M_HARD_MIX: u32 = 19u;
const M_DIFFERENCE: u32 = 20u;
const M_EXCLUSION: u32 = 21u;
const M_SUBTRACT: u32 = 22u;
const M_DIVIDE: u32 = 23u;
const M_HUE: u32 = 24u;
const M_SATURATION: u32 = 25u;
const M_COLOR: u32 = 26u;
const M_LUMINOSITY: u32 = 27u;

// Adjust flags.
const F_CONFINE: u32 = 1u;
const F_FILL: u32 = 2u;

// Full-colour adjustment kinds (plan::D_*).
const D_NONE: u32 = 0u;
const D_HUE_SATURATION: u32 = 1u;
const D_BLACK_WHITE: u32 = 2u;
const D_THRESHOLD: u32 = 3u;
const D_POSTERIZE: u32 = 4u;
const DIRECT_STRIDE: u32 = 6u;

// Source formats (matches TileBuf variants).
const FMT_U8: u32 = 0u;
const FMT_U16: u32 = 1u;
const FMT_F32: u32 = 2u;

struct Op {
    kind: u32,
    mode: u32,
    opacity: f32,
    flags: u32,
    src_ref: i32,
    src_fmt: u32,
    mask_ref: i32,
    lut: i32,
    mask_bounds: vec4<i32>, // left, top, right, bottom
    fill: vec4<f32>,
    mask_default: f32,
    direct: u32,
    dparams: i32,
    _p0: u32,
}

struct Globals {
    n_ops: u32,
    n_tiles: u32,
    _p0: u32,
    _p1: u32,
}

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var<storage, read> ops: array<Op>;
@group(0) @binding(2) var<storage, read> tile_origin: array<vec2<i32>>;
@group(0) @binding(3) var<storage, read> src_words: array<u32>;
@group(0) @binding(4) var<storage, read> mask_words: array<u32>;
@group(0) @binding(5) var<storage, read> slots: array<i32>;
@group(0) @binding(6) var<storage, read> luts: array<f32>;
@group(0) @binding(7) var<storage, read_write> out_f32: array<f32>;
@group(0) @binding(8) var<storage, read> dparams: array<f32>;

// ---- pixel-ops mirror ----

fn mulc(b: f32, s: f32) -> f32 {
    return b * s;
}

fn screenc(b: f32, s: f32) -> f32 {
    return b + s - b * s;
}

fn hard_light(b: f32, s: f32) -> f32 {
    if (s <= 0.5) {
        return mulc(b, 2.0 * s);
    }
    return screenc(b, 2.0 * s - 1.0);
}

fn color_dodge(b: f32, s: f32) -> f32 {
    if (b <= 0.0) {
        return 0.0;
    } else if (s >= 1.0) {
        return 1.0;
    }
    return min(b / (1.0 - s), 1.0);
}

fn color_burn(b: f32, s: f32) -> f32 {
    if (b >= 1.0) {
        return 1.0;
    } else if (s <= 0.0) {
        return 0.0;
    }
    return 1.0 - min((1.0 - b) / s, 1.0);
}

fn soft_light(b: f32, s: f32) -> f32 {
    if (s <= 0.5) {
        return b - (1.0 - 2.0 * s) * b * (1.0 - b);
    }
    var d: f32;
    if (b <= 0.25) {
        d = ((16.0 * b - 12.0) * b + 4.0) * b;
    } else {
        d = sqrt(b);
    }
    return b + (2.0 * s - 1.0) * (d - b);
}

fn separable(mode: u32, b: f32, s: f32) -> f32 {
    var v: f32;
    switch mode {
        case 3u: { v = min(b, s); }                       // Darken
        case 4u: { v = mulc(b, s); }                      // Multiply
        case 5u: { v = color_burn(b, s); }                // ColorBurn
        case 6u: { v = b + s - 1.0; }                     // LinearBurn
        case 8u: { v = max(b, s); }                       // Lighten
        case 9u: { v = screenc(b, s); }                   // Screen
        case 10u: { v = color_dodge(b, s); }              // ColorDodge
        case 11u: { v = b + s; }                          // LinearDodge
        case 13u: { v = hard_light(s, b); }               // Overlay
        case 14u: { v = soft_light(b, s); }               // SoftLight
        case 15u: { v = hard_light(b, s); }               // HardLight
        case 16u: {                                       // VividLight
            if (s <= 0.5) {
                v = color_burn(b, 2.0 * s);
            } else {
                v = color_dodge(b, 2.0 * s - 1.0);
            }
        }
        case 17u: { v = b + 2.0 * s - 1.0; }              // LinearLight
        case 18u: {                                       // PinLight
            if (s <= 0.5) {
                v = min(b, 2.0 * s);
            } else {
                v = max(b, 2.0 * s - 1.0);
            }
        }
        case 19u: {                                       // HardMix
            if (b + s >= 1.0) {
                v = 1.0;
            } else {
                v = 0.0;
            }
        }
        case 20u: { v = abs(b - s); }                     // Difference
        case 21u: { v = b + s - 2.0 * b * s; }            // Exclusion
        case 22u: { v = b - s; }                          // Subtract
        case 23u: {                                       // Divide
            if (s <= 0.0) {
                v = 1.0;
            } else {
                v = b / s;
            }
        }
        default: { v = s; }                               // Normal/PassThrough/…
    }
    return clamp(v, 0.0, 1.0);
}

fn lum3(c: vec3<f32>) -> f32 {
    return 0.3 * c.x + 0.59 * c.y + 0.11 * c.z;
}

fn clip_color(c: vec3<f32>) -> vec3<f32> {
    let l = lum3(c);
    let n = min(c.x, min(c.y, c.z));
    let x = max(c.x, max(c.y, c.z));
    var out = c;
    if (n < 0.0) {
        out = vec3(l) + (out - vec3(l)) * (l / (l - n));
    }
    if (x > 1.0) {
        out = vec3(l) + (out - vec3(l)) * ((1.0 - l) / (x - l));
    }
    return out;
}

fn set_lum(c: vec3<f32>, l: f32) -> vec3<f32> {
    let d = l - lum3(c);
    return clip_color(c + vec3(d));
}

fn sat3(c: vec3<f32>) -> f32 {
    return max(c.x, max(c.y, c.z)) - min(c.x, min(c.y, c.z));
}

fn set_sat(c: vec3<f32>, s: f32) -> vec3<f32> {
    // Stable 3-element sort of channel indices by value, matching the CPU
    // reference's sort_by.
    var cc = c;
    var i0 = 0u;
    var i1 = 1u;
    var i2 = 2u;
    if (cc[i0] > cc[i1]) { let t = i0; i0 = i1; i1 = t; }
    if (cc[i1] > cc[i2]) { let t = i1; i1 = i2; i2 = t; }
    if (cc[i0] > cc[i1]) { let t = i0; i0 = i1; i1 = t; }
    var out = vec3(0.0);
    if (cc[i2] > cc[i0]) {
        out[i1] = (cc[i1] - cc[i0]) * s / (cc[i2] - cc[i0]);
        out[i2] = s;
    }
    return out;
}

fn blend_color(mode: u32, cb: vec3<f32>, cs: vec3<f32>) -> vec3<f32> {
    switch mode {
        case 24u: { return set_lum(set_sat(cs, sat3(cb)), lum3(cb)); }  // Hue
        case 25u: { return set_lum(set_sat(cb, sat3(cs)), lum3(cb)); }  // Saturation
        case 26u: { return set_lum(cs, lum3(cb)); }                     // Color
        case 27u: { return set_lum(cb, lum3(cs)); }                     // Luminosity
        case 7u: {                                                      // DarkerColor
            if (lum3(cs) < lum3(cb)) { return cs; }
            return cb;
        }
        case 12u: {                                                     // LighterColor
            if (lum3(cs) > lum3(cb)) { return cs; }
            return cb;
        }
        default: {
            return vec3(
                separable(mode, cb.x, cs.x),
                separable(mode, cb.y, cs.y),
                separable(mode, cb.z, cs.z),
            );
        }
    }
}

fn dissolve_hash(x: i32, y: i32) -> f32 {
    var h = (u32(x) * 0x9E3779B9u) ^ (u32(y) * 0x85EBCA6Bu);
    h ^= h >> 16u;
    h *= 0x7FEB352Du;
    h ^= h >> 15u;
    h *= 0x846CA68Bu;
    h ^= h >> 16u;
    return f32(h >> 8u) / 16777216.0;
}

fn over(top: vec4<f32>, bottom: vec4<f32>) -> vec4<f32> {
    let a = top.a + bottom.a * (1.0 - top.a);
    if (a <= 1.1920929e-7) {
        return vec4(0.0);
    }
    let c = (top.rgb * top.a + bottom.rgb * bottom.a * (1.0 - top.a)) / a;
    return vec4(c, a);
}

fn blend_px(mode: u32, top: vec4<f32>, bottom: vec4<f32>, x: i32, y: i32) -> vec4<f32> {
    if (mode == M_DISSOLVE) {
        // Dissolve: source shown opaque with probability = source alpha.
        if (top.a > dissolve_hash(x, y)) {
            return over(vec4(top.rgb, 1.0), bottom);
        }
        return bottom;
    }
    let a_s = top.a;
    let a_b = bottom.a;
    if (a_s <= 0.0) {
        return bottom;
    }
    let bl = blend_color(mode, bottom.rgb, top.rgb);
    let a_o = a_s + a_b * (1.0 - a_s);
    if (a_o <= 0.0) {
        return vec4(0.0);
    }
    let co = (top.rgb * a_s * (1.0 - a_b) + bl * a_s * a_b + bottom.rgb * a_b * (1.0 - a_s)) / a_o;
    return vec4(co, a_o);
}

// ---- sources ----

fn src_texel(row: i32, fmt: u32, tile: u32, px: u32) -> vec4<f32> {
    if (row < 0) {
        return vec4(0.0);
    }
    let off = slots[u32(row) * globals.n_tiles + tile];
    if (off < 0) {
        return vec4(0.0);
    }
    let base = u32(off);
    switch fmt {
        case 0u: {
            let w = src_words[base + px];
            return vec4(
                f32(w & 0xFFu),
                f32((w >> 8u) & 0xFFu),
                f32((w >> 16u) & 0xFFu),
                f32((w >> 24u) & 0xFFu),
            ) / 255.0;
        }
        case 1u: {
            let w0 = src_words[base + px * 2u];
            let w1 = src_words[base + px * 2u + 1u];
            return vec4(
                f32(w0 & 0xFFFFu),
                f32(w0 >> 16u),
                f32(w1 & 0xFFFFu),
                f32(w1 >> 16u),
            ) / 65535.0;
        }
        default: {
            let b = base + px * 4u;
            return vec4(
                bitcast<f32>(src_words[b]),
                bitcast<f32>(src_words[b + 1u]),
                bitcast<f32>(src_words[b + 2u]),
                bitcast<f32>(src_words[b + 3u]),
            );
        }
    }
}

// LayerMask::value: default outside bounds, stored tiles (0 when sparse)
// inside.
fn mask_value(op_i: u32, tile: u32, x: i32, y: i32, px: u32) -> f32 {
    let op = ops[op_i];
    if (op.mask_ref < 0) {
        return 1.0;
    }
    let b = op.mask_bounds;
    if (x < b.x || y < b.y || x >= b.z || y >= b.w) {
        return op.mask_default;
    }
    let off = slots[u32(op.mask_ref) * globals.n_tiles + tile];
    if (off < 0) {
        return 0.0;
    }
    let w = mask_words[u32(off) + px / 4u];
    return f32((w >> ((px % 4u) * 8u)) & 0xFFu) / 255.0;
}

fn sample_lut(base: u32, v: f32) -> f32 {
    let x = clamp(v, 0.0, 1.0) * 255.0;
    let i = u32(x);
    if (i >= 255u) {
        return luts[base + 255u];
    }
    // Linear interpolation keeps 16/32-bit inputs from banding.
    let f = x - f32(i);
    let a = luts[base + i];
    return a + (luts[base + i + 1u] - a) * f;
}

fn apply_lut(lut: i32, c: vec3<f32>) -> vec3<f32> {
    let base = u32(lut) * 768u;
    return vec3(
        sample_lut(base, c.x),
        sample_lut(base + 256u, c.y),
        sample_lut(base + 512u, c.z),
    );
}

// ---- full-colour adjustments ----
//
// A mirror of `Params::apply` for the four kinds no per-channel LUT can
// express that the shader models. The coefficients arrive pre-scaled
// (the /100 divisions happen on the CPU), so what is left here is the
// same arithmetic in the same order the reference runs.

fn rem_euclid_f(a: f32, b: f32) -> f32 {
    let r = a % b;
    if (r < 0.0) {
        return r + b;
    }
    return r;
}

fn rgb_to_hsl(c: vec3<f32>) -> vec3<f32> {
    let mx = max(c.r, max(c.g, c.b));
    let mn = min(c.r, min(c.g, c.b));
    let l = (mx + mn) / 2.0;
    if (abs(mx - mn) < 1e-6) {
        return vec3(0.0, 0.0, l);
    }
    let d = mx - mn;
    var s: f32;
    if (l > 0.5) {
        s = d / (2.0 - mx - mn);
    } else {
        s = d / (mx + mn);
    }
    var h: f32;
    if (mx == c.r) {
        h = 60.0 * (((c.g - c.b) / d) % 6.0);
    } else if (mx == c.g) {
        h = 60.0 * ((c.b - c.r) / d + 2.0);
    } else {
        h = 60.0 * ((c.r - c.g) / d + 4.0);
    }
    return vec3(rem_euclid_f(h, 360.0), s, l);
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> vec3<f32> {
    if (s <= 1e-6) {
        return vec3(l, l, l);
    }
    let c = (1.0 - abs(2.0 * l - 1.0)) * s;
    let hp = rem_euclid_f(h, 360.0) / 60.0;
    let x = c * (1.0 - abs(hp % 2.0 - 1.0));
    var rgb: vec3<f32>;
    switch u32(hp) {
        case 0u: { rgb = vec3(c, x, 0.0); }
        case 1u: { rgb = vec3(x, c, 0.0); }
        case 2u: { rgb = vec3(0.0, c, x); }
        case 3u: { rgb = vec3(0.0, x, c); }
        case 4u: { rgb = vec3(x, 0.0, c); }
        default: { rgb = vec3(c, 0.0, x); }
    }
    return clamp(rgb + vec3(l - c / 2.0), vec3(0.0), vec3(1.0));
}

// `amount` is already the /100 fraction.
fn adjust_lightness(l: f32, amount: f32) -> f32 {
    var v: f32;
    if (amount >= 0.0) {
        v = l + (1.0 - l) * amount;
    } else {
        v = l * (1.0 + amount);
    }
    return clamp(v, 0.0, 1.0);
}

// Photoshop's six-slider mono mix: weight the two colour regions the
// pixel's channel ordering places it between.
fn black_white(base: u32, c: vec3<f32>) -> vec3<f32> {
    let reds = dparams[base];
    let yellows = dparams[base + 1u];
    let greens = dparams[base + 2u];
    let cyans = dparams[base + 3u];
    let blues = dparams[base + 4u];
    let magentas = dparams[base + 5u];
    let r = c.r;
    let g = c.g;
    let b = c.b;
    let mx = max(r, max(g, b));
    let mn = min(r, min(g, b));
    let mid = r + g + b - mx - mn;
    var gray: f32;
    if (mx <= mn + 1e-6) {
        gray = mx;
    } else {
        let t = (mid - mn) / (mx - mn);
        var lo: f32;
        var hi: f32;
        if (r >= g && g >= b) {
            lo = reds;
            hi = yellows;
        } else if (g >= r && r >= b) {
            lo = greens;
            hi = yellows;
        } else if (g >= b && b >= r) {
            lo = greens;
            hi = cyans;
        } else if (b >= g && g >= r) {
            lo = blues;
            hi = cyans;
        } else if (b >= r && r >= g) {
            lo = blues;
            hi = magentas;
        } else {
            lo = reds;
            hi = magentas;
        }
        gray = mn + (mx - mn) * (lo * (1.0 - t) + hi * t);
    }
    let v = clamp(gray, 0.0, 1.0);
    return vec3(v, v, v);
}

fn apply_direct(kind: u32, base: u32, c: vec3<f32>) -> vec3<f32> {
    switch kind {
        case D_HUE_SATURATION: {
            let hue = dparams[base];
            let saturation = dparams[base + 1u];
            let lightness = dparams[base + 2u];
            let colorize = dparams[base + 3u] != 0.0;
            let lightness_desaturates = dparams[base + 4u] != 0.0;
            let reciprocal_saturation = dparams[base + 5u] != 0.0;
            let hsl = rgb_to_hsl(c);
            // Affinity's lightness slider flattens colour as it lifts,
            // and its saturation slider boosts reciprocally. Both are
            // off for our own (Photoshop-style) sliders.
            var desat = 1.0;
            if (lightness_desaturates) {
                desat = clamp(1.0 - abs(lightness), 0.0, 1.0);
            }
            var shifted = hsl.y * (1.0 + saturation);
            if (reciprocal_saturation && saturation > 0.0) {
                shifted = hsl.y / max(1.0 - saturation, 0.02);
            }
            var nh: f32;
            var ns: f32;
            if (colorize) {
                nh = rem_euclid_f(hue, 360.0);
                ns = clamp(saturation, 0.0, 1.0);
            } else {
                nh = rem_euclid_f(hsl.x + hue, 360.0);
                ns = clamp(shifted * desat, 0.0, 1.0);
            }
            return hsl_to_rgb(nh, ns, adjust_lightness(hsl.z, lightness));
        }
        case D_BLACK_WHITE: {
            return black_white(base, c);
        }
        case D_THRESHOLD: {
            let lum = 0.3 * c.r + 0.59 * c.g + 0.11 * c.b;
            if (lum >= dparams[base]) {
                return vec3(1.0);
            }
            return vec3(0.0);
        }
        case D_POSTERIZE: {
            // floor into n equal input bands, outputs over the full
            // range — the CPU's (and Photoshop's, and Affinity's)
            // convention.
            let n = dparams[base];
            return clamp(
                vec3(
                    min(floor(c.r * n), n - 1.0) / (n - 1.0),
                    min(floor(c.g * n), n - 1.0) / (n - 1.0),
                    min(floor(c.b * n), n - 1.0) / (n - 1.0),
                ),
                vec3(0.0),
                vec3(1.0),
            );
        }
        default: {
            return c;
        }
    }
}

// ---- the program interpreter ----

@compute @workgroup_size(16, 16, 1)
fn composite(@builtin(global_invocation_id) gid: vec3<u32>) {
    let tile = gid.z;
    if (tile >= globals.n_tiles) {
        return;
    }
    let lx = gid.x;
    let ly = gid.y;
    let px = ly * TILE + lx;
    let orig = tile_origin[tile];
    let x = orig.x + i32(lx);
    let y = orig.y + i32(ly);

    var stack: array<vec4<f32>, MAX_DEPTH>;
    var snap: array<f32, MAX_DEPTH>;
    var sp: u32 = 1u;
    stack[0] = vec4(0.0);

    for (var i = 0u; i < globals.n_ops; i++) {
        let op = ops[i];
        switch op.kind {
            case 0u: { // PushLayer: source pixels, mask folded into alpha
                var v = src_texel(op.src_ref, op.src_fmt, tile, px);
                v.a = v.a * mask_value(i, tile, x, y, px);
                stack[sp] = v;
                sp += 1u;
            }
            case 1u: { // PushBlank
                stack[sp] = vec4(0.0);
                sp += 1u;
            }
            case 2u: { // Blend: pop, blend onto below
                sp -= 1u;
                let s = stack[sp];
                let a = s.a * op.opacity * mask_value(i, tile, x, y, px);
                if (a > 0.0 || op.mode == M_DISSOLVE) {
                    stack[sp - 1u] = blend_px(op.mode, vec4(s.rgb, a), stack[sp - 1u], x, y);
                }
            }
            case 3u: { // ClipBlend: confined to the snapshot base alpha
                sp -= 1u;
                let ba = snap[sp - 1u];
                if (ba > 0.0) {
                    let s = stack[sp];
                    let a = s.a * op.opacity * ba;
                    if (a > 0.0 || op.mode == M_DISSOLVE) {
                        stack[sp - 1u] = blend_px(op.mode, vec4(s.rgb, a), stack[sp - 1u], x, y);
                    }
                }
            }
            case 4u: { // SnapshotAlpha
                snap[sp - 1u] = stack[sp - 1u].a;
            }
            case 5u: { // Adjust: LUT, full-colour branch or fill, weighted
                let d = stack[sp - 1u];
                var weight = op.opacity * mask_value(i, tile, x, y, px);
                if ((op.flags & F_CONFINE) != 0u) {
                    weight *= snap[sp - 1u];
                }
                if (weight > 0.0) {
                    if ((op.flags & F_FILL) != 0u) {
                        // Fill layers paint their colour rather than
                        // transforming the backdrop.
                        stack[sp - 1u] = blend_px(op.mode, vec4(op.fill.rgb, weight), d, x, y);
                    } else if (d.a > 0.0) {
                        var adjusted: vec3<f32>;
                        if (op.direct == D_NONE) {
                            adjusted = apply_lut(op.lut, d.rgb);
                        } else {
                            adjusted = apply_direct(
                                op.direct,
                                u32(op.dparams) * DIRECT_STRIDE,
                                d.rgb,
                            );
                        }
                        // Mirrors the CPU compositor: the adjustment's
                        // own blend mode applies, with the adjusted colour
                        // as the source and `weight` as its alpha. It used
                        // to be uploaded and then ignored, so every
                        // adjustment rendered as Normal.
                        if (op.mode == M_NORMAL) {
                            stack[sp - 1u] = vec4(d.rgb + (adjusted - d.rgb) * weight, d.a);
                        } else {
                            let blended = blend_px(op.mode, vec4(adjusted, weight), d, x, y);
                            stack[sp - 1u] = vec4(blended.rgb, d.a);
                        }
                    }
                }
            }
            case 6u: { // MaskTop: isolated-group mask
                stack[sp - 1u].a *= mask_value(i, tile, x, y, px);
            }
            default: {}
        }
    }

    let o = (tile * TILE_PIXELS + px) * 4u;
    let out = stack[0];
    out_f32[o] = out.x;
    out_f32[o + 1u] = out.y;
    out_f32[o + 2u] = out.z;
    out_f32[o + 3u] = out.w;
}
