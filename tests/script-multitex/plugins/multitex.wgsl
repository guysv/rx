// Two-texture blend (groups 1 and 2 each texture+sampler) and a
// two-input compute sum. The group-0 declarations alias between the
// render and compute entry points, which is fine — they are never
// used by the same one.
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
fn fs_blend(in: VertexOutput) -> @location(0) vec4<f32> {
    let a = textureSample(tex_a, samp_a, in.uv);
    let b = textureSample(tex_b, samp_b, in.uv);
    return mix(a, b, 0.5);
}

@group(0) @binding(0) var cs_a: texture_2d<f32>;
@group(0) @binding(1) var cs_b: texture_2d<f32>;
@group(0) @binding(2) var cs_out: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(8, 8)
fn cs_sum(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(cs_a);
    if (gid.x >= dims.x || gid.y >= dims.y) { return; }
    let a = textureLoad(cs_a, vec2<i32>(gid.xy), 0);
    let b = textureLoad(cs_b, vec2<i32>(gid.xy), 0);
    textureStore(cs_out, vec2<i32>(gid.xy), min(a + b, vec4<f32>(1.0)));
}
