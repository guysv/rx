// Two-texture blend, rendered into a script texture target.
struct TransformUniforms { ortho: mat4x4<f32>, transform: mat4x4<f32>, }
@group(0) @binding(0) var<uniform> uniforms: TransformUniforms;
@group(1) @binding(0) var tex_a: texture_2d<f32>;
@group(1) @binding(1) var samp_a: sampler;
@group(2) @binding(0) var tex_b: texture_2d<f32>;
@group(2) @binding(1) var samp_b: sampler;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) opacity: f32,
}
struct VertexOutput {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.pos = uniforms.ortho * uniforms.transform * vec4<f32>(in.position, 1.0);
    out.uv = in.uv;
    return out;
}

@fragment
fn fs_max(in: VertexOutput) -> @location(0) vec4<f32> {
    let a = textureSample(tex_a, samp_a, in.uv);
    let b = textureSample(tex_b, samp_b, in.uv);
    return max(a, b);
}
