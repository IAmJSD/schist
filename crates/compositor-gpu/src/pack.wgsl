// RGBA8 packing: convert the composite pass's f32 output to packed
// RGBA8 for a quarter-size readback (the canvas cache wants bytes).
//
// Lives in its own module: its bindings would otherwise overlap the
// composite entry point's, which Vulkan and Metal translation tolerate
// but naga's HLSL backend rejects at pipeline creation.
struct PackGlobals {
    n_pixels: u32,
    _p0: u32,
    _p1: u32,
    _p2: u32,
}

@group(0) @binding(0) var<uniform> pack_globals: PackGlobals;
@group(0) @binding(1) var<storage, read> pack_in: array<f32>;
@group(0) @binding(2) var<storage, read_write> pack_out: array<u32>;

fn to_u8(v: f32) -> u32 {
    return u32(clamp(v, 0.0, 1.0) * 255.0 + 0.5);
}

@compute @workgroup_size(256, 1, 1)
fn pack_rgba8(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= pack_globals.n_pixels) {
        return;
    }
    let o = i * 4u;
    pack_out[i] = to_u8(pack_in[o])
        | (to_u8(pack_in[o + 1u]) << 8u)
        | (to_u8(pack_in[o + 2u]) << 16u)
        | (to_u8(pack_in[o + 3u]) << 24u);
}
