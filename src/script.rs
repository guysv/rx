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

use rune::runtime::{Function, RuntimeContext, VmError};
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
// Script commands

/// A typed parameter of a script command.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ParamType {
    Int,
    Float,
    Str,
    Color,
    Bool,
}

impl ParamType {
    fn name(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Float => "float",
            Self::Str => "str",
            Self::Color => "color",
            Self::Bool => "bool",
        }
    }
}

#[derive(Clone, Debug)]
struct Param {
    ty: ParamType,
    optional: bool,
}

/// Parse a declared signature, e.g. `["int", "color?"]`. A `?` suffix
/// marks the parameter optional; optional parameters must come last.
fn parse_sig(sig: &[String]) -> Result<Vec<Param>, String> {
    let mut params: Vec<Param> = Vec::with_capacity(sig.len());
    for s in sig {
        let (name, optional) = match s.strip_suffix('?') {
            Some(n) => (n, true),
            None => (s.as_str(), false),
        };
        let ty = match name {
            "int" => ParamType::Int,
            "float" => ParamType::Float,
            "str" => ParamType::Str,
            "color" => ParamType::Color,
            "bool" => ParamType::Bool,
            other => return Err(format!("unknown parameter type `{}`", other)),
        };
        if !optional && params.last().is_some_and(|p| p.optional) {
            return Err("required parameter after optional parameter".to_string());
        }
        params.push(Param { ty, optional });
    }
    Ok(params)
}

/// `usage: <name> <int> [color]` — for dispatch-time argument errors.
fn usage(name: &str, params: &[Param]) -> String {
    let mut s = format!("usage: {}", name);
    for p in params {
        if p.optional {
            s.push_str(&format!(" [{}]", p.ty.name()));
        } else {
            s.push_str(&format!(" <{}>", p.ty.name()));
        }
    }
    s
}

/// Parse raw invocation arguments against a declared signature into Rune
/// values (i64, f64, String, Rgba8, bool).
fn parse_args(name: &str, params: &[Param], raw: &str) -> Result<Vec<Value>, String> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let required = params.iter().filter(|p| !p.optional).count();
    if tokens.len() < required || tokens.len() > params.len() {
        return Err(usage(name, params));
    }

    let mut out = Vec::with_capacity(tokens.len());
    for (tok, param) in tokens.iter().zip(params) {
        let value = match param.ty {
            ParamType::Int => tok
                .parse::<i64>()
                .ok()
                .and_then(|n| rune::to_value(n).ok()),
            ParamType::Float => tok
                .parse::<f64>()
                .ok()
                .and_then(|n| rune::to_value(n).ok()),
            ParamType::Str => rune::to_value(tok.to_string()).ok(),
            ParamType::Bool => match *tok {
                "on" | "true" => rune::to_value(true).ok(),
                "off" | "false" => rune::to_value(false).ok(),
                _ => None,
            },
            ParamType::Color => {
                // Guard the length: `Rgba8::from_str` slices bytes.
                if tok.len() == 7 && tok.starts_with('#') && tok.is_ascii() {
                    tok.parse::<crate::gfx::color::Rgba8>()
                        .ok()
                        .and_then(|c| rune::to_value(c).ok())
                } else {
                    None
                }
            }
        };
        match value {
            Some(v) => out.push(v),
            None => {
                return Err(format!(
                    "invalid {} `{}`; {}",
                    param.ty.name(),
                    tok,
                    usage(name, params)
                ))
            }
        }
    }
    Ok(out)
}

/// A command registered by a plugin.
pub struct ScriptCommand {
    pub name: String,
    pub help: String,
    pub repeating: bool,
    /// Owning plugin; handlers run with this plugin's state.
    pub plugin: String,
    params: Vec<Param>,
    handler: Function,
}

/// All commands registered by loaded plugins. Owned by [`PluginHost`];
/// reachable from hooks through [`Ctx`] for registration.
#[derive(Default)]
pub struct ScriptCommands {
    entries: Vec<ScriptCommand>,
}

impl ScriptCommands {
    fn register(&mut self, cmd: ScriptCommand) -> Result<(), String> {
        if let Some(existing) = self.entries.iter().find(|c| c.name == cmd.name) {
            return Err(format!(
                "command ':{}' is already registered by plugin `{}`",
                cmd.name, existing.plugin
            ));
        }
        self.entries.push(cmd);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&ScriptCommand> {
        self.entries.iter().find(|c| c.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ScriptCommand> {
        self.entries.iter()
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    /// `(name, help)` pairs for the help view.
    fn help_entries(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .map(|c| (c.name.clone(), c.help.clone()))
            .collect()
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
    /// Draw sink; non-null only during the `draw` hook.
    draw: *mut crate::draw::Context,
    /// Command registry; non-null only for calls made by the plugin host.
    cmds: *mut ScriptCommands,
    /// Name of the plugin whose hook is running (owner of registrations).
    plugin: String,
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
            draw: std::ptr::null_mut(),
            cmds: std::ptr::null_mut(),
            plugin: String::new(),
        }
    }

    /// A context that can also draw (used by the `draw` hook). Same
    /// safety contract as [`Ctx::new`], extended to `draw`.
    pub fn with_draw(session: &mut Session, draw: &mut crate::draw::Context) -> Self {
        Ctx {
            session: session as *mut Session,
            draw: draw as *mut crate::draw::Context,
            cmds: std::ptr::null_mut(),
            plugin: String::new(),
        }
    }

    /// Attach the command registry and the calling plugin's name. Same
    /// safety contract as [`Ctx::new`], extended to `cmds`.
    pub fn with_commands(mut self, cmds: &mut ScriptCommands, plugin: &str) -> Self {
        self.cmds = cmds as *mut ScriptCommands;
        self.plugin = plugin.to_string();
        self
    }

    fn session(&self) -> &Session {
        unsafe { &*self.session }
    }

    fn session_mut(&mut self) -> &mut Session {
        unsafe { &mut *self.session }
    }

    fn draw_mut(&mut self) -> Option<&mut crate::draw::Context> {
        if self.draw.is_null() {
            None
        } else {
            Some(unsafe { &mut *self.draw })
        }
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

    /// Switch the session mode. Builtin mode names (`normal`, `visual`,
    /// `command`, `present`, `help`) switch to the builtin mode; any
    /// other name enters a custom script mode (escape exits it, builtin
    /// input handling is inert in it). Returns whether the switch was
    /// accepted.
    #[rune::function]
    fn switch_mode(&mut self, name: &str) -> bool {
        use crate::session::{Mode, ModeString, VisualState};

        let mode = match name {
            "normal" => Mode::Normal,
            "visual" => Mode::Visual(VisualState::default()),
            "command" => Mode::Command,
            "present" => Mode::Present,
            "help" => Mode::Help,
            custom => match ModeString::try_from_str(custom) {
                Ok(s) if !s.is_empty() => Mode::Script(s),
                _ => {
                    self.session_mut().message(
                        format!("Error: invalid mode name `{}`", custom),
                        crate::session::MessageType::Error,
                    );
                    return false;
                }
            },
        };
        self.session_mut().switch_mode(mode);
        true
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

    /// Register a command: `rx.register_command(name, sig, help, handler)`.
    ///
    /// `sig` declares the typed parameters (`"int"`, `"float"`, `"str"`,
    /// `"color"`, `"bool"`; suffix `?` for optional). The handler is called
    /// as `handler(state, rx, args)` with `args` a vector of parsed values.
    /// Returns whether registration succeeded.
    #[rune::function]
    fn register_command(&mut self, name: &str, sig: Vec<String>, help: &str, handler: Function) -> bool {
        self.register_cmd(name, sig, help, handler, false)
    }

    /// Like `register_command`, but the command repeats while its bound
    /// key is held.
    #[rune::function]
    fn register_command_repeating(
        &mut self,
        name: &str,
        sig: Vec<String>,
        help: &str,
        handler: Function,
    ) -> bool {
        self.register_cmd(name, sig, help, handler, true)
    }

    fn register_cmd(
        &mut self,
        name: &str,
        sig: Vec<String>,
        help: &str,
        handler: Function,
        repeating: bool,
    ) -> bool {
        use crate::session::MessageType;

        if self.cmds.is_null() {
            self.session_mut().message(
                format!("Error: ':{}' cannot be registered in this context", name),
                MessageType::Error,
            );
            return false;
        }
        // Builtins always win at parse time; a shadowing registration
        // would never be dispatched, so reject it loudly.
        if self
            .session()
            .cmdline
            .commands
            .iter()
            .any(|(n, _, _)| *n == name)
        {
            self.session_mut().message(
                format!("Error: ':{}' would shadow a builtin command", name),
                MessageType::Error,
            );
            return false;
        }
        let params = match parse_sig(&sig) {
            Ok(p) => p,
            Err(e) => {
                self.session_mut().message(
                    format!("Error registering ':{}': {}", name, e),
                    MessageType::Error,
                );
                return false;
            }
        };
        let entry = ScriptCommand {
            name: name.to_string(),
            help: help.to_string(),
            repeating,
            plugin: self.plugin.clone(),
            params,
            handler,
        };
        let result = unsafe { &mut *self.cmds }.register(entry);
        match result {
            Ok(()) => true,
            Err(e) => {
                self.session_mut()
                    .message(format!("Error: {}", e), MessageType::Error);
                false
            }
        }
    }

    /// Run a builtin command, e.g. `rx.run_builtin("v/center")`. Script
    /// commands are not resolvable through this; it exists so handlers
    /// can delegate to (or compose) default behavior. Returns whether the
    /// invocation parsed and ran.
    #[rune::function]
    fn run_builtin(&mut self, invocation: &str) -> bool {
        use crate::cmd::Command;
        use crate::session::MessageType;

        let input = format!(":{}", invocation.trim());
        let session = self.session_mut();
        match session.cmdline.parse(&input) {
            Ok(Command::Script(name, _)) => {
                session.message(
                    format!("Error: ':{}' is not a builtin command", name),
                    MessageType::Error,
                );
                false
            }
            Ok(cmd) => {
                session.command(cmd);
                true
            }
            Err(e) => {
                session.message(format!("Error: {}", e), MessageType::Error);
                false
            }
        }
    }

    /// Draw text at `(x, y)` in UI coordinates. Only valid inside the
    /// `draw` hook; a no-op elsewhere.
    #[rune::function]
    fn draw_text(&mut self, text: &str, x: f64, y: f64, color: crate::gfx::color::Rgba8) {
        use crate::font::TextAlign;

        if let Some(draw) = self.draw_mut() {
            draw.text_batch.add(
                text,
                x as f32,
                y as f32,
                crate::draw::UI_LAYER,
                color,
                TextAlign::Left,
            );
        }
    }

    /// Draw a 1px line from `p1` to `p2` (`(x, y)` tuples) in UI
    /// coordinates. Only valid inside the `draw` hook; a no-op elsewhere.
    #[rune::function]
    fn draw_line(&mut self, p1: (f64, f64), p2: (f64, f64), color: crate::gfx::color::Rgba8) {
        use crate::gfx::math::Point2;
        use crate::gfx::shape2d::Shape;

        if let Some(draw) = self.draw_mut() {
            draw.ui_batch.add(
                Shape::line(
                    Point2::new(p1.0 as f32, p1.1 as f32),
                    Point2::new(p2.0 as f32, p2.1 as f32),
                )
                .stroke(1.0, color)
                .zdepth(crate::draw::UI_LAYER),
            );
        }
    }
}

/// Construct a color from RGB components (alpha 255).
#[rune::function]
fn rgb(r: i64, g: i64, b: i64) -> crate::gfx::color::Rgba8 {
    crate::gfx::color::Rgba8::new(r as u8, g as u8, b as u8, 0xff)
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
    m.function_meta(Ctx::switch_mode)?;
    m.function_meta(Ctx::message)?;
    m.function_meta(Ctx::active_view_id)?;
    m.function_meta(Ctx::views)?;
    m.function_meta(Ctx::setting)?;
    m.function_meta(Ctx::set_setting)?;
    m.function_meta(Ctx::selection)?;
    m.function_meta(Ctx::set_selection)?;
    m.function_meta(Ctx::clear_selection)?;
    m.function_meta(Ctx::register_command)?;
    m.function_meta(Ctx::register_command_repeating)?;
    m.function_meta(Ctx::run_builtin)?;
    m.function_meta(Ctx::draw_text)?;
    m.function_meta(Ctx::draw_line)?;
    m.function_meta(rgb)?;
    m.ty::<ViewInfo>()?;
    m.ty::<crate::gfx::color::Rgba8>()?;
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
    /// Commands registered by plugins (cleared on unload/reload).
    commands: ScriptCommands,
    dir: Option<std::path::PathBuf>,
    watcher: Option<ReloadWatcher>,
    /// Last mode reported to `switch_mode` hooks (edge detection).
    last_mode: Option<String>,
}

/// Loop over enabled plugins defining `$hook` and call it; disable a
/// plugin whose hook errors. A macro rather than a generic fn: the
/// argument tuple borrows a per-call local `Ctx`, which a closure-based
/// helper can't express without aliasing the host. `$plugins`/`$cmds`
/// are the host's fields, split-borrowed by the caller.
macro_rules! dispatch {
    ($plugins:expr, $cmds:expr, $session:expr, $hook:expr, |$state:ident, $ctx:ident| $args:tt) => {
        for i in 0..$plugins.len() {
            {
                let plugin = &$plugins[i];
                if !plugin.enabled || !plugin.script.has_fn($hook) {
                    continue;
                }
            }
            let $state = $plugins[i].state.clone();
            let name = $plugins[i].name.clone();
            let mut ctx = Ctx::new($session).with_commands(&mut *$cmds, &name);
            let $ctx = &mut ctx;
            let result = $plugins[i].script.call($hook, $args);
            drop(ctx);
            if let Err(e) = result {
                let plugin = &mut $plugins[i];
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
            commands: ScriptCommands::default(),
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
                let mut ctx = Ctx::new(session).with_commands(&mut self.commands, &name);
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
        session
            .cmdline
            .set_script_commands(self.commands.help_entries());
    }

    /// Call `unload` on every plugin that defines it, then drop them all
    /// along with their command registrations.
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
        self.commands.clear();
        session.cmdline.set_script_commands(Vec::new());
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
        let Self {
            plugins, commands, ..
        } = self;
        dispatch!(plugins, commands, session, "cursor_moved", |state, ctx| (
            state, ctx, x, y
        ));
    }

    /// Mouse button input. Runs before builtin event handling.
    pub fn dispatch_mouse_input(&mut self, session: &mut Session, button: &str, input: &str) {
        let Self {
            plugins, commands, ..
        } = self;
        dispatch!(plugins, commands, session, "mouse_input", |state, ctx| (
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
        let switched = self.last_mode.as_deref() != Some(mode.as_str());
        if switched {
            self.last_mode = Some(mode);
        }
        let Self {
            plugins, commands, ..
        } = self;
        if switched {
            dispatch!(plugins, commands, session, "switch_mode", |state, ctx| (
                state, ctx
            ));
        }
        dispatch!(plugins, commands, session, "update", |state, ctx| (
            state, ctx
        ));
    }

    /// Dispatch a script command invocation: resolve `name` in the
    /// registry, parse `raw` against the declared signature, and call the
    /// handler as `handler(state, rx, args)` with the owning plugin's
    /// state. Errors (unknown command, bad arguments, disabled plugin)
    /// land in the message line.
    pub fn dispatch_command(&mut self, session: &mut Session, name: &str, raw: &str) {
        use crate::session::MessageType;
        use rune::alloc::clone::TryClone;

        let (params, handler, plugin_name) = match self.commands.get(name) {
            Some(c) => match c.handler.try_clone() {
                Ok(handler) => (c.params.clone(), handler, c.plugin.clone()),
                Err(e) => {
                    session.message(format!("Error: ':{}': {}", name, e), MessageType::Error);
                    return;
                }
            },
            None => {
                session.message(
                    format!("Error: unknown command: {}", name),
                    MessageType::Error,
                );
                return;
            }
        };
        let args = match parse_args(name, &params, raw) {
            Ok(a) => a,
            Err(e) => {
                session.message(format!("Error: {}", e), MessageType::Error);
                return;
            }
        };
        let args = match rune::to_value(args) {
            Ok(v) => v,
            Err(e) => {
                session.message(format!("Error: {}", e), MessageType::Error);
                return;
            }
        };
        let Some(i) = self
            .plugins
            .iter()
            .position(|p| p.name == plugin_name && p.enabled)
        else {
            session.message(
                format!("Error: ':{}': plugin `{}` is not loaded", name, plugin_name),
                MessageType::Error,
            );
            return;
        };
        let state = self.plugins[i].state.clone();
        let Self {
            plugins, commands, ..
        } = self;
        let mut ctx = Ctx::new(session).with_commands(commands, &plugin_name);
        let result = handler
            .call::<Value>((state, &mut ctx, args))
            .into_result()
            .map_err(|e| ScriptError::Vm(e.to_string()));
        drop(ctx);
        if let Err(e) = result {
            let plugin = &mut plugins[i];
            plugin.enabled = false;
            log::error!("plugin `{}` :{}: {}", plugin.name, name, e);
            session.message(
                format!("Plugin `{}` disabled: {}", plugin_name, first_line(&e)),
                MessageType::Error,
            );
        }
    }

    /// Whether a registered script command repeats on key-hold.
    pub fn command_repeats(&self, name: &str) -> bool {
        self.commands.get(name).is_some_and(|c| c.repeating)
    }

    /// The script command registry (read access, e.g. for help).
    pub fn commands(&self) -> &ScriptCommands {
        &self.commands
    }

    /// The per-frame `draw` hook: scripts populate the UI batches through
    /// `rx.draw_text` / `rx.draw_line`.
    pub fn dispatch_draw(&mut self, session: &mut Session, draw: &mut crate::draw::Context) {
        let Self {
            plugins, commands, ..
        } = self;
        for i in 0..plugins.len() {
            {
                let plugin = &plugins[i];
                if !plugin.enabled || !plugin.script.has_fn("draw") {
                    continue;
                }
            }
            let state = plugins[i].state.clone();
            let name = plugins[i].name.clone();
            let mut ctx = Ctx::with_draw(session, draw).with_commands(&mut *commands, &name);
            let result = plugins[i].script.call("draw", (state, &mut ctx));
            drop(ctx);
            if let Err(e) = result {
                let plugin = &mut plugins[i];
                plugin.enabled = false;
                log::error!("plugin `{}` draw: {}", plugin.name, e);
                let name = plugin.name.clone();
                session.message(
                    format!("Plugin `{}` disabled: {}", name, first_line(&e)),
                    crate::session::MessageType::Error,
                );
            }
        }
    }

    /// View lifecycle hooks, driven from the session's effects.
    pub fn dispatch_effects(&mut self, session: &mut Session, effects: &[crate::session::Effect]) {
        use crate::session::Effect;

        let Self {
            plugins, commands, ..
        } = self;
        for effect in effects {
            let (hook, id) = match effect {
                Effect::ViewAdded(id) => ("view_added", *id),
                Effect::ViewRemoved(id) => ("view_removed", *id),
                _ => continue,
            };
            let id = u16::from(id) as i64;
            match hook {
                "view_added" => {
                    dispatch!(plugins, commands, session, "view_added", |state, ctx| (
                        state, ctx, id
                    ))
                }
                _ => dispatch!(plugins, commands, session, "view_removed", |state, ctx| (
                    state, ctx, id
                )),
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
    fn draw_hook_populates_ui_batches() {
        use crate::draw;
        use crate::font::TextBatch;
        use crate::gfx::{shape2d, sprite2d};
        use crate::sprite;

        let dir = tempfile::tempdir().unwrap();
        let (mut host, mut session) = host_with(
            dir.path(),
            "p",
            r#"
            pub fn init(rx) { #{} }
            pub fn draw(state, rx) {
                rx.draw_text("hi", 10.0, 20.0, rx::rgb(255, 0, 0));
                rx.draw_line((0.0, 0.0), (50.0, 50.0), rx::rgb(0, 255, 0));
            }
            "#,
        );

        let mut draw_ctx = draw::Context {
            ui_batch: shape2d::Batch::new(),
            text_batch: TextBatch::new(96, 208, draw::GLYPH_WIDTH, draw::GLYPH_HEIGHT),
            overlay_batch: TextBatch::new(96, 208, draw::GLYPH_WIDTH, draw::GLYPH_HEIGHT),
            cursor_sprite: sprite::Sprite::new(96, 96),
            tool_batch: sprite2d::Batch::new(96, 96),
            paste_batch: sprite2d::Batch::new(8, 8),
            checker_batch: sprite2d::Batch::new(2, 2),
        };
        host.dispatch_draw(&mut session, &mut draw_ctx);

        // "hi" = 2 glyphs * 6 vertices.
        assert_eq!(draw_ctx.text_batch.vertices().len(), 12);
        // One stroked line = one quad = 6 vertices.
        assert_eq!(draw_ctx.ui_batch.vertices().len(), 6);
        // Drawing outside the draw hook is a no-op, not a crash.
        host.dispatch_update(&mut session);
    }

    #[test]
    fn script_mode_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let (mut host, mut session) = host_with(
            dir.path(),
            "p",
            r#"
            pub fn init(rx) {
                rx.register_command("probe", ["str"], "Switch and probe", probe);
                #{}
            }
            pub fn probe(state, rx, args) {
                let ok = rx.switch_mode(args[0]);
                let prev = rx.prev_mode().unwrap_or("-");
                rx.message(`${ok} ${rx.mode()} ${prev}`);
            }
            "#,
        );

        // Custom mode: accepted, prev_mode tracked.
        host.dispatch_command(&mut session, "probe", "funky");
        assert_eq!(session.message.to_string(), "true funky normal");
        assert_eq!(session.mode.to_string(), "funky");

        // Builtin name maps to the builtin mode.
        host.dispatch_command(&mut session, "probe", "visual");
        assert_eq!(session.mode, crate::session::Mode::Visual(Default::default()));

        // An over-long name is rejected and the mode stays.
        host.dispatch_command(
            &mut session,
            "probe",
            "way-too-long-of-a-mode-name-to-be-allowed-in-here",
        );
        assert!(session.message.to_string().starts_with("false"));
        assert_eq!(session.mode, crate::session::Mode::Visual(Default::default()));
    }

    #[test]
    fn script_mode_click_does_not_pass_views() {
        use crate::event::Event;
        use crate::execution::Execution;
        use crate::platform;
        use crate::session::Mode;
        use crate::view::FileStatus;

        let dir = tempfile::tempdir().unwrap();
        let (mut host, session) = host_with(dir.path(), "p", "pub fn init(rx) { #{} }");
        // The first view must not be a scratch pad (`NoFile`), or adding
        // the second view would replace it.
        let mut session = session.with_blank(
            FileStatus::New(std::path::PathBuf::from("/tmp/rx-test-first.png").into()),
            32,
            32,
        );
        let first = session.views.active_id;
        session.blank(FileStatus::NoFile, 32, 32);
        let second = session.views.active_id;
        assert_ne!(first, second);
        session.activate(first);

        let mut exec = Execution::normal().unwrap();
        let click = || {
            vec![
                Event::MouseInput(platform::MouseButton::Left, platform::InputState::Pressed),
                Event::MouseInput(platform::MouseButton::Left, platform::InputState::Released),
            ]
        };

        // In a script mode, clicking a non-active view must not activate it.
        session.switch_mode(Mode::Script("funky".try_into().unwrap()));
        session.hover_view = Some(second);
        session.update(
            &mut click(),
            &mut exec,
            Duration::default(),
            Duration::default(),
            &mut host,
        );
        assert_eq!(session.views.active_id, first);

        // In normal mode the same click activates the hovered view.
        session.switch_mode(Mode::Normal);
        session.hover_view = Some(second);
        session.update(
            &mut click(),
            &mut exec,
            Duration::default(),
            Duration::default(),
            &mut host,
        );
        assert_eq!(session.views.active_id, second);
    }

    #[test]
    fn escape_exits_script_mode() {
        use crate::event::Event;
        use crate::execution::Execution;
        use crate::platform;
        use crate::session::Mode;

        let dir = tempfile::tempdir().unwrap();
        let (mut host, session) = host_with(dir.path(), "p", "pub fn init(rx) { #{} }");
        let mut session = session.with_blank(crate::view::FileStatus::NoFile, 32, 32);

        session.switch_mode(Mode::Script("funky".try_into().unwrap()));
        let mut exec = Execution::normal().unwrap();
        let mut events = vec![Event::KeyboardInput(platform::KeyboardInput {
            key: Some(platform::Key::Escape),
            modifiers: platform::ModifiersState::default(),
            state: platform::InputState::Pressed,
        })];
        session.update(
            &mut events,
            &mut exec,
            Duration::default(),
            Duration::default(),
            &mut host,
        );
        assert_eq!(session.mode, Mode::Normal);
    }

    #[test]
    fn register_and_dispatch_typed_command() {
        let dir = tempfile::tempdir().unwrap();
        let (mut host, mut session) = host_with(
            dir.path(),
            "p",
            r#"
            pub fn init(rx) {
                rx.register_command("greet", ["str", "int?"], "Greet someone", greet);
                #{ calls: 0 }
            }
            pub fn greet(state, rx, args) {
                state.calls += 1;
                if args.len() == 2 {
                    rx.message(`hi ${args[0]} x${args[1]} (#${state.calls})`);
                } else {
                    rx.message(`hi ${args[0]} (#${state.calls})`);
                }
            }
            "#,
        );

        host.dispatch_command(&mut session, "greet", "world 3");
        assert_eq!(session.message.to_string(), "hi world x3 (#1)");

        // Optional argument omitted; state persists across calls.
        host.dispatch_command(&mut session, "greet", "moon");
        assert_eq!(session.message.to_string(), "hi moon (#2)");
    }

    #[test]
    fn command_argument_validation() {
        let dir = tempfile::tempdir().unwrap();
        let (mut host, mut session) = host_with(
            dir.path(),
            "p",
            r#"
            pub fn init(rx) {
                rx.register_command("paint", ["int", "color"], "Paint", paint);
                #{}
            }
            pub fn paint(state, rx, args) { rx.message("painted"); }
            "#,
        );

        // Too few arguments.
        host.dispatch_command(&mut session, "paint", "1");
        assert_eq!(
            session.message.to_string(),
            "Error: usage: paint <int> <color>"
        );

        // Bad int.
        host.dispatch_command(&mut session, "paint", "x #ff0000");
        assert!(session.message.to_string().contains("invalid int `x`"));

        // Bad color (also: must not panic on short tokens).
        host.dispatch_command(&mut session, "paint", "1 #f");
        assert!(session.message.to_string().contains("invalid color `#f`"));

        // Too many arguments.
        host.dispatch_command(&mut session, "paint", "1 #ff0000 extra");
        assert!(session.message.to_string().contains("usage: paint"));

        // Valid invocation.
        host.dispatch_command(&mut session, "paint", "1 #ff0000");
        assert_eq!(session.message.to_string(), "painted");
    }

    #[test]
    fn unknown_command_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (mut host, mut session) = host_with(dir.path(), "p", "pub fn init(rx) { #{} }");

        host.dispatch_command(&mut session, "no/such", "");
        assert_eq!(session.message.to_string(), "Error: unknown command: no/such");
    }

    #[test]
    fn run_builtin_delegates_to_builtin_commands() {
        let dir = tempfile::tempdir().unwrap();
        let (mut host, mut session) = host_with(
            dir.path(),
            "p",
            r#"
            pub fn init(rx) {
                rx.register_command("debug/on", [], "Enable debug", handler);
                #{}
            }
            pub fn handler(state, rx, args) {
                let ok = rx.run_builtin("set debug = on");
                let not_builtin = rx.run_builtin("no/such/builtin");
                if ok && !not_builtin {
                    rx.message("delegated");
                }
            }
            "#,
        );

        host.dispatch_command(&mut session, "debug/on", "");
        assert_eq!(session.message.to_string(), "delegated");
        assert!(session.settings["debug"].is_set());
    }

    #[test]
    fn command_registry_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let (mut host, mut session) = host_with(
            dir.path(),
            "p",
            r#"
            pub fn init(rx) {
                rx.register_command("mine", [], "My command", handler);
                rx.register_command_repeating("again", [], "Repeats", handler);
                #{}
            }
            pub fn handler(state, rx, args) { rx.message("ran"); }
            "#,
        );

        // Declared metadata is queryable and synced to the help list.
        assert!(!host.command_repeats("mine"));
        assert!(host.command_repeats("again"));
        assert_eq!(
            session.cmdline.commands.script_commands(),
            &[
                ("mine".to_string(), "My command".to_string()),
                ("again".to_string(), "Repeats".to_string())
            ]
        );

        // Re-registering after reload works (the registry is cleared).
        host.reload(&mut session);
        host.dispatch_command(&mut session, "mine", "");
        assert_eq!(session.message.to_string(), "ran");

        // A plugin that goes away takes its commands with it.
        std::fs::write(
            dir.path().join("p.rune"),
            "pub fn init(rx) { #{} }",
        )
        .unwrap();
        host.reload(&mut session);
        assert!(session.cmdline.commands.script_commands().is_empty());
        host.dispatch_command(&mut session, "mine", "");
        assert_eq!(session.message.to_string(), "Error: unknown command: mine");
    }

    #[test]
    fn command_registration_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        // `a` registers first (lexicographic load order); `b` collides.
        write_plugin(
            dir.path(),
            "a",
            r#"
            pub fn init(rx) {
                rx.register_command("shared", [], "From a", handler);
                #{}
            }
            pub fn handler(state, rx, args) { rx.message("a ran"); }
            "#,
        );
        write_plugin(
            dir.path(),
            "b",
            r#"
            pub fn init(rx) {
                let dup = rx.register_command("shared", [], "From b", handler);
                let builtin = rx.register_command("undo", [], "Shadow", handler);
                let bad_sig = rx.register_command("sig", ["nope"], "Bad", handler);
                if !dup && !builtin && !bad_sig {
                    rx.message("all rejected");
                }
                #{}
            }
            pub fn handler(state, rx, args) { rx.message("b ran"); }
            "#,
        );

        let mut session = test_session();
        let mut host = PluginHost::new(Some(dir.path().to_path_buf())).unwrap();
        host.load(&mut session);
        assert_eq!(session.message.to_string(), "all rejected");

        // First registration wins.
        host.dispatch_command(&mut session, "shared", "");
        assert_eq!(session.message.to_string(), "a ran");
    }

    #[test]
    fn erroring_command_handler_disables_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let (mut host, mut session) = host_with(
            dir.path(),
            "p",
            r#"
            pub fn init(rx) {
                rx.register_command("boom", [], "Explode", handler);
                #{}
            }
            pub fn handler(state, rx, args) { None.unwrap() }
            "#,
        );

        host.dispatch_command(&mut session, "boom", "");
        assert!(session.message.to_string().starts_with("Plugin `p` disabled"));
        assert_eq!(host.plugins().filter(|p| p.enabled).count(), 0);

        // Disabled plugin's commands report instead of running.
        host.dispatch_command(&mut session, "boom", "");
        assert_eq!(
            session.message.to_string(),
            "Error: ':boom': plugin `p` is not loaded"
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
