# Layer status overlay: plan for phases 39+

This document covers the work after `docs/layers-plan.md` (P35–P38,
complete): **`plugins/layer-status/`**, a screen-space overlay showing
the active view's layer stack as a two-column table at the right edge
of the screen, vertically centered — one row per layer, a lock icon and
a visibility icon per row, the active layer marked. Commands show and
hide it.

This is the plugin customer the deferred layers script tier (P39 in the
layers plan) was waiting for, so the API gets designed here —
demand-driven, sized to what the overlay actually reads, nothing
speculative. The plan reclaims the P39 numbering.

The feature smokes out one core gap before any API work: **there is no
lock attribute**. P38 built visibility and opacity; "locked" must exist
in core (with real write-blocking semantics, or the icon would lie)
before a plugin can display it.

**Icons: TODO** — the icon assets (lock/unlock, visible/hidden) are
user-provided and arrive before P42 starts. Nothing in P39–P41 blocks
on them.

## Decisions made up front

- **Lock means pixel protection, nothing more.** A locked layer refuses
  pixel writes (brush, fill, erase, paste, flood, `p/*` paints,
  `v/clear`) and refuses ops that would rewrite or destroy its strip
  (`layer/merge` when either strip is locked, `layer/flatten` when any
  visible layer is locked, `layer/remove` of a locked layer). It does
  *not* block: activation, reorder (pure position), `layer/dup`
  (reads), visibility/opacity, or undo/redo — history stays sovereign
  over attributes, the P38 precedent.
- **The gate lives in the session, not the renderer.** Write-blocking
  happens at the user-facing entry points (tool engagement and the
  effect-emitting command handlers) with a message-line refusal —
  the renderer stays policy-free, and the staging preview never shows
  a stroke that won't commit.
- **`locked` joins `LayerAttrs`** — presentation/protection state like
  `visible`: not recorded in history, not undoable, not persisted.
- **Script API is read-only.** The overlay reads layer state; it
  mutates nothing (and lifecycle mutations already work via
  `run_builtin("layer/...")`). No mutation API until a plugin needs
  one.
- **Row order: topmost layer first** — the table reads like the visual
  stack, Aseprite-style. Rows are numbered with the same 1-based
  indices the `layer N/M` messages use, so the table and the message
  line never disagree (the top row of an n-layer view reads `n`).
- **Tiers compose by frame order**: row numbers and the active-row
  marker are `draw`-hook text/lines (UI tier); icons are textured
  quads in the `render` hook, which draws after the UI tier
  (`compute → views → shade → ui → render → present`, P23). Placement
  math is shared: `rx.screen_size()`, right edge, vertically centered,
  remembering UI coords are y-down while render-stage orthos are
  script-built.

## Phase plan

Each phase ends green: `cargo test --no-default-features` passes, with
unit tests or a recorded replay per the house rules (digest hygiene,
events format, and the verify-frames-before-recording practice from
P36–P38 — and its key-binding finding: test keys must dodge the
mode-specific defaults like `h`/`l`).

### Part L — the lock (core)

**P39. The lock attribute & gate.**
`LayerAttrs` gains `locked: bool` (default false); `:layer/lock [n]`,
`:layer/unlock [n]` (default: active layer), messages in the
established voice (`layer 2 locked`). A session-side
`active_layer_locked()` check gates, with an Error-message refusal:
brush engagement (`start_drawing`), `selection/fill|erase|paste`,
flood fill, `v/clear`, and the `p/*` paint commands. Structural
refusals per the decisions above. The status bar indicator grows the
lock marker for the active layer (e.g. `L2/4*` — exact glyph TBD with
the icons; single-layer unlocked views stay byte-identical, the
standing digest rule).
- Test: unit (attrs lifecycle through extend/shrink/reset, the refusal
  matrix for merge/flatten/remove); replay — lock the top layer, paint
  over it, erase, fill: composite unchanged throughout (the digest
  proof that nothing landed), unlock, paint, change visible. Error
  messages are `MessageType::Error` → they log at error level, which
  `record-digest` refuses — the refusal feedback in the *replay* is
  the unchanged composite plus Info-level lock/unlock messages, and
  the Error path is asserted in unit tests instead.

### Part M — the script tier & the plugin

**P40. Layer state for scripts.**
The read slice, sized to the overlay: `ViewInfo` grows `layers`
(count) and `active_layer` (the P27 expose-what-View-has pattern), and
a new `rx.layer_attrs(id) -> Vec` returns per-layer
`#{ visible, locked, opacity }` in layer order (index 0 = bottom, the
same order `layer/set` addresses). No new hooks: the overlay derives
edges in `update` by comparing against its cached state — the standing
edge-not-counter rule keeps digests clean anyway. Routed-compat
semantics from the layers plan Status section get their contractual
write-up in `docs/script-api.md` alongside the new functions.
- Test: extend `tests/script-view` (or a sibling replay) with a fixture
  plugin reading `layers`/`active_layer`/`layer_attrs` across
  `:layer/add`, `:layer/next`, `:layer/hide`, `:layer/lock` and posting
  probes to the message line; plugin suite green on 1-layer views.

**P41. Binary assets for plugins.**
`rx.read_file` returns a `String` — fine for WGSL, useless for icons.
Add `rx.read_png(path) -> Option<(w, h, Bytes)>`: plugin-dir-relative
like `read_file`, decoding through the existing PNG load path
(`src/image.rs`), rgba8 row-major — `upload`-ready for a script
texture. Loud on failure via the message line, like the other
constructors.
- Test: unit — roundtrip a fixture PNG through read_png → upload →
  `texture_pixels`; error path (missing file → None + message).

**P42. The plugin (`plugins/layer-status/`).**
The overlay itself, gated on the icon assets (TODO) landing in the
plugin directory:
- State: shown/hidden flag, cached `(view id, layers, active,
  attrs)` snapshot for change detection; `view_removed` prunes.
- Commands: `lo/show`, `lo/hide`, `lo/toggle` — registered with help
  strings; show/hide also make natural `map` targets. (`ls/*` is taken
- Render: `init` loads the icon PNGs (P41) into two script textures
  (or one atlas — decided by the assets' shape); the `render` hook
  draws one icon pair per layer at the right edge, vertically
  centered on `rx.screen_size()`, topmost layer first; the `draw`
  hook adds row numbers and the active-row marker. Hidden overlay =
  hooks return early; nothing is drawn, nothing is cached stale.
- Per-frame draws of an unchanged overlay hash to identical frames
  (the P34 finding), so no edge-gating is needed for digests — only
  for message-line noise, of which the overlay produces none.
- Test: `plugins/layer-status/test/` e2e replay — build a 3-layer
  view, `lo/show`, cycle active, hide a layer, lock a layer, `lo/hide`
  — each step digest-distinct via the overlay pixels themselves; icon
  fixtures checked in with the test.

## Open questions

- **Auto-hide on single-layer views?** Default: the overlay shows a
  one-row table when enabled — honest, and the enable/disable is
  already one command. Revisit only if it annoys in practice.
- **Click-to-toggle** (mouse on the overlay flipping
  visibility/lock): explicitly out of scope — the overlay is
  read-only; `mouse_input` + row hit-testing makes it a natural
  later phase if wanted.
- **Overlay styling knobs** (margin, scale, position): none for now;
  `declare_setting` makes them cheap to add when someone asks.
- **Lock and the script write path**: `clear_view_rect` and script
  view passes bypass the session gate by design (plugins are power
  tools). Documented in P40's script-api write-up rather than
  blocked; revisit if a plugin ever wants the gate applied to itself.

## Appendix: reference map

| Area | Reference |
|---|---|
| LayerAttrs & attr lifecycle (extend in P39) | `src/view.rs` (`LayerAttrs`, `extend_layer`/`shrink_layer`/`reset`) |
| Session write entry points (gate in P39) | `src/session.rs` — `start_drawing` site, `SelectionFill/Erase/Paste`, flood at `Tool::FloodFill`, `Command::Paint*`, `v/clear` |
| Structural ops (refusals in P39) | `src/session.rs` `Command::LayerMerge/LayerFlatten/LayerRemove` handlers |
| Status bar indicator (extend in P39) | `src/draw.rs` (the `L{}/{}` block) |
| De-facto layered-view script semantics (contract in P40) | `docs/layers-plan.md` Status section |
| PNG load path (reuse in P41) | `src/image.rs`, `io::load_image` |
| read_file precedent (mirror in P41) | `src/script.rs` (`read_file`, plugin-dir-relative) |
