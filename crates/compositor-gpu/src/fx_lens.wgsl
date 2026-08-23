// Lens blur: a disc-shaped kernel, so out-of-focus highlights come out as
// bokeh circles rather than smears.
//
// O(r²) taps per pixel with r up to 60 — the one filter where the CPU
// cost is measured in seconds on a full canvas, and the clearest win a
// second wgpu device has. The tap order matches `schist_fx`'s reference
// exactly (dy outer, dx inner, both ascending) so the accumulation error
// is the same.

struct Params {
    width: u32,
    height: u32,
    radius: i32,
    boost: f32,
}

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> src: array<f32>;
@group(0) @binding(2) var<storage, read_write> dst: array<f32>;

// Premultiplied read, clamping to the edge.
fn at(x: i32, y: i32) -> vec4<f32> {
    let cx = u32(clamp(x, 0, i32(p.width) - 1));
    let cy = u32(clamp(y, 0, i32(p.height) - 1));
    let i = (cy * p.width + cx) * 4u;
    let v = vec4(src[i], src[i + 1u], src[i + 2u], src[i + 3u]);
    return vec4(v.rgb * v.a, v.a);
}

@compute @workgroup_size(16, 16, 1)
fn lens_blur(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.width || gid.y >= p.height) {
        return;
    }
    let x = i32(gid.x);
    let y = i32(gid.y);
    let r = p.radius;
    var acc = vec4(0.0);
    var n = 0.0;
    for (var dy = -r; dy <= r; dy++) {
        for (var dx = -r; dx <= r; dx++) {
            if (dx * dx + dy * dy > r * r) {
                continue;
            }
            let s = at(x + dx, y + dy);
            let l = 0.299 * s.r + 0.587 * s.g + 0.114 * s.b;
            // Weighting bright samples up spreads highlights into discs
            // instead of smearing them away.
            let k = 1.0 + l * l * l * p.boost * 8.0;
            acc += s * k;
            n += k;
        }
    }
    var out = at(x, y);
    if (n > 0.0) {
        out = acc / n;
    }
    if (out.a > 1e-6) {
        out = vec4(out.rgb / out.a, out.a);
    } else {
        out = vec4(0.0, 0.0, 0.0, out.a);
    }
    let o = (gid.y * p.width + gid.x) * 4u;
    dst[o] = out.x;
    dst[o + 1u] = out.y;
    dst[o + 2u] = out.z;
    dst[o + 3u] = out.w;
}
