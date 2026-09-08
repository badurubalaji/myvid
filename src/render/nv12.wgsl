// NV12 -> RGB. One full-screen triangle, scaled to letterbox the video inside
// the widget bounds. The render pass iced hands us is already scissored to the
// widget, so clip space here maps exactly onto those bounds.

struct Uniforms {
    // Shrinks the triangle on one axis to preserve the video's aspect ratio.
    scale: vec2<f32>,
    // 1.0 when the render target is sRGB and the hardware will re-encode for us.
    srgb: f32,
    _pad: f32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var t_luma: texture_2d<f32>;
@group(0) @binding(2) var t_chroma: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOut {
    // An oversized triangle covering the [-1, 1] square.
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -3.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 3.0,  1.0),
    );

    let corner = corners[index];

    var out: VertexOut;
    // Texture coordinates come from the *unscaled* corner, so shrinking the
    // triangle letterboxes the picture instead of cropping it.
    out.uv = vec2<f32>((corner.x + 1.0) * 0.5, (1.0 - corner.y) * 0.5);
    out.position = vec4<f32>(corner.x * u.scale.x, corner.y * u.scale.y, 0.0, 1.0);
    return out;
}

// Gamma-encoded sRGB -> linear, for when the target format re-encodes on write.
fn to_linear(c: vec3<f32>) -> vec3<f32> {
    let cutoff = c <= vec3<f32>(0.04045);
    let low = c / 12.92;
    let high = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(high, low, cutoff);
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let luma = textureSample(t_luma, samp, in.uv).r;
    let chroma = textureSample(t_chroma, samp, in.uv).rg;

    // BT.709, limited range (16-235 luma, 16-240 chroma).
    let y = (luma - 0.0625) * 1.164383;
    let cb = chroma.r - 0.5;
    let cr = chroma.g - 0.5;

    var rgb = vec3<f32>(
        y + 1.792741 * cr,
        y - 0.213249 * cb - 0.532909 * cr,
        y + 2.112402 * cb,
    );
    rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));

    if (u.srgb > 0.5) {
        rgb = to_linear(rgb);
    }

    return vec4<f32>(rgb, 1.0);
}
