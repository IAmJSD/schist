// Viewport resampling on the GPU: the same crisp/bilinear/box sampling,
// checkerboard and surround as `schist_compositor::viewport`, one thread
// per output device pixel. Mirrors render_viewport_cpu exactly — loop
// order and operand order included — so the two stay interchangeable.

const TILE: i32 = 256;

struct VParams {
    size: vec2<u32>,
    origin: vec2<f32>,
    centre: vec2<f32>,
    // sin/cos of -rotation, computed on the CPU so both paths share the
    // exact same constants.
    rot: vec2<f32>,
    inv_zoom: f32,
    sf: f32,
    footprint: f32,
    _p0: f32,
    canvas: vec4<i32>, // left, top, right, bottom
    grid: vec4<i32>,   // tx0, ty0, cols, rows
    surround: u32,
    crisp: u32,
    box_taps: u32,
    _p1: u32,
}

@group(0) @binding(0) var<uniform> p: VParams;
@group(0) @binding(1) var<storage, read> tiles: array<u32>;
@group(0) @binding(2) var<storage, read> grid_index: array<i32>;
@group(0) @binding(3) var<storage, read_write> out_bgra: array<u32>;

fn doc_at(dx: f32, dy: f32) -> vec2<f32> {
    let o = vec2(dx, dy) - p.centre;
    let r = vec2(o.x * p.rot.y - o.y * p.rot.x, o.x * p.rot.x + o.y * p.rot.y) + p.centre;
    return (r - p.origin) * p.inv_zoom / p.sf;
}

// RGBA8 sample as 0..255 components; transparent outside the canvas, the
// grid, or missing tiles.
fn sample(x: i32, y: i32) -> vec4<f32> {
    if (x < p.canvas.x || y < p.canvas.y || x >= p.canvas.z || y >= p.canvas.w) {
        return vec4(0.0);
    }
    let tx = (x >> 8u) - p.grid.x;
    let ty = (y >> 8u) - p.grid.y;
    if (tx < 0 || ty < 0 || tx >= p.grid.z || ty >= p.grid.w) {
        return vec4(0.0);
    }
    let slot = grid_index[ty * p.grid.z + tx];
    if (slot < 0) {
        return vec4(0.0);
    }
    let lx = x & 255;
    let ly = y & 255;
    let w = tiles[u32(slot) * 65536u + u32(ly * TILE + lx)];
    return vec4(
        f32(w & 0xFFu),
        f32((w >> 8u) & 0xFFu),
        f32((w >> 16u) & 0xFFu),
        f32((w >> 24u) & 0xFFu),
    );
}

// Straight-alpha result from a premultiplied accumulator (u8 components).
fn resolve(acc: vec4<f32>) -> vec4<u32> {
    if (acc.a <= 1e-6) {
        return vec4(0u);
    }
    return vec4(
        u32(clamp(floor(acc.r / acc.a + 0.5), 0.0, 255.0)),
        u32(clamp(floor(acc.g / acc.a + 0.5), 0.0, 255.0)),
        u32(clamp(floor(acc.b / acc.a + 0.5), 0.0, 255.0)),
        u32(clamp(floor(acc.a * 255.0 + 0.5), 0.0, 255.0)),
    );
}

@compute @workgroup_size(16, 16, 1)
fn viewport(@builtin(global_invocation_id) gid: vec3<u32>) {
    let col = gid.x;
    let row = gid.y;
    if (col >= p.size.x || row >= p.size.y) {
        return;
    }
    let dx = f32(col) + 0.5;
    let dy = f32(row) + 0.5;
    let f = doc_at(dx, dy);
    var px: vec4<u32>;
    if (p.crisp != 0u) {
        let s = sample(i32(floor(f.x)), i32(floor(f.y)));
        px = vec4<u32>(s);
    } else if (p.box_taps == 0u) {
        // Bilinear over the four neighbours, in the CPU's loop order.
        let sx = f.x - 0.5;
        let sy = f.y - 0.5;
        let ix = floor(sx);
        let iy = floor(sy);
        let tx = sx - ix;
        let ty = sy - iy;
        var acc = vec4(0.0);
        for (var dxi = 0; dxi < 2; dxi++) {
            var wx = 1.0 - tx;
            if (dxi == 1) { wx = tx; }
            for (var dyi = 0; dyi < 2; dyi++) {
                var wy = 1.0 - ty;
                if (dyi == 1) { wy = ty; }
                let w = wx * wy;
                if (w <= 0.0) {
                    continue;
                }
                let s = sample(i32(ix) + dxi, i32(iy) + dyi);
                let a = s.a / 255.0 * w;
                acc += vec4(s.rgb * a, a);
            }
        }
        px = resolve(acc);
    } else {
        // Box average over the pixel footprint.
        let n = i32(p.box_taps);
        var acc = vec4(0.0);
        for (var sy = 0; sy < n; sy++) {
            let oy = ((f32(sy) + 0.5) / f32(n) - 0.5) * p.footprint;
            for (var sx = 0; sx < n; sx++) {
                let ox = ((f32(sx) + 0.5) / f32(n) - 0.5) * p.footprint;
                let s = sample(i32(floor(f.x + ox)), i32(floor(f.y + oy)));
                let a = s.a / 255.0;
                acc += vec4(s.rgb * a, a);
            }
        }
        // Mean, not sum: `resolve` reads acc.a as the pixel's coverage,
        // and the rgb divide cancels the same factor.
        acc /= f32(n * n);
        px = resolve(acc);
    }

    // Transparency shows the checkerboard inside the canvas and the app
    // background outside it.
    let fi = vec2<i32>(i32(floor(f.x)), i32(floor(f.y)));
    let inside = fi.x >= p.canvas.x && fi.y >= p.canvas.y && fi.x < p.canvas.z && fi.y < p.canvas.w;
    var bg = p.surround & 0xFFu;
    if (inside) {
        if ((((col >> 3u) + (row >> 3u)) & 1u) == 0u) {
            bg = 0xFFu;
        } else {
            bg = 0xCCu;
        }
    }
    let inv = 255u - px.a;
    let b = (px.b * px.a + bg * inv) / 255u;
    let g = (px.g * px.a + bg * inv) / 255u;
    let r = (px.r * px.a + bg * inv) / 255u;
    out_bgra[row * p.size.x + col] = b | (g << 8u) | (r << 16u) | (255u << 24u);
}
