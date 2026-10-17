// Optical flow for the Smooth Cut: pyramidal Lucas-Kanade on luminance.
// Every kernel shares one layout; the textures it does not use are dummies.
// Flow is in pixels of the level it lives at, in `.xy`.

struct Params {
    weights: vec4<f32>,
    dst_size: vec2<u32>,
    use_flow: u32,
    radius: i32,
    lambda: f32,
    max_step: f32,
    sigma: f32,
    _pad: f32,
}

@group(0) @binding(0) var src0: texture_2d<f32>;
@group(0) @binding(1) var src1: texture_2d<f32>;
@group(0) @binding(2) var src2: texture_2d<f32>;
@group(0) @binding(3) var dst: texture_storage_2d<rgba32float, write>;
@group(0) @binding(4) var<uniform> params: Params;

fn load0(p: vec2<i32>) -> vec4<f32> {
    return textureLoad(src0, clamp(p, vec2<i32>(0), vec2<i32>(textureDimensions(src0)) - 1), 0);
}

fn load1(p: vec2<i32>) -> vec4<f32> {
    return textureLoad(src1, clamp(p, vec2<i32>(0), vec2<i32>(textureDimensions(src1)) - 1), 0);
}

fn load2(p: vec2<i32>) -> vec4<f32> {
    return textureLoad(src2, clamp(p, vec2<i32>(0), vec2<i32>(textureDimensions(src2)) - 1), 0);
}

// Float textures are not filterable everywhere: bilinear by hand. `pos` in
// texels, pixel centers at +0.5.
fn bilinear0(pos: vec2<f32>) -> vec4<f32> {
    let q = pos - 0.5;
    let i = vec2<i32>(floor(q));
    let f = q - floor(q);
    let top = mix(load0(i), load0(i + vec2<i32>(1, 0)), f.x);
    let bottom = mix(load0(i + vec2<i32>(0, 1)), load0(i + vec2<i32>(1, 1)), f.x);
    return mix(top, bottom, f.y);
}

fn bilinear1(pos: vec2<f32>) -> vec4<f32> {
    let q = pos - 0.5;
    let i = vec2<i32>(floor(q));
    let f = q - floor(q);
    let top = mix(load1(i), load1(i + vec2<i32>(1, 0)), f.x);
    let bottom = mix(load1(i + vec2<i32>(0, 1)), load1(i + vec2<i32>(1, 1)), f.x);
    return mix(top, bottom, f.y);
}

fn outside(p: vec2<u32>) -> bool {
    return any(p >= params.dst_size);
}

// src0 → dst, box average over each destination pixel's footprint, reduced to
// one channel with `weights` (luminance from RGB, or `.r` of a coarser level).
@compute @workgroup_size(8, 8)
fn reduce(@builtin(global_invocation_id) gid: vec3<u32>) {
    if outside(gid.xy) {
        return;
    }
    let ratio = vec2<f32>(textureDimensions(src0)) / vec2<f32>(params.dst_size);
    let lo = vec2<i32>(floor(vec2<f32>(gid.xy) * ratio));
    let hi = max(vec2<i32>(ceil(vec2<f32>(gid.xy + 1u) * ratio)), lo + 1);
    let end = min(hi, lo + 8);
    var sum = 0.0;
    var n = 0.0;
    for (var y = lo.y; y < end.y; y++) {
        for (var x = lo.x; x < end.x; x++) {
            sum += dot(load0(vec2<i32>(x, y)), params.weights);
            n += 1.0;
        }
    }
    textureStore(dst, gid.xy, vec4<f32>(sum / n, 0.0, 0.0, 0.0));
}

// Coarse flow (src0) → finer level, scaled to its pixels.
@compute @workgroup_size(8, 8)
fn upsample(@builtin(global_invocation_id) gid: vec3<u32>) {
    if outside(gid.xy) {
        return;
    }
    let scale = vec2<f32>(params.dst_size) / vec2<f32>(textureDimensions(src0));
    let pos = (vec2<f32>(gid.xy) + 0.5) / scale;
    textureStore(dst, gid.xy, vec4<f32>(bilinear0(pos).xy * scale, 0.0, 0.0));
}

// One Lucas-Kanade step: src0 = first image, src1 = second, src2 = current
// flow (ignored without `use_flow`). Solves for the update that makes
// src1(p + flow) match src0(p) over a gaussian window.
@compute @workgroup_size(8, 8)
fn lk(@builtin(global_invocation_id) gid: vec3<u32>) {
    if outside(gid.xy) {
        return;
    }
    let p = vec2<i32>(gid.xy);
    var flow = vec2<f32>(0.0);
    if params.use_flow != 0u {
        flow = load2(p).xy;
    }
    let r = params.radius;
    let inv_two_sigma2 = 1.0 / (2.0 * params.sigma * params.sigma);
    var gxx = 0.0;
    var gxy = 0.0;
    var gyy = 0.0;
    var b = vec2<f32>(0.0);
    for (var dy = -r; dy <= r; dy++) {
        for (var dx = -r; dx <= r; dx++) {
            let q = p + vec2<i32>(dx, dy);
            let w = exp(-f32(dx * dx + dy * dy) * inv_two_sigma2);
            let ix = 0.5 * (load0(q + vec2<i32>(1, 0)).r - load0(q - vec2<i32>(1, 0)).r);
            let iy = 0.5 * (load0(q + vec2<i32>(0, 1)).r - load0(q - vec2<i32>(0, 1)).r);
            let it = bilinear1(vec2<f32>(q) + 0.5 + flow).r - load0(q).r;
            gxx += w * ix * ix;
            gxy += w * ix * iy;
            gyy += w * iy * iy;
            b += w * vec2<f32>(ix, iy) * it;
        }
    }
    // The damping keeps flat areas (no gradient) from blowing up.
    let axx = gxx + params.lambda;
    let ayy = gyy + params.lambda;
    let det = axx * ayy - gxy * gxy;
    var step = -vec2<f32>(ayy * b.x - gxy * b.y, axx * b.y - gxy * b.x) / det;
    let len = length(step);
    if len > params.max_step {
        step *= params.max_step / len;
    }
    textureStore(dst, gid.xy, vec4<f32>(flow + step, 0.0, 0.0));
}

// Edge-aware smoothing of the flow (src0), guided by the image (src1): fills
// the flat areas from their surroundings without bleeding across edges.
@compute @workgroup_size(8, 8)
fn smooth_flow(@builtin(global_invocation_id) gid: vec3<u32>) {
    if outside(gid.xy) {
        return;
    }
    let p = vec2<i32>(gid.xy);
    let center = load1(p).r;
    let r = params.radius;
    let inv_two_sigma2 = 1.0 / (2.0 * params.sigma * params.sigma);
    var sum = vec2<f32>(0.0);
    var total = 0.0;
    for (var dy = -r; dy <= r; dy++) {
        for (var dx = -r; dx <= r; dx++) {
            let q = p + vec2<i32>(dx, dy);
            let diff = (load1(q).r - center) / params.lambda;
            let w = exp(-f32(dx * dx + dy * dy) * inv_two_sigma2 - diff * diff);
            sum += w * load0(q).xy;
            total += w;
        }
    }
    textureStore(dst, gid.xy, vec4<f32>(sum / total, 0.0, 0.0));
}
