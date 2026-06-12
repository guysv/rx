# Rune scripting: plan for phases 4+

This document covers the work from phase 4 onward on the `rune` branch:
embedding [Rune](https://rune-rs.github.io) and rebuilding the plugin system
on top of it. The old Rhai implementation on `master` is the *reference*, not
the template — `master:src/script.rs` (~2300 lines) is a complete catalog of
the API surface a plugin system needs, discovered over ~60 commits of
dogfooding. This time the API is built against that spec, in an order that
avoids the retrofits the first iteration needed.

## Where the branch stands (phases 1–3, done)

- wgpu port + engine-neutral fixes cherry-picked from `master`; renderer is
  `master`'s renderer minus all scripting (single `Texture` type, no
  re-exports, surface optional).
- The e2e harness runs headless on macOS (`cargo test --no-default-features`),
  in parallel, in ~5s. Digests are wgpu/Metal baselines, re-recorded and
  visually verified. `RX_DUMP_FRAMES=<dir>` dumps distinct frames as PNGs for
  triaging any future digest mismatch.
- 28 unit tests + 18 integration tests green (incl. a hand-written
  `flood` test — see the TDD section).

## Architecture principles

These are settled (the borrow model was validated experimentally) and every
phase below assumes them:

1. **Borrows in, no globals.** Rune native functions and hook calls receive
   `&mut` references to host state directly. Around *session/host state*
   there is no `Rc<RefCell<...>>` object registry, no session-as-global, and
   no queued two-phase command dispatch (`master:src/session.rs:785`, `:2557`
   document what we're *not* rebuilding) — shared mutable cells there hide
   undo/damage invariants. At the *wgpu boundary* shared ownership is fine
   (see the GPU section): wgpu objects are Arc-backed, invariant-free from
   the host's perspective, and self-validating.
2. **One context object.** Hooks take a single `rx` context argument exposing
   `session`, `gfx`, `cmd` (etc.) rather than N loose parameters. Hook
   signatures stay stable as the API grows, and hooks that need both the
   session and the renderer (`render`/`shade` did, last time) are expressible
   without double-`&mut` gymnastics.
3. **Compile errors are a feature.** Rune compiles to bytecode units with
   real diagnostics. Plugins are compiled on load/reload, errors land in the
   message line with file/line/span, and a `:plugin-check` command compiles
   without loading. Ship type-hint stubs so plugin authors get LSP support.
4. **f64 everywhere** at the script boundary (Rune's native numeric types are
   i64/f64). Geometry crosses the boundary as the `gfx` types (`Rect`,
   `Point`, `Rgba8`), not tuples.
5. **Determinism.** Nothing in the script API reads wall-clock time or OS
   randomness. Time comes from the session frame clock (`rx.time`), so plugin
   behavior is replayable and digest-testable.

## The scripting API: as built in Rhai, and its Rune shape

### The Rhai model (master, as-built)

Execution/state model — the parts that shaped everything else:

- One `rhai::Engine` + `Scope` **per plugin**. Plugin state = global variables
  in the plugin's scope, mutated across calls.
- `session` and `renderer` pushed into every scope as
  `Rc<RefCell<Session>>` / `Rc<RefCell<Renderer>>` globals — the borrow
  workaround that drove the queued command dispatch.
- Hooks are **optional magic-named exports**; "function not found" is
  silently OK (`a4b0efb`).
- Meta-plugins: cross-plugin calls are host-mediated — a `"plugin::fn"`
  string resolves to a `PluginFnRef`, invoked through the target plugin's
  engine (`get_plugin_fn` / `invoke`).

Hook catalog (exact names and payloads on master):

| Hook | Payload | Called from |
|---|---|---|
| `init()` / `unload()` | — | plugin (re)load / unload |
| `draw()` | — | per frame, UI batch population |
| `shade(encoder)` | command encoder | per frame, before view passes |
| `render(pass)` | render pass | per frame, screen composition |
| `view_added(id)` / `view_removed(id)` | view id | session events |
| `mouse_input(state, button, point)` | input | event dispatch |
| `mouse_wheel(delta)` / `cursor_moved(point)` | input | event dispatch |
| `switch_mode()` | — | mode transitions |
| `cmd_<name>(args) -> bool` | string args array | command dispatch |

API inventory (registered functions, grouped; names verbatim from
`master:src/script.rs`):

- **Session**: getters `mode`, `prev_mode`, `active_view_id`, `selection`,
  `fg`, `cursor`, `offset`, `zoom`, `keys_pressed`, `frame_width/height`;
  `views()`, `view(id)`, `switch_mode`, `script_mode`, `key_pressed`,
  `get_setting`, `init_setting`, `touch_active_view`,
  `active_view_coords` / `active_view_sub_coords`.
- **Commands**: `register_command(name, help)` (handler resolved as
  `cmd_<name>`), `run_builtin(invocation)`, `repeat`.
- **Effects/interop**: `queue_effect`, `effect_view_damaged`,
  `effect_view_paint_final`, `queue_active_view_rect_clear`,
  `upload_selection_to_texture`.
- **Drawing**: `draw_text`, `draw_line` (with color), `zdepth`.
- **GPU**: `create_shader_module`, `create_render_pipeline(_with_texture)`,
  `create_compute_pipeline`, `create_render_texture`,
  `create_compute_texture`, `create_buffer`, `create_sampler`,
  `create_texture_sampler_bind_group`,
  `create_(ortho_(custom_))transform_bind_group`,
  `create_view_transform_bind_group`,
  `create_vertex_buffer_from_sprite_vertices`, `view_render_texture`,
  `view_staging_texture`, `ensure_render_texture_size`, `sprite_pipeline`,
  begin render/compute pass on the encoder.
- **Math/types**: `vec2`, `rect`, `rgb8`, `mat4_identity/_translation/`
  `_scale/_rotation_z/_transform_point2`, `shape_rectangle`.
- **Misc**: `read_file`, `store_store`/`load_load`/`load_clear`
  (state persistence), debug printing.

### The Rune shape

The same surface, restructured around three changes: state is an object the
host holds, context is an argument, and GPU work is recorded rather than
handed live objects.

**State: a plugin is a struct.** Rune has no mutable scope globals. `init`
constructs and returns the plugin's state; the host stores it and passes it
back to every hook as the receiver. Hooks become methods — which also makes
`save_state`/`restore_state` (hot reload) and per-plugin isolation natural.

Rhai (master), abridged from `mode-vis.rxx`:

```rhai
let label = "";                          // scope global = plugin state

fn init() {
    session.init_setting("mode-vis/on", "on");   // session is a global
    register_command("mode-vis/toggle", "...");
}
fn switch_mode() { label = session.mode; }       // mutate scope global
fn draw() { draw_text(label, 8.0, 8.0, rgb8(255, 255, 255)); }
fn cmd_mode_vis_toggle(args) { ... ; true }
```

Rune (proposed):

```rune
struct ModeVis { label }

pub fn init(rx) {
    rx.settings.declare("mode-vis/on", true);
    rx.cmd.register("mode-vis/toggle", [], "Toggle the mode display");
    ModeVis { label: "" }
}

impl ModeVis {
    pub fn switch_mode(self, rx)     { self.label = rx.session.mode(); }
    pub fn draw(self, rx)            { rx.draw.text(self.label, 8.0, 8.0, WHITE); }
    pub fn mode_vis_toggle(self, rx, args) { ... }
}
```

**Context: `rx` replaces the globals.** Every hook takes `rx` after `self`.
`rx.session` is a live `&mut` for the duration of the call — *not* storable.
Collections stay snapshot/ID-based exactly like master (`views()` returns
lightweight copies; mutation goes through session methods by id): Rune's
borrow guards are call-scoped, so live references cannot leak into state.

**GPU: direct calls on real wgpu objects.** Scripts call wgpu methods
directly — there is no record/replay channel. This is viable because wgpu
22+ (we ship 23) made resources (`Device`, `Queue`, `Buffer`, `Texture`,
pipelines) Arc-backed and `Clone`, and `RenderPass`/`ComputePass` gain
`'static` lifetimes via `forget_lifetime()` — so Rune wrapper types can
*own* them. Shared-ownership wrappers (`Rc<RefCell>`) are acceptable at
this boundary, unlike around `Session`: wgpu objects carry no host
invariants (undo, damage tracking) and validate misuse themselves at
runtime. The one rule: the stage pass handed to `shade`/`render` lives in
`Rc<RefCell<Option<Pass>>>` and the host `take()`s it when the hook
returns — a script that stores it gets a clean "pass ended" error on next
use, never a wedged encoder.

Mapping summary:

| Rhai (master) | Rune (plan) |
|---|---|
| scope globals | state struct returned by `init`, host-held |
| `session`/`renderer` globals | `rx` ctx argument (call-scoped `&mut`) |
| magic exports, not-found-is-OK | methods on the state struct; same optionality |
| `cmd_<name>` name mangling | typed registration; handler is a method |
| `register_command(name, help)` | `rx.cmd.register(name, sig, help)` (typed args, completion) |
| `shade(encoder)`/`render(pass)` live objects | same, via owned `'static` wrappers (`forget_lifetime`), host-ended at hook return |
| `get_plugin_fn`/`invoke` string refs | host-mediated registry (same idea, typed handle) |
| `store_store`/`load_load` | `save_state`/`restore_state` on the struct |
| `vec2`/`rect`/`rgb8`/`mat4_*` helpers | `rx::gfx` types registered natively (`Rect`, `Point`, `Rgba8`, `Mat4`) |

Note on the renderer borrow: `shade`/`render` run inside
`Renderer::frame(&mut self)`, so a hook can never receive `&mut Renderer`
(a Rust self-borrow conflict, independent of the scripting engine). With
direct wgpu access this stops mattering: `rx.gfx` is clones of
`Device`/`Queue` plus the script resource registry — hooks don't need the
renderer at all, and master's frame-as-handle refactor (`30b817b`) has no
successor.

## Phase plan

Each phase ends green: `cargo test --no-default-features` passes, and phases
that add observable behavior add a test (unit test or recorded replay).

### Part B — Rune skeleton

**P4. Embed Rune.**
Add the `rune` dependency (pick the latest release at implementation time and
pin it). Load a single script, compile it, call `init()`, surface diagnostics
in the message line. Hot reload from day one: watch the plugin dir, recompile
on change, rebuild script state (ref: `18c98f8`, `5c3b55a` on master).
- Deliverable: a `hello.rune` that prints to the message line on load and
  reload.
- Test: unit test compiling + calling a script fixture; error-path test that
  a syntax error produces a diagnostic and does not abort the session.

**P5. Context type & borrow contract.**
Define the `Ctx` type passed to every hook, wire `init(rx)` end-to-end with
`&mut Session` reachable through it. This is a build step, not a spike — the
borrow mechanics are proven. Decide here, in code: how `Ctx` is constructed
per call, what its fields are at this stage (`session` only), and how it
grows (adding `gfx` in P14 must not break existing plugins).
- Deliverable: `init` can read and mutate real session state (e.g. set a
  setting, post a message).
- Test: unit test asserting a script-driven session mutation.

**P6. Plugin model.**
Plugin discovery and lifecycle: default plugin dir + `:plugin-dir/open`
(`e182508`), plugins keyed by name in a map (`3b47a74` — do it this way from
the start), optional callbacks (`a4b0efb`), `unload` event (`03fbd18`),
deterministic load order (lexicographic).
Packaging: a plugin is a *directory* (`plugins/foo/` with `foo.rune`, WGSL
files, assets). The real plugins were always file pairs; make that the unit.
Isolation: a plugin that fails to compile or panics in a hook is disabled and
reported; the host and sibling plugins are unaffected.
Reload: state dies on reload, but plugins may implement `save_state()` /
`restore_state(v)` to survive iteration.
- Test: replay test loading a fixture plugin via `Options.plugin_dir`
  (add the field to `Options`; `tests/main.rs` passes `None` — this is the
  one-line diff master already carries).

### Part C — Session & input API

**P7. Session read/write API.**
The `rx.session` module: `views()`, `active_view_id`, `mode`, settings get
*and* set, selection get/set (refs: `9ed48ed`, `eaab517`, `58a47bb`,
`8215de7`, `f54cc05`). Plugins declare settings they own (typed, with
defaults) — declared settings are `:set`-able and completable like builtins.

**P8. Events.**
Hook catalog with payloads, as methods on the plugin state struct (see the
API section above; absent method = not subscribed, same optionality as
Rhai's not-found-is-OK): view lifecycle (`view_added`/`view_removed`),
input (mouse, cursor, key, `keys_pressed` query), mode change, frame
`update`. Define and document ordering (script handlers run before builtin
handling — `660ebce` was the hard-won ordering) and re-entrancy rules (a
hook may call `switch_mode`, may not recursively dispatch events).

**P9. Drawing API.**
`rx.draw`: text, colored text, lines, shapes into the UI overlay batches
(refs: `057bbd6`, `87bcff8`, `d744379`). The shared-batch plumbing on master
existed because Rhai couldn't borrow the renderer — revisit whether plain
`&mut` access through `Ctx` makes the `Rc` sharing unnecessary.

**P10. Validation plugin: mode-vis.**
Port `mode-vis.rxx` (29 lines — shows the current mode on screen). It
exercises P7–P9; any gap it finds is a P7–P9 bug, fix there.
- Test: recorded replay with the plugin loaded; mode changes show up in
  digests.

### Part D — Commands, modes, bindings

**P11. Command registration, declarative.**
`rx.cmd.register(name, signature, help, handler)` with typed parameters —
parsing, `:help` listing, and completion fall out of the declaration. This
replaces master's stringly `(name, args)` commands plus the `cmd_*`
name-mangling fallback (`7ab1326`) — keep the unknown-command fallback
behavior, lose the mangling. Commands may declare `repeating` (`49c1d8e`).
`:command/default` (delegate to builtin — `session.rs:3236` on master) is
part of this phase.

**P12. Script modes.**
Custom modes with `switch_mode`/`prev_mode` (`29615dc`, `0fabf2e`,
`6615d02`), the no-view-passing-on-click rule in script modes (`867f086`).
Escape already exits any non-normal mode via the catch-all (P3 carried that
fix) — script modes inherit it for free; add the replay test proving it.

**P13. Bindings.**
`:map/script` + binding tiers + repeat semantics + mouse-down rules
(`7132224`, `f513d47`, `49c1d8e`, `5c705cb`). The tier-resolution logic and
its unit tests on master (`session.rs:3497`, `:3549`) port nearly verbatim —
bring the tests first. Scripts can also bind keys directly at registration
(`rx.cmd.bind`), not only via `:map/script` in config.
- Test: ported unit tests + a replay test for script-tier binding override.

### Part E — GPU API

The renderer half of every GPU feature already exists from the wgpu port;
these phases add the binding layer. Hook points get *names* — the frame is a
small fixed graph (`compute → views → shade → ui → present`) and plugins
attach to named stages instead of overloading hardcoded callbacks.

**P14. Renderer exposure & textures.**
`Ctx` grows `gfx`. Texture create/destroy with handles, upload from CPU
(refs: `c33ed88`, `7e54b01`, `49d4ff3`). Ownership: script textures belong
to the plugin and are destroyed on its unload/reload — decide the mid-frame
destruction rule here (defer to frame end).

**P15. Render passes & shaders.**
WGSL shader loading (with hot reload alongside script reload), `shade` stage
callbacks, `begin_render_pass`, texture/uniform bind groups — the dynamic
bind-group API was the right shape (`209a941`); make it the only path
(refs: `fcaa028`→`545d37d` arc, `ab150a1`).

**P16. Validation plugin: selection-outline.**
Port the 68-line driver; the WGSL copies unchanged.
- Test: replay with selection changes; outline is digest-visible.

**P17. Compute.**
`begin_compute_pass`, 2D/3D compute textures, dispatch
(refs: `108d5f6`, `e9ad95b`, `d9f1bc3`).

**P18. Selection/view interop.**
`upload_selection_to_texture`, `queue_active_view_rect_clear`,
`effect_view_damaged` (refs: `baa859a`, `4ff1a6e`, `6615d02`), and the
*write-back* path: a script edit to view pixels is an undoable transaction —
define the snapshot/undo integration explicitly instead of inheriting it
accidentally like the rotate tool did.

### Part F — Plugin ports (each is an API audit)

**P19. cleanedge** — first compute/shader consumer (`af7c7c9` arc; WGSL done).
**P20. Meta plugins** — introduce only when cleanedge/rotsprite need shared
rotation UI. Mechanism: host-mediated registry (a plugin exposes an interface
object, others look it up by name) rather than cross-unit Rune imports —
prototype the import route first and use it only if it's clearly better
(refs: `0df3be5`, `a765ac6`).
**P21. mmpx + rotsprite** (`1c2f69c`, `d26dc5b`, `e8d6d38`).
**P22. rotate-scale** — the boss fight: 496 lines exercising modes, commands,
bindings, pivot, compute, selection write-back (the 17-commit arc ending
`e3417da`). Budget API-gap fixes here; every gap fixed lands with a test.
- Each port ships a `tests/` replay directory. GPU plugin output is exactly
  what pixel digests are good at.

## Testing policy & TDD workflow

### Tests are authored, not recorded

`.events` files are plain ASCII — `FRAME DELTA event args` — so e2e tests
can be written by hand *before* the feature exists. The event vocabulary:

```
00000 0000010 cursor/moved 639 359
00020 0000100 keyboard/input z pressed
00040 0000200 char/received ':'
00060 0000300 mouse/input pressed
```

(`keyboard/input` fires key bindings; `char/received` feeds the command
line; `mouse/input` is always the left button; coordinates are window
logical pixels — window center is the centered view's center.)

The loop, per test:

1. **Red.** Write `tests/<name>/` (`.rx` setup, `.events` scenario,
   `.toml`) and the `#[test]` entry. Run verify — it fails (missing digest,
   or the behavior visibly absent in frames).
2. **Implement & inspect.** Replay with `RX_DUMP_FRAMES=<dir>`, read the
   distinct frames as PNGs, confirm the behavior is actually what was
   intended. *Recording is "capture verified output", never "make red
   green"* — a digest is only recorded after this inspection.
3. **Green.** `rx --headless --replay tests/<name> --record-digests
   --width .. --height .. -u tests/<name>/<name>.rx`, commit the digest.
   The behavior is locked from then on.

Honest caveat: step 1's red is weaker than classic TDD — before the first
recording there is no assertion to fail against, only a missing digest.
Strengthen it by designing the scenario so a missing feature is *visible*:
end on a state that differs only if the feature worked (see the undo trick
below), or route through an error path that prints to the message line.

### Worked example: the `flood` test (written for this plan)

`flood.rs` had 0% coverage — no recording ever exercised it. A 9-line
hand-written events file fixed that (`flood.rs` → 95%):

- `flood.rx`: UI off, `map z :v/center`, `map f :flood`, `map u :undo`,
  and `v/fill #333333` so the canvas is *visibly* gray.
- `flood.events`: cursor to window center → `z` → `f` → click → `u`.
- Distinct frames: gray view → flood-filled view → gray again. The undo
  restoring gray proves the fill painted (and is undoable) even though
  white-on-white is hard to eyeball — design scenarios so state changes
  are loud (pre-fill with a contrasting color; default `fg` is white and
  there is no `:fg` command).

Two design lessons baked into the workflow: make every state transition
produce a *visually distinct* frame (hash-distinct is not enough for the
human inspection step), and end scenarios with an inverse operation
(undo/esc) so the test asserts the round-trip, not just the final image.

### TDD per phase

- **P4–P5 (engine, Ctx):** classic Rust unit TDD — fixtures in
  `tests/scripts/`, assertions on compile errors, diagnostics, and
  script-driven session mutations. No digests involved.
- **P6 (plugin model):** first plugin-loading replay test, written before
  the loader: `Options.plugin_dir` pointing at a fixture plugin whose
  `init` posts to the message line (`set ui/message = on` in the `.rx` so
  it's digest-visible). Unload/reload covered by unit tests driving the
  watcher path directly.
- **P7–P10 (session API):** unit tests per binding as it lands; the
  mode-vis replay test is written at the *start* of P10 against the
  not-yet-ported plugin — it stays red through P10 and going green is the
  phase's exit criterion.
- **P11–P13 (commands, modes, bindings):** port master's binding-tier unit
  tests (`session.rs:3497`–`:3560`) *first*. Replay tests type script
  commands through the command line (`char/received` events) and exercise
  `:map/script` keybinds — written per feature before the dispatch code.
- **P14–P18 (GPU):** unit tests assert registry/handle bookkeeping
  (create/destroy/reload ownership) without a device where possible.
  Behavior is digest-tested through the validation plugins — for each GPU
  feature the corresponding plugin replay (selection-outline for render
  passes, cleanedge for compute) is the acceptance test, written when the
  phase starts.
- **P19–P22 (plugin ports):** each port begins by writing its replay
  scenario from the old plugin's *intended* behavior (not from running
  master — the point is to encode intent, catch translation bugs). The
  rotate-scale scenario should include: enter mode, rotate, switch
  scale/rotation, change pivot, commit, undo — the undo-roundtrip pattern
  from the flood test, applied to the hardest feature.

### Digest rules

- Digests are recorded on macOS/Metal and are backend-specific. CI verifies
  digests on one pinned platform; other platforms build + run unit tests.
- Any digest change must be triaged with `RX_DUMP_FRAMES` before
  re-recording. Re-record because behavior changed *on purpose*, never to
  make red green.
- Keep `.rx` setups minimal and note in the test dir (or the `.rx` header
  comment) what the scenario exercises.
- Scripts are deterministic by construction (no wall clock / RNG in the API),
  so plugin replays are as stable as core replays.

## Open questions (decide in the phase that hits them)

- **Rune version & API**: pin at P4; check `Vm::call` ergonomics for guarded
  `&mut` args against the version pinned.
- **Ctx growth**: field per module (`rx.session`, `rx.gfx`) vs. flat methods —
  pick at P5 and stay consistent.
- **GPU misuse policy** (P15): exact error surface when a plugin keeps a
  stage pass or encoder alive past its hook (message-line error + plugin
  disable vs. error only), and whether `Device`/`Queue` clones are exposed
  to all hooks or only GPU stages.
- **Meta-plugin mechanism**: registry vs. cross-unit imports (P20). The Rune
  unit/Vm model (one shared `Context`, one `Unit` + `Vm` per plugin) makes
  cross-unit function values awkward — registry is the default assumption.
- **Mid-frame GPU resource destruction** (P14).
- **Whether the user-batch `Rc` sharing survives** once `Ctx` carries the
  renderer (P9/P15).

## Appendix: reference map

For any phase, the implementation reference is `master` (Rhai-era). Useful
anchors:

| Area | Reference |
|---|---|
| Full API inventory | `master:src/script.rs` |
| Command dispatch & queueing (what to *avoid*) | `master:src/session.rs:785`, `:2557` |
| Binding tiers + tests | `master:src/session.rs:3497`-`3560` |
| Renderer script hooks (wgpu side) | `master:src/wgpu/mod.rs` (~160 `script` references) |
| Plugins to port | `master:plugins/` (`mode-vis`, `selection-outline`, `cleanedge`, `mmpx`, `rotsprite-gl`, `rotate-scale`) |
| Commit-by-commit history | `git log upstream/master..master` (107 commits, see buckets in project notes) |
