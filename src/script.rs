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
use rune::{Context, Diagnostics, Hash, Source, Sources, Unit, Value, Vm};

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

    /// Post a message to the message line.
    #[rune::function]
    fn message(&mut self, msg: &str) {
        self.session_mut()
            .message(msg.to_string(), crate::session::MessageType::Echo);
    }
}

/// The native `rx` module installed into every plugin's context.
fn module() -> Result<rune::Module, rune::ContextError> {
    let mut m = rune::Module::with_crate("rx")?;
    m.ty::<Ctx>()?;
    m.function_meta(Ctx::mode)?;
    m.function_meta(Ctx::message)?;
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
