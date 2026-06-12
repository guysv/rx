use crate::platform::{GraphicsContext, LogicalSize, WindowEvent, WindowHint};

use std::io;

pub struct Events {
    _handle: (),
}

impl Events {
    pub fn wait(&mut self) {}

    pub fn wait_timeout(&mut self, _timeout: std::time::Duration) {}

    pub fn poll(&mut self) {}

    pub fn flush<'a>(&'a self) -> impl Iterator<Item = WindowEvent> + 'a {
        std::iter::empty::<WindowEvent>()
    }
}

pub struct Window {
    size: LogicalSize,
}

impl Window {
    pub fn request_redraw(&self) {}

    pub fn handle(&self) -> &Self {
        self
    }

    pub fn get_proc_address(&mut self, _s: &str) -> *const std::ffi::c_void {
        unreachable!()
    }

    pub fn set_cursor_visible(&mut self, _visible: bool) {}

    pub fn scale_factor(&self) -> f64 {
        1.0
    }

    pub fn size(&self) -> LogicalSize {
        self.size
    }

    pub fn is_focused(&self) -> bool {
        true
    }

    pub fn is_closing(&self) -> bool {
        false
    }

    pub fn present(&self) {}

    pub fn clipboard(&self) -> Option<String> {
        None
    }
}

pub fn init(
    _title: &str,
    w: u32,
    h: u32,
    _hints: &[WindowHint],
    _context: GraphicsContext,
) -> io::Result<(Window, Events)> {
    Ok((
        Window {
            size: LogicalSize::new(w as f64, h as f64),
        },
        Events { _handle: () },
    ))
}
