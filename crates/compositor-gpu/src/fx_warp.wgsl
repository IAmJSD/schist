// Resample a source plane through a coarse displacement grid.
//
// Liquify and Puppet Warp re-run this over the whole warped region on
// every pointer move, always from the same snapshot — which is why the
// executor keeps the source plane resident on the device between calls
// and only reads the result back.
//
// Sampling is bilinear on premultiplied alpha, so warping a soft edge does
// not fringe it; the arithmetic mirrors `schist_fx::warp_cpu`.

struct Params {
    src_width: u32,
    src_height: u32,
    src_origin: vec2<i32>,
    dst_width: u32,
    dst_height: u32,
    dst_origin: vec2<i32>,
    mesh_cols: u32,
    mesh_rows: u32,
    mesh_origin: vec2<i32>,
    cell: f32,
    _p0: u32,
    _p1: u32,
    _p2: u32,
}

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> src: array<f32>;
@group(0) @binding(2) var<storage, read_write> dst: array<f32>;
@group(0) @binding(3) var<storage, read> mesh: array<f32>;

/// Straight-alpha source pixel in document coordinates; transparent
/// outside the plane.
fn src_pixel(x: i32, y: i32) -> vec4<f32> {
    let lx = x - p.src_origin.x;
    let ly = y - p.src_origin.y;
    if (lx < 0 || ly < 0 || lx >= i32(p.src_width) || ly >= i32(p.src_height)) {
        return vec4(0.0);
    }
    let i = (u32(ly) * p.src_width + u32(lx)) * 4u;
    return vec4(src[i], src[i + 1u], src[i + 2u], src[i + 3u]);
}

fn mesh_at(c: u32, r: u32) -> vec2<f32> {
    let i = (r * p.mesh_cols + c) * 2u;
    return vec2(mesh[i], mesh[i + 1u]);
}

fn mesh_sample(x: f32, y: f32) -> vec2<f32> {
    if (p.mesh_cols < 2u || p.mesh_rows < 2u) {
        return vec2(0.0);
    }
    let fx = clamp((x - f32(p.mesh_origin.x)) / p.cell, 0.0, f32(p.mesh_cols - 1u));
    let fy = clamp((y - f32(p.mesh_origin.y)) / p.cell, 0.0, f32(p.mesh_rows - 1u));
    let c0 = u32(floor(fx));
    let r0 = u32(floor(fy));
    let c1 = min(c0 + 1u, p.mesh_cols - 1u);
    let r1 = min(r0 + 1u, p.mesh_rows - 1u);
    let tx = fx - f32(c0);
    let ty = fy - f32(r0);
    let a = mesh_at(c0, r0);
    let b = mesh_at(c1, r0);
    let cc = mesh_at(c0, r1);
    let d = mesh_at(c1, r1);
    let top = a + (b - a) * tx;
    let bottom = cc + (d - cc) * tx;
    return top + (bottom - top) * ty;
}

fn fetch(fx: f32, fy: f32) -> vec4<f32> {
    let x0f = floor(fx);
    let y0f = floor(fy);
    let tx = fx - x0f;
    let ty = fy - y0f;
    let x0 = i32(x0f);
    let y0 = i32(y0f);
    var acc = vec4(0.0);
    for (var t = 0u; t < 4u; t++) {
        let dx = i32(t & 1u);
        let dy = i32(t >> 1u);
        var wx = 1.0 - tx;
        if (dx == 1) {
            wx = tx;
        }
        var wy = 1.0 - ty;
        if (dy == 1) {
            wy = ty;
        }
        let w = wx * wy;
        if (w <= 0.0) {
            continue;
        }
        let s = src_pixel(x0 + dx, y0 + dy);
        acc += vec4(s.rgb * s.a * w, s.a * w);
    }
    if (acc.a <= 1e-6) {
        return vec4(0.0);
    }
    return vec4(acc.rgb / acc.a, acc.a);
}

@compute @workgroup_size(16, 16, 1)
fn mesh_warp(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.dst_width || gid.y >= p.dst_height) {
        return;
    }
    let x = p.dst_origin.x + i32(gid.x);
    let y = p.dst_origin.y + i32(gid.y);
    let fx = f32(x) + 0.5;
    let fy = f32(y) + 0.5;
    let d = mesh_sample(fx, fy);
    let px = fetch(fx + d.x - 0.5, fy + d.y - 0.5);
    let o = (gid.y * p.dst_width + gid.x) * 4u;
    dst[o] = px.x;
    dst[o + 1u] = px.y;
    dst[o + 2u] = px.z;
    dst[o + 3u] = px.w;
}
