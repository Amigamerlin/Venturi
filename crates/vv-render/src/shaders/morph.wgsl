// Smooth Cut: the frame at `progress` between `a` and `b`, each warped along
// the flow towards it and then mixed.

struct Morph {
    out_size: vec2<f32>,
    // Output pixels per flow pixel.
    flow_scale: vec2<f32>,
    progress: f32,
    // Not a vec3: it would align the struct to 48 bytes.
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
}

@group(0) @binding(0) var a_tex: texture_2d<f32>;
@group(0) @binding(1) var b_tex: texture_2d<f32>;
@group(0) @binding(2) var image_sampler: sampler;
@group(0) @binding(3) var flow_ab: texture_2d<f32>;
@group(0) @binding(4) var flow_ba: texture_2d<f32>;
@group(0) @binding(5) var<uniform> morph: Morph;

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    let x = f32((vertex_index << 1u) & 2u);
    let y = f32(vertex_index & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

fn load_flow(t: texture_2d<f32>, p: vec2<i32>) -> vec2<f32> {
    return textureLoad(t, clamp(p, vec2<i32>(0), vec2<i32>(textureDimensions(t)) - 1), 0).xy;
}

fn flow_at(t: texture_2d<f32>, pos: vec2<f32>) -> vec2<f32> {
    let q = pos - 0.5;
    let i = vec2<i32>(floor(q));
    let f = q - floor(q);
    let top = mix(load_flow(t, i), load_flow(t, i + vec2<i32>(1, 0)), f.x);
    let bottom = mix(load_flow(t, i + vec2<i32>(0, 1)), load_flow(t, i + vec2<i32>(1, 1)), f.x);
    return mix(top, bottom, f.y);
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let pos = position.xy;
    let flow_pos = pos / morph.flow_scale;
    // Both directions, averaged: the flow at this output pixel from a to b.
    let flow = 0.5 * (flow_at(flow_ab, flow_pos) - flow_at(flow_ba, flow_pos)) * morph.flow_scale;
    let t = morph.progress;
    let a = textureSampleLevel(a_tex, image_sampler, (pos - t * flow) / morph.out_size, 0.0);
    let b = textureSampleLevel(b_tex, image_sampler, (pos + (1.0 - t) * flow) / morph.out_size, 0.0);
    return mix(a, b, t);
}
