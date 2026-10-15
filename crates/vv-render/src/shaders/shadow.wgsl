// Drop shadow of a layer: the alpha of its own render, already blurred,
// moved and painted in one color.

struct ShadowUniform {
    // rgb: color, a: opacity.
    color: vec4<f32>,
    // xy: displacement in uv.
    offset: vec4<f32>,
};

@group(0) @binding(0) var layer_tex: texture_2d<f32>;
@group(0) @binding(1) var layer_sampler: sampler;
@group(0) @binding(2) var<uniform> shadow: ShadowUniform;

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

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv - shadow.offset.xy;
    var alpha = textureSampleLevel(layer_tex, layer_sampler, uv, 0.0).a;
    // The sampler clamps to the edge: past it there is nothing to cast.
    if (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0))) {
        alpha = 0.0;
    }
    return vec4<f32>(shadow.color.rgb, alpha * shadow.color.a);
}
