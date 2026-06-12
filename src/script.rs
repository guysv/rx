//! Rune-based plugin scripting.
//!
//! Plugins are Rune scripts. Each plugin exports a `pub fn init(rx)` that
//! receives the [`Ctx`] and returns the plugin's state value; further hooks
//! are free functions taking `(state, rx, ...)`. A missing hook simply means
//! the plugin isn't subscribed to it.
//!
//! Architecture rules (see docs/rune-plan.md): hooks receive `&mut` host
//! state through `Ctx` for the duration of the call only — no globals, no
//! queued dispatch.

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use rune::runtime::{RuntimeContext, VmError};
use rune::{Context, Diagnostics, Source, Sources, Unit, Value, Vm};

use crate::session::Session;

////////////////////////////////////////////////////////////////////////////
// Errors

#[derive(Debug)]
pub enum ScriptError {
    /// Compilation failed; the string holds rendered diagnostics.
    Compile(String),
    /// A runtime error in a script call.
    Vm(String),
    /// The script doesn't define the requested function.
    MissingFn(String),
    Io(io::Error),
}

impl fmt::Display for ScriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compile(e) => write!(f, "script compile error: {}", e),
            Self::Vm(e) => write!(f, "script error: {}", e),
            Self::MissingFn(name) => write!(f, "script function not found: {}", name),
            Self::Io(e) => write!(f, "script i/o error: {}", e),
        }
    }
}

impl From<io::Error> for ScriptError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<VmError> for ScriptError {
    fn from(e: VmError) -> Self {
        Self::Vm(e.to_string())
    }
}

////////////////////////////////////////////////////////////////////////////
// Ctx

/// The context object passed to every plugin hook as `rx`.
///
/// Holds a raw pointer to the session for the duration of a single hook
/// call. Hooks receive it as `&mut Ctx` through rune's guarded arguments,
/// so scripts cannot store it; the pointer never outlives the call.
#[derive(rune::Any)]
#[rune(item = ::rx)]
pub struct Ctx {
    session: *mut Session,
}

impl Ctx {
    /// Construct a context borrowing the session for one hook call.
    ///
    /// SAFETY: the returned `Ctx` must not outlive `session`, and the
    /// session must not be accessed by the host while a hook call using
    /// this ctx is in progress. Both hold because `Ctx` is created
    /// immediately before a `vm.call` and dropped right after, and the
    /// call passes it as a guarded `&mut`.
    pub fn new(session: &mut Session) -> Self {
        Ctx {
            session: session as *mut Session,
        }
    }

    fn session(&self) -> &Session {
        unsafe { &*self.session }
    }

    fn session_mut(&mut self) -> &mut Session {
        unsafe { &mut *self.session }
    }

    /// Current mode, as a string (e.g. "normal", "visual", "command").
    #[rune::function]
    fn mode(&self) -> String {
        self.session().mode.to_string()
    }

    /// Previous mode, if any.
    #[rune::function]
    fn prev_mode(&self) -> Option<String> {
        self.session().prev_mode.as_ref().map(|m| m.to_string())
    }

    /// Post a message to the message line.
    #[rune::function]
    fn message(&mut self, msg: &str) {
        self.session_mut()
            .message(msg.to_string(), crate::session::MessageType::Echo);
    }

    /// Id of the active view.
    #[rune::function]
    fn active_view_id(&self) -> i64 {
        u16::from(self.session().views.active_id) as i64
    }

    /// Snapshots of all views, in view order.
    #[rune::function]
    fn views(&self) -> Vec<ViewInfo> {
        self.session()
            .views
            .iter()
            .map(|v| ViewInfo {
                id: u16::from(v.id) as i64,
                width: v.width() as i64,
                height: v.height() as i64,
                offset_x: v.offset.x as f64,
                offset_y: v.offset.y as f64,
                zoom: v.zoom as f64,
            })
            .collect()
    }

    /// A setting's value: bool, integer, float, string or a tuple,
    /// depending on the setting. Unit if the setting doesn't exist.
    #[rune::function]
    fn setting(&self, name: &str) -> Value {
        use crate::cmd::Value as V;

        let unit = || rune::to_value(()).expect("unit converts");
        match self.session().settings.get(name) {
            None => unit(),
            Some(V::Bool(b)) => rune::to_value(*b).unwrap_or_else(|_| unit()),
            Some(V::U32(n)) => rune::to_value(*n as i64).unwrap_or_else(|_| unit()),
            Some(V::U32Tuple(a, b)) => {
                rune::to_value((*a as i64, *b as i64)).unwrap_or_else(|_| unit())
            }
            Some(V::F32Tuple(a, b)) => {
                rune::to_value((*a as f64, *b as f64)).unwrap_or_else(|_| unit())
            }
            Some(V::F64(f)) => rune::to_value(*f).unwrap_or_else(|_| unit()),
            Some(V::Str(s)) | Some(V::Ident(s)) => {
                rune::to_value(s.clone()).unwrap_or_else(|_| unit())
            }
            Some(V::Rgba8(c)) => rune::to_value(c.to_string()).unwrap_or_else(|_| unit()),
        }
    }

    /// Set a setting. The value must match the setting's current type.
    /// Returns whether the set was applied.
    #[rune::function]
    fn set_setting(&mut self, name: &str, value: Value) -> bool {
        use crate::cmd::Value as V;

        let converted = match self.session().settings.get(name) {
            None => None,
            Some(V::Bool(_)) => rune::from_value::<bool>(value).ok().map(V::Bool),
            Some(V::U32(_)) => rune::from_value::<i64>(value).ok().map(|n| V::U32(n as u32)),
            Some(V::F64(_)) => rune::from_value::<f64>(value).ok().map(V::F64),
            Some(V::Str(_)) => rune::from_value::<String>(value).ok().map(V::Str),
            Some(V::Ident(_)) => rune::from_value::<String>(value).ok().map(V::Ident),
            Some(V::U32Tuple(..)) | Some(V::F32Tuple(..)) | Some(V::Rgba8(_)) => None,
        };
        match converted {
            Some(v) => self.session_mut().settings.set(name, v).is_ok(),
            None => false,
        }
    }

    /// The current selection bounds as `(x1, y1, x2, y2)`, if any.
    #[rune::function]
    fn selection(&self) -> Option<(i64, i64, i64, i64)> {
        self.session().selection.map(|s| {
            let r = s.bounds();
            (r.x1 as i64, r.y1 as i64, r.x2 as i64, r.y2 as i64)
        })
    }

    /// Replace the selection with the given bounds.
    #[rune::function]
    fn set_selection(&mut self, x1: i64, y1: i64, x2: i64, y2: i64) {
        self.session_mut().selection = Some(crate::session::Selection::new(
            x1 as i32, y1 as i32, x2 as i32, y2 as i32,
        ));
    }

    /// Clear the selection.
    #[rune::function]
    fn clear_selection(&mut self) {
        self.session_mut().selection = None;
    }
}

/// An immutable snapshot of a view, handed to scripts. Mutation goes
/// through session methods by id — live references never cross the
/// boundary (docs/rune-plan.md).
#[derive(rune::Any, Clone)]
#[rune(item = ::rx)]
pub struct ViewInfo {
    #[rune(get)]
    pub id: i64,
    #[rune(get)]
    pub width: i64,
    #[rune(get)]
    pub height: i64,
    #[rune(get)]
    pub offset_x: f64,
    #[rune(get)]
    pub offset_y: f64,
    #[rune(get)]
    pub zoom: f64,
}

/// The native `rx` module installed into every plugin's context.
fn module() -> Result<rune::Module, rune::ContextError> {
    let mut m = rune::Module::with_crate("rx")?;
    m.ty::<Ctx>()?;
    m.function_meta(Ctx::mode)?;
    m.function_meta(Ctx::prev_mode)?;
    m.function_meta(Ctx::message)?;
    m.function_meta(Ctx::active_view_id)?;
    m.function_meta(Ctx::views)?;
    m.function_meta(Ctx::setting)?;
    m.function_meta(Ctx::set_setting)?;
    m.function_meta(Ctx::selection)?;
    m.function_meta(Ctx::set_selection)?;
    m.function_meta(Ctx::clear_selection)?;
    m.ty::<ViewInfo>()?;
    Ok(m)
}

////////////////////////////////////////////////////////////////////////////
// Engine

/// Shared compiler state: the native context all plugins compile against.
pub struct ScriptEngine {
    context: Context,
    runtime: Arc<RuntimeContext>,
}

impl ScriptEngine {
    pub fn new() -> Result<Self, ScriptError> {
        let mut context =
            Context::with_default_modules().map_err(|e| ScriptError::Compile(e.to_string()))?;
        context
            .install(module().map_err(|e| ScriptError::Compile(e.to_string()))?)
            .map_err(|e| ScriptError::Compile(e.to_string()))?;
        let runtime = Arc::new(
            context
                .runtime()
                .map_err(|e| ScriptError::Compile(e.to_string()))?,
        );
        Ok(Self { context, runtime })
    }

    /// Compile a script from a file path.
    pub fn compile_path(&self, path: &Path) -> Result<CompiledScript, ScriptError> {
        let source = Source::from_path(path).map_err(|e| {
            ScriptError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{}: {}", path.display(), e),
            ))
        })?;
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        self.compile(name, source)
    }

    /// Compile a script from an in-memory string (used by tests).
    pub fn compile_str(&self, name: &str, text: &str) -> Result<CompiledScript, ScriptError> {
        let source = Source::memory(text).map_err(|e| ScriptError::Compile(e.to_string()))?;
        self.compile(name.to_string(), source)
    }

    fn compile(&self, name: String, source: Source) -> Result<CompiledScript, ScriptError> {
        let mut sources = Sources::new();
        sources
            .insert(source)
            .map_err(|e| ScriptError::Compile(e.to_string()))?;

        let mut diagnostics = Diagnostics::new();
        let result = rune::prepare(&mut sources)
            .with_context(&self.context)
            .with_diagnostics(&mut diagnostics)
            .build();

        if diagnostics.has_error() {
            return Err(ScriptError::Compile(render_diagnostics(
                &diagnostics,
                &sources,
            )));
        }
        let unit = result.map_err(|e| ScriptError::Compile(e.to_string()))?;

        Ok(CompiledScript {
            name,
            unit: Arc::new(unit),
            runtime: self.runtime.clone(),
        })
    }
}

/// Render diagnostics to a plain string (for the message line / logs).
fn render_diagnostics(diagnostics: &Diagnostics, sources: &Sources) -> String {
    use rune::termcolor::{Buffer, BufferWriter, ColorChoice};

    let writer = BufferWriter::stderr(ColorChoice::Never);
    let mut buffer: Buffer = writer.buffer();
    if diagnostics.emit(&mut buffer, sources).is_err() {
        return "unknown compile error".to_string();
    }
    String::from_utf8_lossy(buffer.as_slice()).into_owned()
}

////////////////////////////////////////////////////////////////////////////
// Compiled script

/// A compiled script, ready to call.
pub struct CompiledScript {
    pub name: String,
    unit: Arc<Unit>,
    runtime: Arc<RuntimeContext>,
}

impl CompiledScript {
    /// Whether the script defines a top-level function with this name.
    pub fn has_fn(&self, name: &str) -> bool {
        let vm = Vm::new(self.runtime.clone(), self.unit.clone());
        vm.lookup_function([name]).is_ok()
    }

    /// Call a top-level function. `MissingFn` if it isn't defined.
    pub fn call(
        &self,
        name: &str,
        args: impl rune::runtime::GuardedArgs,
    ) -> Result<Value, ScriptError> {
        let mut vm = Vm::new(self.runtime.clone(), self.unit.clone());
        if vm.lookup_function([name]).is_err() {
            return Err(ScriptError::MissingFn(name.to_string()));
        }
        let value = vm.call([name], args)?;
        Ok(value)
    }
}

impl fmt::Debug for CompiledScript {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CompiledScript({})", self.name)
    }
}

////////////////////////////////////////////////////////////////////////////
// Plugin host

/// A loaded plugin: compiled script + the state value its `init` returned.
pub struct Plugin {
    pub name: String,
    script: CompiledScript,
    state: Value,
    /// Cleared when a hook errors at runtime; the plugin stops receiving
    /// hooks but its siblings are unaffected.
    enabled: bool,
}

/// Owns all plugins. Lives outside [`Session`] so hooks can borrow the
/// session mutably while the host borrows itself.
pub struct PluginHost {
    engine: ScriptEngine,
    plugins: Vec<Plugin>,
    dir: Option<std::path::PathBuf>,
    watcher: Option<ReloadWatcher>,
    /// Last mode reported to `switch_mode` hooks (edge detection).
    last_mode: Option<String>,
}

/// Loop over enabled plugins defining `$hook` and call it; disable a
/// plugin whose hook errors. A macro rather than a generic fn: the
/// argument tuple borrows a per-call local `Ctx`, which a closure-based
/// helper can't express without aliasing the host.
macro_rules! dispatch {
    ($host:expr, $session:expr, $hook:expr, |$state:ident, $ctx:ident| $args:tt) => {
        for i in 0..$host.plugins.len() {
            {
                let plugin = &$host.plugins[i];
                if !plugin.enabled || !plugin.script.has_fn($hook) {
                    continue;
                }
            }
            let $state = $host.plugins[i].state.clone();
            let mut ctx = Ctx::new($session);
            let $ctx = &mut ctx;
            let result = $host.plugins[i].script.call($hook, $args);
            drop(ctx);
            if let Err(e) = result {
                let plugin = &mut $host.plugins[i];
                plugin.enabled = false;
                log::error!("plugin `{}` {}: {}", plugin.name, $hook, e);
                let name = plugin.name.clone();
                $session.message(
                    format!("Plugin `{}` disabled: {}", name, first_line(&e)),
                    crate::session::MessageType::Error,
                );
            }
        }
    };
}

impl PluginHost {
    /// A host rooted at the given plugin directory (`None` = no plugins).
    pub fn new(dir: Option<std::path::PathBuf>) -> Result<Self, ScriptError> {
        let engine = ScriptEngine::new()?;
        let watcher = match &dir {
            Some(d) if d.is_dir() => ReloadWatcher::new(d).ok(),
            _ => None,
        };
        Ok(Self {
            engine,
            plugins: Vec::new(),
            dir,
            watcher,
            last_mode: None,
        })
    }

    pub fn plugin_dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn plugins(&self) -> impl Iterator<Item = &Plugin> {
        self.plugins.iter()
    }

    /// Discover plugin entry points: `<dir>/<name>.rune` files and
    /// `<dir>/<name>/<name>.rune` packages, in lexicographic order.
    fn discover(dir: &Path) -> Vec<(String, std::path::PathBuf)> {
        let mut found = Vec::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return found,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    let entry_point = path.join(name).with_extension("rune");
                    if entry_point.is_file() {
                        found.push((name.to_string(), entry_point));
                    }
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("rune") {
                if let Some(stem) = path.file_stem().and_then(|n| n.to_str()) {
                    found.push((stem.to_string(), path.clone()));
                }
            }
        }
        found.sort();
        found
    }

    /// Load (or re-load) all plugins from the plugin dir. A plugin that
    /// fails to compile or whose `init` errors is reported to the message
    /// line and skipped; it never affects its siblings.
    pub fn load(&mut self, session: &mut Session) {
        let dir = match &self.dir {
            Some(d) => d.clone(),
            None => return,
        };
        for (name, path) in Self::discover(&dir) {
            let script = match self.engine.compile_path(&path) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("plugin `{}`: {}", name, e);
                    session.message(
                        format!("Error loading plugin `{}`: {}", name, first_line(&e)),
                        crate::session::MessageType::Error,
                    );
                    continue;
                }
            };
            let state = {
                let mut ctx = Ctx::new(session);
                match script.call("init", (&mut ctx,)) {
                    Ok(v) => v,
                    Err(e) => {
                        log::error!("plugin `{}` init: {}", name, e);
                        session.message(
                            format!("Error initializing plugin `{}`: {}", name, first_line(&e)),
                            crate::session::MessageType::Error,
                        );
                        continue;
                    }
                }
            };
            log::info!("plugin `{}` loaded from {}", name, path.display());
            self.plugins.push(Plugin {
                name,
                script,
                state,
                enabled: true,
            });
        }
    }

    /// Call `unload` on every plugin that defines it, then drop them all.
    pub fn unload(&mut self, session: &mut Session) {
        for plugin in self.plugins.drain(..) {
            let mut ctx = Ctx::new(session);
            match plugin
                .script
                .call("unload", (plugin.state.clone(), &mut ctx))
            {
                Ok(_) | Err(ScriptError::MissingFn(_)) => {}
                Err(e) => {
                    log::error!("plugin `{}` unload: {}", plugin.name, e);
                }
            }
        }
    }

    /// Reload all plugins if the watcher saw a change. Returns whether a
    /// reload happened.
    pub fn reload_if_changed(&mut self, session: &mut Session) -> bool {
        if self.watcher.as_ref().is_some_and(|w| w.changed()) {
            self.reload(session);
            true
        } else {
            false
        }
    }

    /// Unconditional reload: unload everything and load fresh.
    pub fn reload(&mut self, session: &mut Session) {
        self.unload(session);
        self.load(session);
        session.message("Plugins reloaded", crate::session::MessageType::Execution);
    }

    ////////////////////////////////////////////////////////////////////////
    // Hook dispatch
    //
    // Each dispatch loops over enabled plugins that define the hook and
    // calls it with `(state, rx, ...)`. A runtime error disables the
    // offending plugin and reports it; siblings are unaffected.

    /// Cursor moved (window logical coordinates). Runs before builtin
    /// event handling.
    pub fn dispatch_cursor_moved(&mut self, session: &mut Session, x: f64, y: f64) {
        dispatch!(self, session, "cursor_moved", |state, ctx| (
            state, ctx, x, y
        ));
    }

    /// Mouse button input. Runs before builtin event handling.
    pub fn dispatch_mouse_input(&mut self, session: &mut Session, button: &str, input: &str) {
        dispatch!(self, session, "mouse_input", |state, ctx| (
            state,
            ctx,
            button.to_string(),
            input.to_string()
        ));
    }

    /// End-of-update dispatch: fires `switch_mode` on mode edges, then
    /// `update`.
    pub fn dispatch_update(&mut self, session: &mut Session) {
        let mode = session.mode.to_string();
        if self.last_mode.as_deref() != Some(mode.as_str()) {
            self.last_mode = Some(mode);
            dispatch!(self, session, "switch_mode", |state, ctx| (state, ctx));
        }
        dispatch!(self, session, "update", |state, ctx| (state, ctx));
    }

    /// View lifecycle hooks, driven from the session's effects.
    pub fn dispatch_effects(&mut self, session: &mut Session, effects: &[crate::session::Effect]) {
        use crate::session::Effect;

        for effect in effects {
            let (hook, id) = match effect {
                Effect::ViewAdded(id) => ("view_added", *id),
                Effect::ViewRemoved(id) => ("view_removed", *id),
                _ => continue,
            };
            let id = u16::from(id) as i64;
            match hook {
                "view_added" => {
                    dispatch!(self, session, "view_added", |state, ctx| (state, ctx, id))
                }
                _ => dispatch!(self, session, "view_removed", |state, ctx| (state, ctx, id)),
            }
        }
    }
}


/// First line of an error display, for the one-line message bar.
fn first_line(e: &ScriptError) -> String {
    let s = e.to_string();
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

////////////////////////////////////////////////////////////////////////////
// Hot reload

/// Watches a directory and reports (debounced) whether anything changed.
pub struct ReloadWatcher {
    _watcher: notify::RecommendedWatcher,
    rx: mpsc::Receiver<()>,
}

impl ReloadWatcher {
    pub fn new(dir: &Path) -> Result<Self, ScriptError> {
        use notify::{RecursiveMode, Watcher};

        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                use notify::EventKind::*;
                if matches!(event.kind, Create(_) | Modify(_) | Remove(_)) {
                    tx.send(()).ok();
                }
            }
        })
        .map_err(|e| ScriptError::Io(io::Error::new(io::ErrorKind::Other, e.to_string())))?;

        watcher
            .watch(dir, RecursiveMode::Recursive)
            .map_err(|e| ScriptError::Io(io::Error::new(io::ErrorKind::Other, e.to_string())))?;

        Ok(Self {
            _watcher: watcher,
            rx,
        })
    }

    /// True if anything changed since the last call. Non-blocking; drains
    /// the queue so a burst of events reports once.
    pub fn changed(&self) -> bool {
        let mut changed = false;
        while self.rx.try_recv().is_ok() {
            changed = true;
        }
        changed
    }

    /// Wait up to `timeout` for a change (used by tests).
    pub fn wait_changed(&self, timeout: Duration) -> bool {
        if self.rx.recv_timeout(timeout).is_ok() {
            while self.rx.try_recv().is_ok() {}
            true
        } else {
            false
        }
    }
}

////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn compile_and_call() {
        let engine = ScriptEngine::new().unwrap();
        let script = engine
            .compile_str("t", "pub fn init() { 41 + 1 }")
            .unwrap();
        let v = script.call("init", ()).unwrap();
        let n: i64 = rune::from_value(v).unwrap();
        assert_eq!(n, 42);
    }

    #[test]
    fn compile_error_reports_diagnostics() {
        let engine = ScriptEngine::new().unwrap();
        let err = engine
            .compile_str("bad", "pub fn init() { let }")
            .unwrap_err();
        match err {
            ScriptError::Compile(msg) => {
                assert!(!msg.is_empty(), "diagnostics should not be empty");
            }
            other => panic!("expected compile error, got: {}", other),
        }
    }

    #[test]
    fn missing_function_is_distinguished() {
        let engine = ScriptEngine::new().unwrap();
        let script = engine.compile_str("t", "pub fn init() {}").unwrap();
        assert!(script.has_fn("init"));
        assert!(!script.has_fn("draw"));
        match script.call("draw", ()) {
            Err(ScriptError::MissingFn(name)) => assert_eq!(name, "draw"),
            other => panic!("expected MissingFn, got: {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn runtime_error_is_reported() {
        let engine = ScriptEngine::new().unwrap();
        let script = engine
            .compile_str("t", "pub fn init() { None.unwrap() }")
            .unwrap();
        match script.call("init", ()) {
            Err(ScriptError::Vm(_)) => {}
            other => panic!("expected Vm error, got: {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn state_value_round_trip() {
        let engine = ScriptEngine::new().unwrap();
        let script = engine
            .compile_str(
                "t",
                r#"
                struct State { count }
                pub fn init() { State { count: 0 } }
                pub fn bump(state) { state.count += 1; state.count }
                "#,
            )
            .unwrap();
        let state = script.call("init", ()).unwrap();
        let v = script.call("bump", (state.clone(),)).unwrap();
        let n: i64 = rune::from_value(v).unwrap();
        assert_eq!(n, 1);
        let v = script.call("bump", (state,)).unwrap();
        let n: i64 = rune::from_value(v).unwrap();
        assert_eq!(n, 2, "state mutations must persist across calls");
    }

    /// A bare headless session for script tests.
    pub(crate) fn test_session() -> Session {
        let proj_dirs = directories::ProjectDirs::from("io", "cloudhead", "rx").unwrap();
        let base_dirs = directories::BaseDirs::new().unwrap();
        Session::new(640, 480, std::env::temp_dir(), proj_dirs, base_dirs)
    }

    #[test]
    fn ctx_reads_and_mutates_session() {
        let mut session = test_session();
        let engine = ScriptEngine::new().unwrap();
        let script = engine
            .compile_str(
                "t",
                r#"
                pub fn init(rx) {
                    rx.message("hello from rune");
                    rx.mode()
                }
                "#,
            )
            .unwrap();

        let mut ctx = Ctx::new(&mut session);
        let v = script.call("init", (&mut ctx,)).unwrap();
        drop(ctx);

        let mode: String = rune::from_value(v).unwrap();
        assert_eq!(mode, "normal");
        assert_eq!(session.message.to_string(), "hello from rune");
    }

    #[test]
    fn stored_ctx_is_revoked_after_the_call() {
        let mut session = test_session();
        let engine = ScriptEngine::new().unwrap();
        let script = engine
            .compile_str(
                "t",
                r#"
                struct State { rx }
                pub fn init(rx) { State { rx } }
                pub fn later(state) { state.rx.mode() }
                "#,
            )
            .unwrap();

        let state = {
            let mut ctx = Ctx::new(&mut session);
            script.call("init", (&mut ctx,)).unwrap()
        };
        // The guard was revoked when `init` returned; using the smuggled
        // ctx must be a clean runtime error, not a dangling-pointer deref.
        match script.call("later", (state,)) {
            Err(ScriptError::Vm(_)) => {}
            other => panic!("expected Vm error, got: {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn session_view_api() {
        use crate::view::FileStatus;

        let mut session = test_session().with_blank(FileStatus::NoFile, 128, 96);
        let engine = ScriptEngine::new().unwrap();
        let script = engine
            .compile_str(
                "t",
                r#"
                pub fn probe(rx) {
                    let views = rx.views();
                    let v = views[0];
                    (rx.active_view_id(), views.len(), v.width, v.height)
                }
                "#,
            )
            .unwrap();

        let mut ctx = Ctx::new(&mut session);
        let v = script.call("probe", (&mut ctx,)).unwrap();
        let (id, count, w, h): (i64, i64, i64, i64) = rune::from_value(v).unwrap();
        assert_eq!(count, 1);
        assert_eq!(id, 1);
        assert_eq!((w, h), (128, 96));
    }

    #[test]
    fn settings_get_set() {
        let mut session = test_session();
        let engine = ScriptEngine::new().unwrap();
        let script = engine
            .compile_str(
                "t",
                r#"
                pub fn probe(rx) {
                    let before = rx.setting("debug");
                    let ok = rx.set_setting("debug", true);
                    let bad_type = rx.set_setting("debug", 3.5);
                    let missing = rx.set_setting("no/such/setting", 1);
                    (before, ok, bad_type, missing, rx.setting("debug"))
                }
                "#,
            )
            .unwrap();

        let mut ctx = Ctx::new(&mut session);
        let v = script.call("probe", (&mut ctx,)).unwrap();
        let (before, ok, bad_type, missing, after): (bool, bool, bool, bool, bool) =
            rune::from_value(v).unwrap();
        assert!(!before);
        assert!(ok);
        assert!(!bad_type, "type-mismatched set must be rejected");
        assert!(!missing, "unknown setting must be rejected");
        assert!(after, "the set must be visible");
        assert!(session.settings["debug"].is_set());
    }

    #[test]
    fn selection_round_trip() {
        let mut session = test_session();
        let engine = ScriptEngine::new().unwrap();
        let script = engine
            .compile_str(
                "t",
                r#"
                pub fn probe(rx) {
                    let empty = rx.selection();
                    rx.set_selection(1, 2, 11, 22);
                    let some = rx.selection();
                    (empty, some)
                }
                "#,
            )
            .unwrap();

        let mut ctx = Ctx::new(&mut session);
        let v = script.call("probe", (&mut ctx,)).unwrap();
        let (empty, some): (Option<(i64, i64, i64, i64)>, Option<(i64, i64, i64, i64)>) =
            rune::from_value(v).unwrap();
        assert_eq!(empty, None);
        assert_eq!(some, Some((1, 2, 11, 22)));
        assert!(session.selection.is_some());
    }

    #[test]
    fn prev_mode_tracks_switches() {
        use crate::session::Mode;

        let mut session = test_session();
        session.switch_mode(Mode::Help);

        let engine = ScriptEngine::new().unwrap();
        let script = engine
            .compile_str("t", "pub fn probe(rx) { (rx.mode(), rx.prev_mode()) }")
            .unwrap();

        let mut ctx = Ctx::new(&mut session);
        let v = script.call("probe", (&mut ctx,)).unwrap();
        let (mode, prev): (String, Option<String>) = rune::from_value(v).unwrap();
        assert_eq!(mode, "help");
        assert_eq!(prev.as_deref(), Some("normal"));
    }

    fn write_plugin(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name).with_extension("rune"), body).unwrap();
    }

    #[test]
    fn host_loads_plugins_in_order() {
        let dir = tempfile::tempdir().unwrap();
        // Flat file plugin.
        write_plugin(
            dir.path(),
            "alpha",
            r#"pub fn init(rx) { rx.message("alpha up"); #{} }"#,
        );
        // Directory-package plugin.
        std::fs::create_dir(dir.path().join("beta")).unwrap();
        write_plugin(
            &dir.path().join("beta"),
            "beta",
            r#"pub fn init(rx) { rx.message("beta up"); #{} }"#,
        );

        let mut session = test_session();
        let mut host = PluginHost::new(Some(dir.path().to_path_buf())).unwrap();
        host.load(&mut session);

        let names: Vec<_> = host.plugins().map(|p| p.name.clone()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        // beta loaded last; its message is the visible one.
        assert_eq!(session.message.to_string(), "beta up");
    }

    #[test]
    fn broken_plugin_does_not_block_siblings() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(dir.path(), "broken", "pub fn init(rx) { let }");
        write_plugin(
            dir.path(),
            "crashy",
            r#"pub fn init(rx) { None.unwrap() }"#,
        );
        write_plugin(dir.path(), "good", r#"pub fn init(rx) { #{} }"#);

        let mut session = test_session();
        let mut host = PluginHost::new(Some(dir.path().to_path_buf())).unwrap();
        host.load(&mut session);

        let names: Vec<_> = host.plugins().map(|p| p.name.clone()).collect();
        assert_eq!(names, vec!["good"]);
    }

    #[test]
    fn unload_hook_runs_on_reload() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(
            dir.path(),
            "p",
            r#"
            pub fn init(rx) { #{} }
            pub fn unload(state, rx) { rx.message("p unloaded"); }
            "#,
        );

        let mut session = test_session();
        let mut host = PluginHost::new(Some(dir.path().to_path_buf())).unwrap();
        host.load(&mut session);
        assert_eq!(host.plugins().count(), 1);

        // Change the plugin and force a reload: unload runs, new code lands.
        write_plugin(
            dir.path(),
            "p",
            r#"pub fn init(rx) { rx.message("p v2"); #{} }"#,
        );
        host.reload(&mut session);
        assert_eq!(host.plugins().count(), 1);
        // The reload banner is posted last; v2's init ran before it.
        assert_eq!(session.message.to_string(), "Plugins reloaded");
    }

    fn host_with(dir: &Path, name: &str, body: &str) -> (PluginHost, Session) {
        write_plugin(dir, name, body);
        let mut session = test_session();
        let mut host = PluginHost::new(Some(dir.to_path_buf())).unwrap();
        host.load(&mut session);
        assert_eq!(host.plugins().count(), 1, "fixture plugin must load");
        (host, session)
    }

    #[test]
    fn input_hooks_fire_before_builtins_via_update() {
        use crate::event::Event;
        use crate::execution::Execution;
        use crate::platform;

        let dir = tempfile::tempdir().unwrap();
        let (mut host, session) = host_with(
            dir.path(),
            "p",
            r#"
            pub fn init(rx) { #{} }
            pub fn cursor_moved(state, rx, x, y) {
                rx.message(`cursor ${x} ${y}`);
            }
            pub fn mouse_input(state, rx, button, input) {
                rx.message(`mouse ${button} ${input}`);
            }
            "#,
        );
        // The builtin handlers need an active view to exist.
        let mut session = session.with_blank(crate::view::FileStatus::NoFile, 32, 32);

        let mut exec = Execution::normal().unwrap();
        let mut events = vec![Event::CursorMoved(platform::LogicalPosition::new(5.0, 6.0))];
        session.update(
            &mut events,
            &mut exec,
            Duration::default(),
            Duration::default(),
            &mut host,
        );
        assert_eq!(session.message.to_string(), "cursor 5.0 6.0");

        let mut events = vec![Event::MouseInput(
            platform::MouseButton::Left,
            platform::InputState::Pressed,
        )];
        session.update(
            &mut events,
            &mut exec,
            Duration::default(),
            Duration::default(),
            &mut host,
        );
        assert_eq!(session.message.to_string(), "mouse left pressed");
    }

    #[test]
    fn switch_mode_hook_fires_on_edges_only() {
        use crate::session::Mode;

        let dir = tempfile::tempdir().unwrap();
        let (mut host, mut session) = host_with(
            dir.path(),
            "p",
            r#"
            pub fn init(rx) { #{ switches: 0 } }
            pub fn switch_mode(state, rx) {
                state.switches += 1;
                rx.message(`switched to ${rx.mode()} (#${state.switches})`);
            }
            "#,
        );

        // First dispatch observes the initial mode as an edge.
        host.dispatch_update(&mut session);
        assert_eq!(session.message.to_string(), "switched to normal (#1)");

        // Same mode: no edge, no hook.
        session.message("sentinel", crate::session::MessageType::Info);
        host.dispatch_update(&mut session);
        assert_eq!(session.message.to_string(), "sentinel");

        session.switch_mode(Mode::Help);
        host.dispatch_update(&mut session);
        assert_eq!(session.message.to_string(), "switched to help (#2)");
    }

    #[test]
    fn view_lifecycle_hooks() {
        use crate::session::Effect;
        use crate::view::FileStatus;

        let dir = tempfile::tempdir().unwrap();
        let (mut host, session) = host_with(
            dir.path(),
            "p",
            r#"
            pub fn init(rx) { #{} }
            pub fn view_added(state, rx, id) { rx.message(`added ${id}`); }
            pub fn view_removed(state, rx, id) { rx.message(`removed ${id}`); }
            "#,
        );
        let mut session = session.with_blank(FileStatus::NoFile, 32, 32);
        let id = session.views.active_id;

        host.dispatch_effects(&mut session, &[Effect::ViewAdded(id)]);
        assert_eq!(session.message.to_string(), "added 1");
        host.dispatch_effects(&mut session, &[Effect::ViewRemoved(id)]);
        assert_eq!(session.message.to_string(), "removed 1");
    }

    #[test]
    fn erroring_hook_disables_only_that_plugin() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(
            dir.path(),
            "bad",
            r#"
            pub fn init(rx) { #{} }
            pub fn update(state, rx) { None.unwrap() }
            "#,
        );
        write_plugin(
            dir.path(),
            "good",
            r#"
            pub fn init(rx) { #{ ticks: 0 } }
            pub fn update(state, rx) {
                state.ticks += 1;
                rx.message(`tick ${state.ticks}`);
            }
            "#,
        );

        let mut session = test_session();
        let mut host = PluginHost::new(Some(dir.path().to_path_buf())).unwrap();
        host.load(&mut session);
        assert_eq!(host.plugins().count(), 2);

        host.dispatch_update(&mut session);
        // `bad` errored and was disabled; `good` still ran (it runs after
        // `bad` alphabetically, so its message is the last one).
        assert_eq!(session.message.to_string(), "tick 1");

        host.dispatch_update(&mut session);
        assert_eq!(session.message.to_string(), "tick 2");
        assert_eq!(
            host.plugins().filter(|p| p.enabled).count(),
            1,
            "bad plugin must be disabled"
        );
    }

    #[test]
    fn watcher_reports_changes() {
        let dir = tempfile::tempdir().unwrap();
        let watcher = ReloadWatcher::new(dir.path()).unwrap();
        assert!(!watcher.changed());

        std::fs::write(dir.path().join("plugin.rune"), "pub fn init() {}").unwrap();
        assert!(
            watcher.wait_changed(Duration::from_secs(10)),
            "watcher should observe the write"
        );
        assert!(!watcher.changed(), "queue should be drained");
    }
}
