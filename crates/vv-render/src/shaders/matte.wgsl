// Track matte: the coverage of the matte, written as alpha, which the
// pipeline's blend state multiplies into the (premultiplied) layer below.

struct MatteUniform {
    // x: 1 = luma, 0 = alpha. y: 1 = inverted.
    mode: vec4<f32>,
};

@group(0) @binding(0) var matte_tex: texture_2d<f32>;
@group(0) @binding(1) var<uniform> matte: MatteUniform;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    let x = f32((vertex_index << 1u) & 2u);
    let y = f32(vertex_index & 2u);

    var out: VertexOutput;
    out.clip_position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureLoad(matte_tex, vec2<i32>(in.clip_position.xy), 0);
    // Premultiplied: the luma is already zero where the matte is transparent.
    var coverage = texel.a;
    if (matte.mode.x > 0.5) {
        coverage = dot(texel.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    }
    coverage = clamp(coverage, 0.0, 1.0);
    if (matte.mode.y > 0.5) {
        coverage = 1.0 - coverage;
    }
    return vec4<f32>(0.0, 0.0, 0.0, coverage);
}
