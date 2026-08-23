// Separable box blur, one axis per dispatch.
//
// Three rounds of this approximate a Gaussian, which is what every blur in
// the filter set is built from. The loop mirrors `schist_fx::box_pass`
// tap for tap and in the same order: each output pixel sums its own
// window, so there is no running total whose float error would drift away
// from the reference.

struct Params {
    width: u32,
    height: u32,
    radius: u32,
    // Bit 0: premultiply on read (the first pass of the first round).
    // Bit 1: unpremultiply on write (the last pass of the last round).
    flags: u32,
    vertical: u32,
    _p0: u32,
    _p1: u32,
    _p2: u32,
}

const F_PREMULTIPLY: u32 = 1u;
const F_UNPREMULTIPLY: u32 = 2u;

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> src: array<f32>;
@group(0) @binding(2) var<storage, read_write> dst: array<f32>;

fn load(index: u32) -> vec4<f32> {
    let i = index * 4u;
    var v = vec4(src[i], src[i + 1u], src[i + 2u], src[i + 3u]);
    if ((p.flags & F_PREMULTIPLY) != 0u) {
        v = vec4(v.rgb * v.a, v.a);
    }
    return v;
}

@compute @workgroup_size(16, 16, 1)
fn box_pass(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = gid.x;
    let y = gid.y;
    if (x >= p.width || y >= p.height) {
        return;
    }
    // `i` walks the blurred axis, `inner` is how far it runs.
    var i = x;
    var inner = p.width;
    if (p.vertical != 0u) {
        i = y;
        inner = p.height;
    }
    let r = p.radius;
    var acc = vec4(0.0);
    for (var k = 0u; k <= r * 2u; k++) {
        // saturating_sub then clamp to the last sample, as on the CPU.
        var s = 0u;
        if (i + k >= r) {
            s = i + k - r;
        }
        s = min(s, inner - 1u);
        var index: u32;
        if (p.vertical != 0u) {
            index = s * p.width + x;
        } else {
            index = y * p.width + s;
        }
        acc += load(index);
    }
    var out = acc / f32(r * 2u + 1u);
    if ((p.flags & F_UNPREMULTIPLY) != 0u) {
        if (out.a > 1e-6) {
            out = vec4(out.rgb / out.a, out.a);
        } else {
            out = vec4(0.0, 0.0, 0.0, out.a);
        }
    }
    let o = (y * p.width + x) * 4u;
    dst[o] = out.x;
    dst[o + 1u] = out.y;
    dst[o + 2u] = out.z;
    dst[o + 3u] = out.w;
}
