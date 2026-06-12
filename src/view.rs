pub mod path;
pub mod resource;

pub use path::{Format, Path};
pub use resource::{Edit, EditId, Snapshot, ViewResource};

use crate::cmd::Axis;
use crate::session::{Direction, Session, SessionCoords};

use crate::gfx::math::*;
use crate::gfx::rect::Rect;
use crate::gfx::{Point, Rgba8};

use nonempty::NonEmpty;

use std::collections::btree_map;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io;

/// View identifier.
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone, Debug, Default)]
pub struct ViewId(u16);

impl From<ViewId> for u16 {
    fn from(id: ViewId) -> u16 {
        id.0
    }
}

impl From<u16> for ViewId {
    fn from(id: u16) -> ViewId {
        ViewId(id)
    }
}

impl fmt::Display for ViewId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// View coordinates.
///
/// These coordinates are relative to the bottom left corner of the view.
pub type ViewCoords<T> = Point<ViewExtent, T>;

/// Maximum view sheet dimension, set by the GPU texture size limit.
pub const MAX_SHEET_DIM: u32 = 8192;

/// View extent information.
///
/// The extent describes the view *sheet*: frames are horizontal strips
/// of the sheet, layers are vertical strips. The sheet is `fw * nframes`
/// wide and `fh * nlayers` tall. The view's *display* footprint in the
/// workspace is the sheet width by a single strip height (`fh`) — see
/// `View::height` vs `View::sheet_height`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewExtent {
    /// Frame width.
    pub fw: u32,
    /// Frame height.
    pub fh: u32,
    /// Number of frames.
    pub nframes: usize,
    /// Number of layers.
    pub nlayers: usize,
}

impl ViewExtent {
    /// A single-layer extent.
    pub fn new(fw: u32, fh: u32, nframes: usize) -> Self {
        Self::layered(fw, fh, nframes, 1)
    }

    /// An extent with the given number of layers.
    pub fn layered(fw: u32, fh: u32, nframes: usize, nlayers: usize) -> Self {
        debug_assert!(nlayers >= 1, "a view always has at least one layer");
        ViewExtent {
            fw,
            fh,
            nframes,
            nlayers,
        }
    }

    /// Extent total (sheet) width.
    pub fn width(&self) -> u32 {
        self.fw * self.nframes as u32
    }

    /// Extent total (sheet) height: one strip per layer.
    pub fn height(&self) -> u32 {
        self.fh * self.nlayers as u32
    }

    /// Rect containing the whole extent (the sheet).
    pub fn rect(&self) -> Rect<u32> {
        Rect::origin(self.width(), self.height())
    }

    /// Rect containing a single frame, within the bottom layer strip.
    pub fn frame(&self, n: usize) -> Rect<u32> {
        let n = n as u32;
        Rect::new(self.fw * n, 0, self.fw * n + self.fw, self.fh)
    }

    /// Rect containing a single layer strip (all frames), in sheet space.
    pub fn layer(&self, n: usize) -> Rect<u32> {
        let n = n as u32;
        Rect::new(0, self.fh * n, self.width(), self.fh * (n + 1))
    }

    /// Compute the frame index, given a point.
    /// Warning: can underflow.
    pub fn to_frame(self, p: ViewCoords<u32>) -> usize {
        (p.x / self.fw) as usize
    }

    /// Compute the layer index, given a sheet-space point.
    pub fn to_layer(self, p: ViewCoords<u32>) -> usize {
        (p.y / self.fh) as usize
    }
}

/// Current state of the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewState {
    /// The view is okay. It doesn't need to be redrawn or saved.
    Okay,
    /// The view has been touched, the changes need to be stored in a snapshot.
    /// If the parameter is `Some`, the view extents were changed.
    Dirty(Option<ViewExtent>),
    /// The view is damaged, it needs to be redrawn from a snapshot.
    /// This happens when undo/redo is used.
    Damaged(Option<ViewExtent>),
}

/// A view operation to be carried out by the renderer.
#[derive(Debug, Clone)]
pub enum ViewOp {
    /// Copy an area of the view to another area.
    Blit(Rect<f32>, Rect<f32>),
    /// Clear to a color.
    Clear(Rgba8),
    /// Yank the given area into the paste buffer.
    Yank(Rect<i32>),
    /// Flips a given area horizontally or vertically.
    Flip(Rect<i32>, Axis),
    /// Blit the paste buffer into the given area.
    Paste(Rect<i32>),
    /// Resize the view.
    Resize(u32, u32),
    /// Paint a single pixel.
    SetPixel(Rgba8, i32, i32),
}

/// A view on a sprite or image.
#[derive(Debug)]
pub struct View<R> {
    /// Frame width.
    pub fw: u32,
    /// Frame height.
    pub fh: u32,
    /// Number of layers. Layers are vertical strips of the view sheet,
    /// as frames are horizontal strips.
    pub nlayers: usize,
    /// View offset relative to the session workspace.
    pub offset: Vector2<f32>,
    /// Identifier.
    pub id: ViewId,
    /// Zoom level.
    pub zoom: f32,
    /// List of operations to carry out on the view.  Cleared every frame.
    pub ops: Vec<ViewOp>,
    /// Whether the view is flipped in the X axis.
    pub flip_x: bool,
    /// Whether the view is flipped in the Y axis.
    pub flip_y: bool,
    /// Status of the file displayed by this view.
    pub file_status: FileStatus,
    /// State of the view.
    pub state: ViewState,
    /// Animation state of the sprite displayed by this view.
    pub animation: Animation<Rect<f32>>,
    /// View resource.
    pub resource: R,

    /// Which view snapshot has been saved to disk, if any.
    saved_snapshot: Option<EditId>,
}

/// View animation.
#[derive(Debug)]
pub struct Animation<T> {
    pub index: usize,
    pub frames: Vec<T>,
}

impl<T> Animation<T> {
    pub fn new(frames: Vec<T>) -> Self {
        Self { index: 0, frames }
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn step(&mut self) {
        self.index = (self.index + 1) % self.len();
    }

    pub fn val(&self) -> &T {
        &self.frames[self.index % self.len()]
    }
}

impl<R> std::ops::Deref for View<R> {
    type Target = R;

    fn deref(&self) -> &Self::Target {
        &self.resource
    }
}

impl<R> std::ops::DerefMut for View<R> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.resource
    }
}

impl<R> View<R> {
    /// Create a new view. Takes a frame width and height.
    pub fn new(id: ViewId, fs: FileStatus, fw: u32, fh: u32, nframes: usize, resource: R) -> Self {
        let saved_snapshot = if let FileStatus::Saved(_) = &fs {
            Some(Default::default())
        } else {
            None
        };

        let origin = Rect::origin(fw as f32, fh as f32);
        let frames: Vec<_> = (0..nframes)
            .map(|i| origin + Vector2::new(i as f32 * fw as f32, 0.))
            .collect();

        Self {
            id,
            fw,
            fh,
            nlayers: 1,
            offset: Vector2::zero(),
            zoom: 1.,
            ops: Vec::new(),
            flip_x: false,
            flip_y: false,
            file_status: fs,
            animation: Animation::new(frames),
            state: ViewState::Okay,
            saved_snapshot,
            resource,
        }
    }

    /// View width. Basically frame-width times number of frames.
    pub fn width(&self) -> u32 {
        self.fw * self.animation.len() as u32
    }

    /// View *display* height: the workspace footprint, one strip tall.
    /// The underlying sheet may be taller — see `sheet_height`.
    pub fn height(&self) -> u32 {
        self.fh
    }

    /// View *sheet* height: the pixel storage height, one strip per
    /// layer. Equal to `height()` only for single-layer views.
    pub fn sheet_height(&self) -> u32 {
        self.fh * self.nlayers as u32
    }

    /// View display width and height.
    pub fn size(&self) -> (u32, u32) {
        (self.width(), self.height())
    }

    /// View file name, if any.
    pub fn file_storage(&self) -> Option<&FileStorage> {
        match self.file_status {
            FileStatus::New(ref f) => Some(f),
            FileStatus::Modified(ref f) => Some(f),
            FileStatus::Saved(ref f) => Some(f),
            FileStatus::NoFile => None,
        }
    }

    /// Extend the view by one frame.
    pub fn extend(&mut self) {
        let w = self.width() as f32;
        let fw = self.fw as f32;
        let fh = self.fh as f32;

        self.animation.frames.push(Rect::new(w, 0., w + fw, fh));

        self.resized();
    }

    /// Shrink the view by one frame.
    pub fn shrink(&mut self) {
        // Don't allow the view to have zero frames.
        if self.animation.len() > 1 {
            self.animation.frames.pop();
            self.resized();
        }
    }

    /// Extend the view by one frame, by cloning an existing frame,
    /// by index.
    pub fn extend_clone(&mut self, index: i32) {
        let width = self.width() as f32;
        let (fw, fh) = (self.fw as f32, self.fh as f32);

        let index = if index == -1 {
            self.animation.len() - 1
        } else {
            index as usize
        };

        self.extend();
        self.ops.push(ViewOp::Blit(
            Rect::new(fw * index as f32, 0., fw * (index + 1) as f32, fh),
            Rect::new(width, 0., width + fw, fh),
        ));
    }

    /// Extend the view by one layer: a new transparent strip above the
    /// existing ones. The sheet grows by one strip height; the display
    /// footprint is unchanged.
    pub fn extend_layer(&mut self) {
        self.nlayers += 1;
        self.resized();
    }

    /// Shrink the view by one layer, removing the top strip.
    pub fn shrink_layer(&mut self) {
        // Don't allow the view to have zero layers.
        if self.nlayers > 1 {
            self.nlayers -= 1;
            self.resized();
        }
    }

    /// Extend the view by one layer, cloning an existing layer's strip,
    /// by index. `-1` clones the top layer.
    pub fn extend_clone_layer(&mut self, index: i32) {
        let index = if index == -1 {
            self.nlayers - 1
        } else {
            index as usize
        };

        // The source strip rect is taken from the pre-extend extent: the
        // blit reads the current snapshot, which doesn't have the new
        // strip yet.
        let src = self.extent().layer(index).map(|n| n as f32);
        let dst_y = self.sheet_height() as f32;
        let w = self.width() as f32;

        self.extend_layer();
        self.ops.push(ViewOp::Blit(
            src,
            Rect::new(0., dst_y, w, dst_y + self.fh as f32),
        ));
    }

    /// Resize view frames to the given size.
    pub fn resize_frames(&mut self, fw: u32, fh: u32) {
        self.reset(ViewExtent::layered(
            fw,
            fh,
            self.animation.len(),
            self.nlayers,
        ));
        self.resized();
    }

    /// Clear the view to a color.
    pub fn clear(&mut self, color: Rgba8) {
        self.ops.push(ViewOp::Clear(color));
        self.touch();
    }

    pub fn paint_color(&mut self, color: Rgba8, x: i32, y: i32) {
        self.ops.push(ViewOp::SetPixel(color, x, y));
    }

    pub fn yank(&mut self, area: Rect<i32>) {
        self.ops.push(ViewOp::Yank(area));
    }

    pub fn flip(&mut self, area: Rect<i32>, dir: Axis) {
        self.ops.push(ViewOp::Flip(area, dir));
    }

    pub fn paste(&mut self, area: Rect<i32>) {
        self.ops.push(ViewOp::Paste(area));
        self.touch();
    }

    /// Slice the view into the given number of frames.
    pub fn slice(&mut self, nframes: usize) -> bool {
        if nframes > 0 && self.width() % nframes as u32 == 0 {
            let fw = self.width() / nframes as u32;
            self.reset(ViewExtent::layered(fw, self.fh, nframes, self.nlayers));
            // FIXME: This is very inefficient. Since the actual frame contents
            // haven't changed, we don't need to create a full snapshot. We just
            // have to record how many frames are in this snapshot.
            self.touch();

            return true;
        }
        false
    }

    /// Restore a view to a given snapshot and extent.
    pub fn restore_extent(&mut self, eid: EditId, extent: ViewExtent) {
        self.damaged(Some(extent));
        self.reset(extent);
        self.refresh_file_status(eid);
    }

    /// Restore a view to a given snapshot.
    pub fn restore(&mut self, eid: EditId) {
        self.damaged(None);
        self.refresh_file_status(eid);
    }

    /// If the snapshot was saved to disk, we mark the view as saved too.
    /// Otherwise, if the view was saved before restoring the snapshot,
    /// we mark it as modified.
    pub fn refresh_file_status(&mut self, eid: EditId) {
        match self.file_status {
            FileStatus::Modified(ref f) if self.is_snapshot_saved(eid) => {
                self.file_status = FileStatus::Saved(f.clone());
            }
            FileStatus::Saved(ref f) => {
                self.file_status = FileStatus::Modified(f.clone());
            }
            _ => {
                // TODO
            }
        }
    }

    /// Set the view state to `Okay`.
    pub fn okay(&mut self) {
        self.state = ViewState::Okay;
    }

    /// Return the view area, including the offset.
    pub fn rect(&self) -> Rect<f32> {
        Rect::new(
            self.offset.x,
            self.offset.y,
            self.offset.x + self.width() as f32 * self.zoom,
            self.offset.y + self.height() as f32 * self.zoom,
        )
    }

    /// Check whether the session coordinates are contained within the view.
    pub fn contains(&self, p: SessionCoords) -> bool {
        self.rect().contains(*p)
    }

    /// Get the center of the view.
    pub fn center(&self) -> ViewCoords<f32> {
        ViewCoords::new(self.width() as f32 / 2., self.height() as f32 / 2.)
    }

    /// View has been modified. Called when using the brush on the view,
    /// or resizing the view.
    pub fn touch(&mut self) {
        if let FileStatus::Saved(ref f) = self.file_status {
            self.file_status = FileStatus::Modified(f.clone());
        }
        if self.state == ViewState::Okay {
            self.state = ViewState::Dirty(None);
        }
    }

    /// View should be considered damaged and needs to be restored from snapshot.
    /// Used when undoing or redoing changes.
    pub fn damaged(&mut self, extent: Option<ViewExtent>) {
        self.state = ViewState::Damaged(extent);
    }

    /// Check whether the view is damaged.
    pub fn is_damaged(&self) -> bool {
        matches!(self.state, ViewState::Damaged(_))
    }

    /// Check whether the view is dirty.
    pub fn is_dirty(&self) -> bool {
        matches!(self.state, ViewState::Dirty(_))
    }

    /// Check whether the view is resized.
    pub fn is_resized(&self) -> bool {
        matches!(self.state, ViewState::Dirty(Some(_)))
    }

    /// Check whether the view is okay.
    pub fn is_okay(&self) -> bool {
        self.state == ViewState::Okay
    }

    /// Return the file status as a string.
    pub fn status(&self) -> String {
        self.file_status.to_string()
    }

    /// Return the view extent.
    pub fn extent(&self) -> ViewExtent {
        ViewExtent::layered(self.fw, self.fh, self.animation.len(), self.nlayers)
    }

    /// Return the view bounds, as an origin-anchored rectangle.
    pub fn bounds(&self) -> Rect<i32> {
        Rect::origin(self.width() as i32, self.height() as i32)
    }

    /// Return the view layer bounds, as an origin-anchored rectangle.
    pub fn layer_bounds(&self) -> Rect<i32> {
        Rect::origin(self.width() as i32, self.fh as i32)
    }

    ////////////////////////////////////////////////////////////////////////////

    fn resized(&mut self) {
        if let FileStatus::Saved(ref f) = self.file_status {
            self.file_status = FileStatus::Modified(f.clone());
        }
        if self.state == ViewState::Okay {
            self.state = ViewState::Dirty(Some(self.extent()));
        }
        self.ops
            .push(ViewOp::Resize(self.width(), self.sheet_height()));
    }

    /// Check whether the given snapshot has been saved to disk.
    fn is_snapshot_saved(&self, id: EditId) -> bool {
        self.saved_snapshot == Some(id)
    }

    /// Mark the view as saved at a given snapshot.
    fn saved(&mut self, id: EditId, storage: FileStorage) {
        self.file_status = FileStatus::Saved(storage);
        self.saved_snapshot = Some(id);
    }

    /// Reset the view by providing frame size and number of frames.
    fn reset(&mut self, extent: ViewExtent) {
        self.fw = extent.fw;
        self.fh = extent.fh;
        self.nlayers = extent.nlayers;

        let mut frames = Vec::new();
        let origin = Rect::origin(self.fw as f32, self.fh as f32);

        for i in 0..extent.nframes {
            frames.push(origin + Vector2::new(i as f32 * self.fw as f32, 0.));
        }
        self.animation = Animation::new(frames);
    }
}

impl View<ViewResource> {
    /// Get the color at the given view coordinate.
    pub fn color_at(&self, p: ViewCoords<u32>) -> Option<&Rgba8> {
        let (snapshot, pixels) = self.resource.layer.current_snapshot();
        snapshot.coord_to_index(p).and_then(|idx| pixels.get(idx))
    }

    /// Restore a view snapshot (undo/redo an edit).
    pub fn restore_snapshot(&mut self, dir: Direction) {
        let result = if dir == Direction::Backward {
            self.resource.history_prev()
        } else {
            self.resource.history_next()
        };

        match result {
            Some((eid, Edit::ViewResized(from, to))) => {
                let extent = match dir {
                    Direction::Backward => from,
                    Direction::Forward => to,
                };
                self.restore_extent(eid, extent);
            }
            Some((eid, Edit::ViewPainted)) => {
                self.restore(eid);
            }
            Some((_, Edit::Initial)) => {}
            None => {}
        }
    }

    pub fn save_as(&mut self, storage: &FileStorage) -> io::Result<usize> {
        let ext = self.extent();
        let (edit_id, written) = match &storage {
            FileStorage::Single(path) => {
                {
                    let mut path_copy = path.clone();
                    path_copy.pop();
                    std::fs::create_dir_all(path_copy.as_path())?;
                }

                let edit_id = self.save_rect_as(ext.rect(), path)?;

                (edit_id, (ext.width() * ext.height()) as usize)
            }
            FileStorage::Range(paths) => {
                for (i, path) in paths.iter().enumerate() {
                    self.save_rect_as(ext.frame(i), path)?;
                }

                let edit_id = self.resource.current_edit();

                (edit_id, paths.len() * (ext.fw * ext.fh) as usize)
            }
        };

        // Mark the view as saved at a specific snapshot and with the given path.
        match self.file_status {
            FileStatus::Modified(ref curr_fs) | FileStatus::New(ref curr_fs) => {
                if curr_fs == storage {
                    self.saved(edit_id, storage.clone());
                }
            }
            FileStatus::NoFile => {
                self.saved(edit_id, storage.clone());
            }
            FileStatus::Saved(_) => {}
        }

        Ok(written)
    }

    /// Save part of a layer to disk.
    fn save_rect_as(&mut self, rect: Rect<u32>, path: &std::path::Path) -> io::Result<EditId> {
        // Only allow overwriting of files if it's the file of the view being saved.
        if path.exists() && self.file_storage().map_or(true, |f| !f.contains(path)) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("\"{}\" already exists", path.display()),
            ));
        }
        let (e_id, _) = self.save(rect, path)?;

        Ok(e_id)
    }
}

///////////////////////////////////////////////////////////////////////////////

/// Status of the underlying file displayed by the view.
#[derive(PartialEq, Eq, Clone, Debug)]
pub enum FileStatus {
    /// There is no file being displayed.
    NoFile,
    /// The file is new and unsaved.
    New(FileStorage),
    /// The file is saved and unmodified.
    Saved(FileStorage),
    /// The file has been modified since the last save.
    Modified(FileStorage),
}

impl ToString for FileStatus {
    fn to_string(&self) -> String {
        match self {
            FileStatus::NoFile => String::new(),
            FileStatus::Saved(ref storage) => format!("{}", storage),
            FileStatus::New(ref storage) => format!("{} [new]", storage),
            FileStatus::Modified(ref storage) => format!("{} [modified]", storage),
        }
    }
}

/// Representation of the view data on disk.
#[derive(PartialEq, Eq, Clone, Debug)]
pub enum FileStorage {
    /// Stored as a range of files.
    Range(NonEmpty<std::path::PathBuf>),
    /// Stored as a single file.
    Single(std::path::PathBuf),
}

impl fmt::Display for FileStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Range(paths) => {
                let parent = paths.first().parent();

                if paths.iter().all(|p| p.parent() == parent) {
                    let first = paths
                        .first()
                        .file_stem()
                        .expect("the path has a file stem")
                        .to_string_lossy()
                        .into_owned();
                    let last = paths
                        .last()
                        .file_name()
                        .expect("the path has a file name")
                        .to_string_lossy();

                    let first = if let Some(parent) = parent {
                        parent.join(first)
                    } else {
                        first.into()
                    };

                    write!(f, "{} .. {}", first.display(), last)
                } else {
                    write!(f, "*")
                }
            }
            Self::Single(path) => write!(f, "{}", path.display()),
        }
    }
}

impl From<&std::path::Path> for FileStorage {
    fn from(p: &std::path::Path) -> Self {
        FileStorage::Single(p.into())
    }
}

impl From<std::path::PathBuf> for FileStorage {
    fn from(p: std::path::PathBuf) -> Self {
        FileStorage::Single(p)
    }
}

impl FileStorage {
    pub fn contains<P: AsRef<std::path::Path>>(&self, p: P) -> bool {
        match self {
            Self::Single(buf) => buf.as_path() == p.as_ref(),
            Self::Range(bufs) => bufs.iter().any(|buf| buf.as_path() == p.as_ref()),
        }
    }
}

///////////////////////////////////////////////////////////////////////////////

/// Manages views.
#[derive(Debug)]
pub struct ViewManager<R> {
    /// Currently active view.
    pub active_id: ViewId,

    /// View dictionary.
    views: BTreeMap<ViewId, View<R>>,

    /// The next `ViewId`.
    next_id: ViewId,

    /// A last-recently-used list of views.
    lru: VecDeque<ViewId>,
}

impl<R> ViewManager<R> {
    /// Maximum number of views in the view LRU list.
    const MAX_LRU: usize = Session::MAX_VIEWS;

    /// New empty view manager.
    pub fn new() -> Self {
        Self {
            active_id: ViewId::default(),
            next_id: ViewId(1),
            views: BTreeMap::new(),
            lru: VecDeque::new(),
        }
    }

    /// Add a view.
    pub fn add(&mut self, fs: FileStatus, w: u32, h: u32, nframes: usize, resource: R) -> ViewId {
        let id = self.gen_id();
        let view = View::new(id, fs, w, h, nframes, resource);

        self.views.insert(id, view);

        id
    }

    /// Remove a view.
    pub fn remove(&mut self, id: ViewId) {
        self.views.remove(&id);
        self.lru.retain(|v| *v != id);

        self.active_id = self
            .recent()
            .or_else(|| self.last().map(|v| v.id))
            .unwrap_or(ViewId::default());
    }

    /// Return the id of the last recently active view, if any.
    pub fn recent(&self) -> Option<ViewId> {
        self.lru.front().cloned()
    }

    /// Return the currently active view, if any.
    pub fn active(&self) -> Option<&View<R>> {
        self.views.get(&self.active_id)
    }

    /// Return the currently active view mutably, if any.
    pub fn active_mut(&mut self) -> Option<&mut View<R>> {
        self.views.get_mut(&self.active_id)
    }

    /// Activate a view.
    pub fn activate(&mut self, id: ViewId) {
        debug_assert!(
            self.views.contains_key(&id),
            "the view being activated exists"
        );
        if self.active_id == id {
            return;
        }
        self.active_id = id;
        self.lru.push_front(id);
        self.lru.truncate(Self::MAX_LRU);
    }

    /// Iterate over views.
    pub fn iter(&self) -> btree_map::Values<'_, ViewId, View<R>> {
        self.views.values()
    }

    /// Iterate over views, mutably.
    pub fn iter_mut(&mut self) -> btree_map::ValuesMut<'_, ViewId, View<R>> {
        self.views.values_mut()
    }

    /// Get a view, mutably.
    pub fn get(&self, id: ViewId) -> Option<&View<R>> {
        self.views.get(&id)
    }

    /// Get a view, mutably.
    pub fn get_mut(&mut self, id: ViewId) -> Option<&mut View<R>> {
        self.views.get_mut(&id)
    }

    /// Find a view.
    pub fn find<F>(&self, f: F) -> Option<&View<R>>
    where
        for<'r> F: Fn(&'r &View<R>) -> bool,
    {
        self.iter().find(f)
    }

    /// Iterate over view ids.
    pub fn ids(&self) -> impl DoubleEndedIterator<Item = ViewId> + '_ {
        self.views.keys().cloned()
    }

    /// Get `ViewId` *after* given id.
    pub fn after(&self, id: ViewId) -> Option<ViewId> {
        self.range(id..).nth(1)
    }

    /// Get `ViewId` *before* given id.
    pub fn before(&self, id: ViewId) -> Option<ViewId> {
        self.range(..id).next_back()
    }

    /// Get the first view.
    pub fn first(&self) -> Option<&View<R>> {
        self.iter().next()
    }

    /// Get the first view, mutably.
    pub fn first_mut(&mut self) -> Option<&mut View<R>> {
        self.iter_mut().next()
    }

    /// Get the last view.
    pub fn last(&self) -> Option<&View<R>> {
        self.iter().next_back()
    }

    /// Get view id range.
    pub fn range<G>(&self, r: G) -> impl DoubleEndedIterator<Item = ViewId> + '_
    where
        G: std::ops::RangeBounds<ViewId>,
    {
        self.views.range(r).map(|(id, _)| *id)
    }

    /// Whether there are views.
    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }

    /// Generate a new view id.
    fn gen_id(&mut self) -> ViewId {
        let ViewId(id) = self.next_id;
        self.next_id = ViewId(id + 1);

        ViewId(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extent_single_layer() {
        let e = ViewExtent::new(16, 12, 3);

        assert_eq!(e.nlayers, 1);
        assert_eq!(e, ViewExtent::layered(16, 12, 3, 1));
        assert_eq!(e.width(), 48);
        assert_eq!(e.height(), 12);
        assert_eq!(e.rect(), Rect::origin(48, 12));
        assert_eq!(e.layer(0), Rect::new(0, 0, 48, 12));
        assert_eq!(e.frame(1), Rect::new(16, 0, 32, 12));
    }

    #[test]
    fn test_extent_layered() {
        let e = ViewExtent::layered(16, 12, 3, 4);

        // Width is unaffected by layers; height is one strip per layer.
        assert_eq!(e.width(), 48);
        assert_eq!(e.height(), 48);
        assert_eq!(e.rect(), Rect::origin(48, 48));

        // Layer strips span all frames, stacked bottom-up.
        assert_eq!(e.layer(0), Rect::new(0, 0, 48, 12));
        assert_eq!(e.layer(3), Rect::new(0, 36, 48, 48));

        // `frame` stays within the bottom strip.
        assert_eq!(e.frame(2), Rect::new(32, 0, 48, 12));

        // Sheet-space points map back to layer indices.
        assert_eq!(e.to_layer(ViewCoords::new(0, 0)), 0);
        assert_eq!(e.to_layer(ViewCoords::new(47, 11)), 0);
        assert_eq!(e.to_layer(ViewCoords::new(0, 12)), 1);
        assert_eq!(e.to_layer(ViewCoords::new(47, 47)), 3);
    }

    #[test]
    fn test_view_sheet_height() {
        let mut v: View<()> = View::new(ViewId(1), FileStatus::NoFile, 16, 12, 2, ());

        assert_eq!(v.nlayers, 1);
        assert_eq!(v.height(), 12);
        assert_eq!(v.sheet_height(), 12);
        assert_eq!(v.extent(), ViewExtent::new(16, 12, 2));

        // A layered extent round-trips through `reset` (the undo path).
        v.restore_extent(0, ViewExtent::layered(16, 12, 2, 3));
        assert_eq!(v.nlayers, 3);
        assert_eq!(v.height(), 12, "display height is one strip");
        assert_eq!(v.sheet_height(), 36);
        assert_eq!(v.extent(), ViewExtent::layered(16, 12, 2, 3));

        // Frame ops preserve the layer count.
        v.extend();
        assert_eq!(v.extent(), ViewExtent::layered(16, 12, 3, 3));
        assert!(v.slice(1));
        assert_eq!(v.extent(), ViewExtent::layered(48, 12, 1, 3));
    }

    #[test]
    fn test_view_layer_lifecycle() {
        let mut v: View<()> = View::new(ViewId(1), FileStatus::NoFile, 16, 12, 2, ());

        v.extend_layer();
        assert_eq!(v.nlayers, 2);
        assert_eq!(v.height(), 12);
        assert_eq!(v.sheet_height(), 24);
        // The resize op carries sheet dimensions.
        assert!(matches!(v.ops.last(), Some(ViewOp::Resize(32, 24))));

        // Clone layer 0: the blit reads layer 0's strip and writes the
        // new top strip (above the two existing ones), in y-up coords.
        v.extend_clone_layer(0);
        assert_eq!(v.nlayers, 3);
        match v.ops.last() {
            Some(ViewOp::Blit(src, dst)) => {
                assert_eq!(*src, Rect::new(0., 0., 32., 12.));
                assert_eq!(*dst, Rect::new(0., 24., 32., 36.));
            }
            op => panic!("expected a blit op, got {:?}", op),
        }

        // The top layer never goes away.
        v.shrink_layer();
        v.shrink_layer();
        assert_eq!(v.nlayers, 1);
        v.shrink_layer();
        assert_eq!(v.nlayers, 1);
    }
}

impl ViewManager<ViewResource> {
    pub fn get_snapshot_safe(&self, id: ViewId) -> Option<(&Snapshot, &[Rgba8])> {
        self.views
            .get(&id)
            .map(|v| v.resource.layer.current_snapshot())
    }

    pub fn get_snapshot(&self, id: ViewId) -> (&Snapshot, &[Rgba8]) {
        self.get_snapshot_safe(id).expect(&format!(
            "view #{} must exist and have an associated snapshot",
            id
        ))
    }

    pub fn get_snapshot_rect(
        &self,
        id: ViewId,
        rect: &Rect<i32>,
    ) -> Option<(&Snapshot, Vec<Rgba8>)> {
        self.views
            .get(&id)
            .map(|v| &v.resource.layer)
            .expect(&format!(
                "view #{} must exist and have an associated snapshot",
                id
            ))
            .get_snapshot_rect(rect)
    }
}
