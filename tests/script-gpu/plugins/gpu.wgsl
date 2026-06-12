// Render entry points: sample a texture across a quad using the
// standard sprite vertex layout and bind groups.
struct TransformUniforms { ortho: mat4x4<f32>, transform: mat4x4<f32>, }
@group(0) @binding(0) var<uniform> uniforms: TransformUniforms;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var tex_sampler: sampler;

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
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(tex, tex_sampler, in.uv);
}

// Compute entry point: invert RGB. Uses the compute IO bind group
// layout (group 0: input texture, output storage texture). The
// bindings alias the render declarations above, which is fine — they
// are never used by the same entry point.
@group(0) @binding(0) var cs_input: texture_2d<f32>;
@group(0) @binding(1) var cs_output: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(8, 8)
fn cs_invert(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(cs_input);
    if (gid.x >= dims.x || gid.y >= dims.y) { return; }
    let c = textureLoad(cs_input, vec2<i32>(gid.xy), 0);
    textureStore(
        cs_output,
        vec2<i32>(gid.xy),
        vec4<f32>(1.0 - c.r, 1.0 - c.g, 1.0 - c.b, c.a),
    );
}
