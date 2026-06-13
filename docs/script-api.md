# rx scripting API reference

The script surface as of the rune port (P4–P22, `docs/rune-plan.md`)
truth is `src/script.rs` — every function below carries a doc comment
there, and `module()` (the `rx` module installed into every plugin)
is the canonical index. The sample plugins under `plugins/` exercise
all of it.

## Plugins

A plugin is a `<dir>/<name>.rune` file in the plugin directory, or a
`<dir>/<name>/<name>.rune` package (the directory carrying its
assets, e.g. WGSL shaders), loaded in lexicographic order.
`pub fn init(rx)` runs at load and returns the plugin's state value,
which is passed back as the first argument of every hook, command
handler, and export. Script errors and GPU validation errors disable
the plugin and post one message; other plugins are unaffected.

```rune
pub fn init(rx) {
    rx.register_command("greet", ["str"], "Say hello", greet);
    #{ count: 0 }
}

pub fn greet(state, rx, args) {
    state.count += 1;
    rx.message(`hello ${args[0]} (${state.count})`);
}
```

## Hooks

Defined as `pub fn` at the top level; all optional.

| Hook | Called |
|---|---|
| `init(rx) -> state` | once at load; the return value is the plugin state |
| `unload(state, rx)` | before the plugin is dropped (unload / hot reload) |
| `update(state, rx)` | every frame, before rendering |
| `switch_mode(state, rx)` | on mode *edges* (query `rx.mode()`); fires once for the initial mode, and on `command`-mode entries |
| `cursor_moved(state, rx, x, y)` | mouse motion, window logical coords (`rx.session_coords(x, y)` converts) |
| `mouse_input(state, rx, button, input)` | `button` is `"left"`/`"right"`/`"middle"`, `input` is `"pressed"`/`"released"` |
| `view_added(state, rx, id)` / `view_removed(state, rx, id)` | view lifecycle |
| `shade(state, rx, encoder)` | per frame: record render/compute passes on the frame encoder (before screen composition) |
| `render(state, rx, pass)` | per frame: the live screen pass — everything drawn, present ahead — for screen-space pipeline drawing |
| `draw(state, rx)` | per frame: UI-tier text/line drawing (`rx.draw_text` / `rx.draw_line` are only live here) |

The `update` hook fires every frame: derive edges, never count, if the
result is visible (replay digests treat every distinct frame as
unique).

## Session & modes

| Function | Notes |
|---|---|
| `rx.mode() -> String` | current mode (`"normal"`, `"visual"`, `"command"`, … or a script mode) |
| `rx.prev_mode() -> Option<String>` | previous mode |
| `rx.switch_mode(name) -> bool` | builtin names switch builtin modes; any other name enters a *script mode* — builtin input handling is inert there, escape exits |
| `rx.message(msg)` | post to the message line |
| `rx.fg() -> Rgba8` / `rx.bg() -> Rgba8` | the color pair |
| `rx.set_fg(color)` | picker semantics: old fg becomes bg; transparent ignored |

## Coordinates & screen

| Function | Notes |
|---|---|
| `rx.offset() -> (f64, f64)` | workspace pan offset |
| `rx.screen_size() -> (i64, i64)` | the `render` stage target size; build screen orthos against it |
| `rx.cursor() -> (f64, f64)` | cursor in session coordinates |
| `rx.session_coords(x, y) -> (f64, f64)` | window logical → session |
| `rx.active_view_coords(x, y) -> (f64, f64)` | session → active-view (unrounded) |

Session and view coordinates are y-up. UI coordinates (the `draw`
hook) are y-down, origin top-left.

## Views

| Function | Notes |
|---|---|
| `rx.active_view_id() -> i64` | |
| `rx.views() -> Vec<ViewInfo>` | snapshots, in view order |
| `rx.view_pixels(id, rect) -> Option<Bytes>` | rgba8, row-major, from the *recorded snapshot* (see conventions); rect clamped |
| `rx.layer_visibility(id) -> Vec<bool>` | per-layer `visible`, bottom strip first; empty if the view doesn't exist |
| `rx.touch_view(id)` | mark modified → contents re-recorded (do this after painting a view via a pass) |
| `rx.clear_view_rect(rect)` | clear a rect of the active view to transparent — a recorded paint |
| `rx.damage_view(id)` | re-render from the snapshot, discarding unrecorded GPU-side paint (kill a preview) |

`ViewInfo` fields (read-only): `id`, `width` (full sheet:
`frame_width * frames`), `height`, `offset_x`, `offset_y`, `zoom`,
`frames`, `frame_width`, `frame_height`, `nlayers` (1 for a flat view),
`active_layer` (`0` is the bottom strip).

## Selection

| Function | Notes |
|---|---|
| `rx.selection() -> Option<(i64, i64, i64, i64)>` | normalized (`x1 <= x2`, `y1 <= y2`) regardless of drag direction |
| `rx.set_selection(x1, y1, x2, y2)` | |
| `rx.clear_selection()` | |

The selection is cleared when the session returns to normal mode.

## Settings

| Function | Notes |
|---|---|
| `rx.setting(name) -> Value` | bool / int / float / string / tuple; unit if absent |
| `rx.set_setting(name, value) -> bool` | value must match the current type |
| `rx.declare_setting(name, default) -> bool` | plugin-owned, `:set`-able like builtins; re-declaring is a no-op |

## Commands & bindings

| Function | Notes |
|---|---|
| `rx.register_command(name, sig, help, handler) -> bool` | `sig` is typed params: `"int"`, `"float"`, `"str"`, `"color"`, `"bool"`, suffix `?` for optional; handler is `handler(state, rx, args)` |
| `rx.register_command_repeating(...)` | same, repeats while the bound key is held |
| `rx.bind(mode, mapping) -> bool` | script-tier binding in a script mode; `:map` syntax, e.g. `"<tab> :v/prev"`, `"'r' :rotate 90 {:rotate 0}"`; wins over general bindings while the mode is active, may fire with the mouse held |
| `rx.run_builtin(invocation) -> bool` | run a *builtin* command (script commands aren't resolvable through it) |

Character bindings (`'r'`) fire on `char/received`, not raw key input.
The command line is not reachable from script modes — drive script
modes entirely through bindings.

## Meta plugins

| Function | Notes |
|---|---|
| `rx.export(name, handler) -> bool` | offer `handler(state, rx, args)` to other plugins, run with the *exporting* plugin's state |
| `rx.call_plugin(plugin, name, args) -> Result<Value>` | call another plugin's export |

## UI drawing (the `draw` hook only)

| Function | Notes |
|---|---|
| `rx.draw_text(text, x, y, color)` | UI coordinates; no-op outside `draw` |
| `rx.draw_line(p1, p2, color)` | 1px line, `(x, y)` tuples; no-op outside `draw` |

## Files & output

| Function | Notes |
|---|---|
| `rx.read_file(path) -> Option<String>` | relative to the *plugin's* directory (shaders etc.) |
| `rx.write_png(path, w, h, data) -> bool` | rgba8 row-major → PNG; path resolves like `:export` (cwd-relative, *not* plugin-relative); loud on both outcomes |

## GPU: resources

All constructors return `Option` and post the error (shader compile
errors included) to the message line on `None`.

| Function | Notes |
|---|---|
| `rx.create_texture(w, h) -> Option<ScriptTexture>` | rgba8 (sRGB), max dimension 8192; owned by the plugin, dropped with its state |
| `rx.texture_pixels(tex) -> Option<Bytes>` | readback, mirror of `upload`; forces a GPU sync — command-handler tool, not per-frame (see staleness below) |
| `rx.create_shader(wgsl) -> Option<ScriptShader>` | |
| `rx.create_render_pipeline(shader, vs, fs, textures) -> Option<ScriptPipeline>` | triangle list, sprite vertex layout, alpha blending onto rgba8; `textures` = 0–3 texture groups |
| `rx.create_point_pipeline(shader, vs, fs, textures) -> Option<ScriptPipeline>` | point list, **no vertex buffers** — position points from `@builtin(vertex_index)`, typically `textureLoad`ing a bound texture (scatter passes); same bind-group layout as render pipelines |
| `rx.create_compute_pipeline(shader, entry, inputs) -> Option<ScriptComputePipeline>` | `inputs` = 1–4 input textures at bindings `0..n`, write-only rgba8 storage output at binding `n` (one group) |

ScriptTexture methods: `width()`, `height()`,
`upload(bytes) -> bool` (exactly `w*h*4` row-major bytes; the call
*moves* the Bytes value), `fill(color)`.

## GPU: bind groups & vertices

Render/point pipeline layout: group 0 = transform + params, groups
`1..=textures` = one texture + nearest sampler each. Texture bindings
are visible to both vertex and fragment stages.

| Function | Notes |
|---|---|
| `rx.create_transform_bind_group(w, h, transform) -> Option<ScriptBindGroup>` | group 0: ortho for a `w`×`h` target composed with `transform`; params slot zeroed (WGSL that doesn't declare binding 1 is unaffected) |
| `rx.create_transform_params_bind_group(w, h, transform, params) -> Option<ScriptBindGroup>` | adds user params (≤ 64 floats) at group 0 binding 1: `var<uniform> params: array<vec4<f32>, N>`, packed in order, zero-padded to vec4s; an f32 holds 24 exact integer bits — pass 32-bit masks as two 16-bit halves |
| `rx.create_texture_bind_group(texture) -> Option<ScriptBindGroup>` | texture + nearest sampler, for any texture group slot |
| `encoder.view_bind_group(view_id) -> Result<ScriptBindGroup>` | shade-stage: the view's **live** layer texture as input — unrecorded paints included, unlike `view_pixels`; built per call, so resizes are tracked next frame; binding a view as input to a pass targeting it is a validation error (disables the plugin) |
| `rx.create_compute_bind_group(inputs, output) -> Option<ScriptBindGroup>` | 1–4 input textures + storage output; must match the pipeline's declared count; compute reads raw (no sRGB decode) |
| `rx.create_sprite_vertices(texture, dst, color, opacity) -> Option<ScriptBuffer>` | one quad mapping the whole texture onto `dst` (target pixels); six vertices |
| `rx.create_sprite_vertices_src(texture, src, dst) -> Option<ScriptBuffer>` | maps only `src` (texture pixels) onto `dst`; white, full opacity |

ScriptBuffer: `count() -> i64` (vertex count, for `draw`).

## GPU: passes (the `shade` encoder)

`shade(state, rx, encoder)` records passes on the frame's command
encoder. One pass is open at a time: beginning a new pass auto-ends
its predecessors, and any pass left open is ended when the hook
returns. `load` is `"load"` (keep) or `"clear"` (to transparent).

| Function | Target |
|---|---|
| `encoder.begin_render_pass(label, texture, load)` | a script texture |
| `encoder.begin_view_pass(label, view_id, load)` | a view's layer — follow with `rx.touch_view(id)` so the edit is recorded (and undoable) |
| `encoder.begin_staging_pass(label, view_id, load)` | the view's staging overlay: composited above the view, cleared every frame — for uncommitted previews |
| `encoder.begin_compute_pass(label)` | |

ScriptPass: `set_pipeline(p)`, `set_bind_group(i, g)`,
`set_vertex_buffer(slot, b)`, `draw(vertices, instances)`, `end()`.
ScriptComputePass: `set_pipeline(p)`, `set_bind_group(i, g)`,
`dispatch(x, y, z)`, `end()`.

The `render(state, rx, pass)` hook receives a ScriptPass over the
screen: same methods, screen-sized target (`rx.screen_size()`),
re-begun per hook so errors attribute to the plugin that recorded
them.

## Types & constructors (free functions, `rx::`)

| Function | Notes |
|---|---|
| `rx::rgb(r, g, b) -> Rgba8` | alpha 255; fields `.r .g .b .a` readable |
| `rx::rect(x1, y1, x2, y2) -> Rect` | fields `.x1 .y1 .x2 .y2` |
| `rx::mat4_identity()` / `mat4_translation(x, y)` / `mat4_scale(sx, sy)` / `mat4_rotation_z(theta)` | |
| `rx::mat4_mul(a, b)` / `rx::mat4_transform_point(m, x, y)` | |
| `rx::atan2(y, x)` | radians |

## Conventions & pitfalls

- **Rect coords are y-up, pixel bytes are y-down.** `view_pixels` /
  selection rects and quad `dst` rects use y-up view coordinates;
  the returned (and uploaded) byte buffers are y-down rows. Painting
  at dst y=10 in a 128-high view lands in byte row 117. Selection
  rects from reversed drags arrive normalized.
- **Snapshot vs live.** `view_pixels` reads the *recorded snapshot*:
  it sees a mutation only after the view was touched and the frame
  recorded — split mutate and read into separate commands a few
  frames apart. `encoder.view_bind_group` is the live read: it sees
  same-frame, unrecorded paint.
- **`texture_pixels` is synchronous** and sees only submitted work:
  a readback in the same frame as a shade/render mutation reads stale
  pixels. Same split applies.
- **GPU errors disable the plugin.** Every hook runs inside a
  validation error scope; a GPU validation error (or script error)
  disables the plugin, posts one message, and the frame survives.
- **Alpha blending cannot write transparency.** To erase, redraw the
  sampler's erase.
- **Function arity caps at 5 slots** including the receiver — API
  functions take at most 4 arguments after `rx`.
- **Limits**: texture dimension ≤ 8192; texture groups 0–3; compute
  inputs 1–4; params ≤ 64 floats.
