use rx::execution::{DigestMode, ExecutionMode};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_derive::Deserialize;

#[macro_use]
extern crate lazy_static;

#[derive(Deserialize)]
struct Config {
    window: WindowConfig,
    assets: AssetConfig,
}

#[derive(Deserialize)]
struct WindowConfig {
    width: u32,
    height: u32,
}

#[derive(Deserialize)]
struct AssetConfig {
    glyphs: PathBuf,
}

lazy_static! {
    /// Windowed (glfw) runs spawn real windows and graphics contexts,
    /// which are not thread-safe, so they are serialized. Headless runs
    /// are surface-less and can run in parallel.
    pub static ref MUTEX: Mutex<()> = Mutex::new(());
}

#[test]
fn simple() {
    test("simple");
}

#[test]
fn resize() {
    test("resize");
}

#[test]
fn visual() {
    test("visual");
}

#[test]
fn palette() {
    test("palette");
}

#[test]
fn snapshots() {
    test("snapshots");
}

#[test]
fn saving() {
    test("saving");
}

#[test]
fn views() {
    test("views");
}

#[test]
fn yank_paste() {
    test("yank-paste");
}

#[test]
fn brush_basic() {
    test("brush-basic");
}

#[test]
fn brush_advanced() {
    test("brush-advanced");
}

#[test]
fn frames() {
    test("frames");
}

#[test]
fn ui() {
    test("ui");
}

#[test]
fn grid() {
    test("grid");
}

#[test]
fn flood() {
    test("flood");
}

#[test]
fn plugin_load() {
    test("plugin-load");
}

#[test]
fn source() {
    test("source");
}

#[test]
fn mouse() {
    test("mouse");
}

#[test]
fn visual_mouse() {
    test("visual-mouse");
}

#[test]
fn organize_views() {
    test("organize-views");
}

////////////////////////////////////////////////////////////////////////////////

fn test(name: &str) {
    if let Err(e) = run(name) {
        panic!("test '{}' failed with: {}", name, e);
    }
}

fn run(name: &str) -> io::Result<()> {
    // The `saving` test writes and re-opens this file; make sure it's
    // not there when the test runs. Scoped to `saving` so concurrent
    // tests don't delete it mid-run.
    if name == "saving" {
        fs::remove_file("/tmp/rx.png").ok();
    }

    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(name);
    let cfg: Config = {
        let path = path.join(name).with_extension("toml");
        let cfg = fs::read_to_string(&path)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", path.display(), e)))?;
        toml::from_str(&cfg)?
    };
    let glyphs = fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(&cfg.assets.glyphs))
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", path.display(), e)))?;

    let glyphs = glyphs.as_slice();

    // Tests with a `plugins/` subdirectory get those plugins loaded.
    let plugin_dir = Some(path.join("plugins")).filter(|p| p.is_dir());

    let options = rx::Options {
        resizable: false,
        headless: true,
        source: Some(path.join(name).with_extension("rx")),
        plugin_dir,
        width: cfg.window.width,
        height: cfg.window.height,
        exec: ExecutionMode::Replay(path.clone(), DigestMode::Verify),
        glyphs,
        debug: false,
    };

    {
        let _guard = if cfg!(feature = "glfw") {
            Some(MUTEX.lock())
        } else {
            None
        };
        rx::init::<&str>(&[], options)
    }
}
