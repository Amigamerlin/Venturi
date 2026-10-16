// Thickens a layer's coverage by a radius with round corners, from the exact
// distance to its covered texels: two separable passes, each bounded by the
// radius. Distances are in texels of the X axis.

struct GrowUniform {
    // x: radius, y: texels searched each side along X, z: along Y,
    // w: X texels per Y texel.
    params: vec4<f32>,
};

@group(0) @binding(0) var layer_tex: texture_2d<f32>;
@group(0) @binding(1) var<uniform> grow: GrowUniform;
// Output of `fs_rows`.
@group(0) @binding(2) var rows_tex: texture_2d<f32>;

// Beyond any reach, within the range of the half float `fs_rows` writes.
const FAR: f32 = 60000.0;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    let x = f32((vertex_index << 1u) & 2u);
    let y = f32(vertex_index & 2u);

    var out: VertexOutput;
    out.clip_position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

// Squared distance along the row to the nearest texel at least half covered.
@fragment
fn fs_rows(in: VertexOutput) -> @location(0) vec4<f32> {
    let p = vec2<i32>(floor(in.clip_position.xy));
    let width = i32(textureDimensions(layer_tex).x);
    let reach = i32(grow.params.y);
    var best = FAR;
    for (var dx = -reach; dx <= reach; dx = dx + 1) {
        let x = p.x + dx;
        if (x >= 0 && x < width && textureLoad(layer_tex, vec2<i32>(x, p.y), 0).a >= 0.5) {
            best = min(best, f32(dx * dx));
        }
    }
    return vec4<f32>(best, 0.0, 0.0, 0.0);
}

@fragment
fn fs_grow(in: VertexOutput) -> @location(0) vec4<f32> {
    let p = vec2<i32>(floor(in.clip_position.xy));
    let height = i32(textureDimensions(rows_tex).y);
    let reach = i32(grow.params.z);
    var best = FAR;
    for (var dy = -reach; dy <= reach; dy = dy + 1) {
        let y = p.y + dy;
        if (y >= 0 && y < height) {
            let along = f32(dy) * grow.params.w;
            best = min(best, textureLoad(rows_tex, vec2<i32>(p.x, y), 0).r + along * along);
        }
    }
    // Center to center, as `grow` in text.rs: the edge of the covered texel
    // is half a texel closer.
    let cover = clamp(grow.params.x + 1.0 - sqrt(best), 0.0, 1.0);
    return vec4<f32>(0.0, 0.0, 0.0, max(textureLoad(layer_tex, p, 0).a, cover));
}
