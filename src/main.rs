// Copyright 2019 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[macro_use]
extern crate log;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate failure;
#[macro_use]
extern crate lazy_static;

mod database;
mod image;
mod stats;
mod vec;
mod view;

use crate::stats::ScopedDuration;
use boolinator::Boolinator;
use clap::Arg;
use futures::future::Fuse;
use futures::future::FutureExt;
use futures::future::RemoteHandle;
use futures::select;
use futures::task::SpawnExt;
use piston_window::*;
use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use vec::*;

#[derive(Debug, Fail)]
pub enum E {
    #[fail(display = "database error: {:?}", 0)]
    DatabaseError(sled::Error),

    #[fail(display = "decode error {:?}", 0)]
    DecodeError(bincode::Error),

    #[fail(display = "encode error {:?}", 0)]
    EncodeError(bincode::Error),

    #[fail(display = "missing data for key {:?}", 0)]
    MissingData(String),

    #[fail(display = "image error: {:?}", 0)]
    ImageError(::image::ImageError),
}

type R<T> = std::result::Result<T, E>;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Pow2(u8);

impl Pow2 {
    fn from(i: u32) -> Self {
        assert!(i.is_power_of_two());
        Pow2((32 - i.leading_zeros() - 1) as u8)
    }

    #[allow(unused)]
    fn u32(&self) -> u32 {
        1 << self.0
    }
}

#[test]
fn size_conversions() {
    assert_eq!(Pow2::from(128), Pow2(7));
    assert_eq!(Pow2(7).u32(), 128);
}

#[derive(
    Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone, Default,
)]
pub struct TileRef(u64);

impl TileRef {
    fn new(size: Pow2, index: u64, chunk: u16) -> Self {
        Self((chunk as u64) | ((index % (1u64 << 40)) << 16) | ((size.0 as u64) << 56))
    }

    #[cfg(test)]
    fn deconstruct(&self) -> (Pow2, u64, u16) {
        let size = ((self.0 & 0xFF00_0000_0000_0000u64) >> 56) as u8;
        let index = (self.0 & 0x00FF_FFFF_FFFF_0000u64) >> 16;
        let chunk = (self.0 & 0x0000_0000_0000_FFFFu64) as u16;
        (Pow2(size), index, chunk)
    }
}

#[test]
fn tile_ref_test() {
    assert_eq!(
        TileRef::new(Pow2(0xFFu8), 0u64, 0u16),
        TileRef(0xFF00_0000_0000_0000u64)
    );
    assert_eq!(
        TileRef::new(Pow2(0xFFu8), 0u64, 0u16).deconstruct(),
        (Pow2(0xFFu8), 0u64, 0u16)
    );
    assert_eq!(
        TileRef::new(Pow2(0xFFu8), 0u64, 0u16).0.to_be_bytes(),
        [0xFF, 0, 0, 0, 0, 0, 0, 0]
    );

    assert_eq!(
        TileRef::new(Pow2(0u8), 0x00FF_FFFF_FFFFu64, 0u16),
        TileRef(0x00F_FFFFF_FFFF_0000u64)
    );
    assert_eq!(
        TileRef::new(Pow2(0u8), 0x00FF_FFFF_FFFFu64, 0u16).deconstruct(),
        (Pow2(0u8), 0x00FF_FFFF_FFFFu64, 0u16)
    );

    assert_eq!(
        TileRef::new(Pow2(0u8), 0u64, 0xFFFFu16),
        TileRef(0x0000_0000_0000_FFFFu64)
    );
    assert_eq!(
        TileRef::new(Pow2(0u8), 0u64, 0xFFFFu16).deconstruct(),
        (Pow2(0u8), 0u64, 0xFFFFu16)
    )
}

#[derive(Debug, Serialize, Deserialize, Eq, PartialEq)]
struct Thumb {
    img_size: [u32; 2],
    tile_refs: Vec<TileRef>,
}

#[derive(Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct Metadata {
    thumbs: Vec<Thumb>,
}

impl Metadata {
    fn nearest(&self, target_size: u32) -> usize {
        let mut found = None;

        let ts_zeros = target_size.leading_zeros() as i16;

        for (i, thumb) in self.thumbs.iter().enumerate() {
            let size = thumb.size();
            let size_zeros = size.leading_zeros() as i16;
            let dist = (ts_zeros - size_zeros).abs();
            if let Some((found_dist, found_i)) = found.take() {
                if dist < found_dist {
                    found = Some((dist, i));
                } else {
                    found = Some((found_dist, found_i));
                }
            } else {
                found = Some((dist, i));
            }
        }

        let (_, i) = found.unwrap();
        i
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TileSpec {
    img_size: [u32; 2],

    // Grid width and height (in number of tiles).
    grid_size: [u32; 2],

    // Tile width and height in pixels.
    tile_size: [u32; 2],
}

impl TileSpec {
    fn ranges(img_size: u32, grid_size: u32, tile_size: u32) -> impl Iterator<Item = (u32, u32)> {
        (0..grid_size).map(move |i| {
            let min = i * tile_size;
            let max = std::cmp::min(img_size, min + tile_size);
            (min, max)
        })
    }

    fn x_ranges(&self) -> impl Iterator<Item = (u32, u32)> {
        Self::ranges(self.img_size[0], self.grid_size[0], self.tile_size[0])
    }

    fn y_ranges(&self) -> impl Iterator<Item = (u32, u32)> {
        Self::ranges(self.img_size[1], self.grid_size[1], self.tile_size[1])
    }
}

impl Thumb {
    fn max_dimension(&self) -> u32 {
        let [w, h] = self.img_size;
        std::cmp::max(w, h)
    }

    fn size(&self) -> u32 {
        self.max_dimension().next_power_of_two()
    }

    fn tile_spec(&self) -> TileSpec {
        let img_size = vec2_f64(self.img_size);
        let tile_size = vec2_scale(vec2_log(img_size, 8.0), 128.0);
        let grid_size = vec2_ceil(vec2_div(img_size, tile_size));
        let tile_size = vec2_ceil(vec2_div(img_size, grid_size));
        TileSpec {
            img_size: self.img_size,
            grid_size: vec2_u32(grid_size),
            tile_size: vec2_u32(tile_size),
        }
    }
}

impl Draw for Thumb {
    fn draw(
        &self,
        trans: [[f64; 3]; 2],
        zoom: f64,
        tiles: &BTreeMap<TileRef, G2dTexture>,
        draw_state: &DrawState,
        g: &mut G2d,
    ) -> bool {
        let img = piston_window::image::Image::new();

        let max_dimension = self.max_dimension() as f64;

        let trans = trans.zoom(zoom / max_dimension);

        // Center the image within the grid square.
        let [x_offset, y_offset] = {
            let img_size = vec2_f64(self.img_size);
            let gaps = vec2_sub([max_dimension, max_dimension], img_size);
            vec2_scale(gaps, 0.5)
        };

        let tile_spec = self.tile_spec();

        let mut it = self.tile_refs.iter();
        for (y, _) in tile_spec.y_ranges() {
            for (x, _) in tile_spec.x_ranges() {
                let tile_ref = it.next().unwrap();
                if let Some(texture) = tiles.get(tile_ref) {
                    let trans = trans.trans(x_offset + x as f64, y_offset + y as f64);
                    img.draw(texture, &draw_state, trans, g);
                }
            }
        }

        true
    }
}

static UPSIZE_FACTOR: f64 = 1.5;

trait Draw {
    fn draw(
        &self,
        trans: [[f64; 3]; 2],
        zoom: f64,
        tiles: &BTreeMap<TileRef, G2dTexture>,
        draw_state: &DrawState,
        g: &mut G2d,
    ) -> bool;
}

#[derive(Debug, Eq, PartialEq)]
pub enum MetadataState {
    Unknown,
    Missing,
    Some(Metadata),
    Errored,
}

impl std::default::Default for MetadataState {
    fn default() -> Self {
        MetadataState::Unknown
    }
}

type Handle<T> = Fuse<RemoteHandle<T>>;

pub type TileMap<T> = BTreeMap<TileRef, T>;

struct App {
    db: Arc<database::Database>,

    groups: Groups,

    // Graphics state
    new_window_settings: Option<WindowSettings>,
    window_settings: WindowSettings,
    window: PistonWindow,
    texture_context: G2dTextureContext,

    // Movement state & modes.
    view: view::View,
    panning: bool,
    zooming: Option<f64>,
    cursor_captured: bool,

    // Mouse distance calculations are relative to this point.
    focus: Option<Vector2<f64>>,

    thumb_executor: futures::executor::ThreadPool,
    thumb_threads: usize,

    shift_held: bool,

    base_id: u64,
}

struct Stopwatch {
    start: std::time::Instant,
    duration: std::time::Duration,
}

impl Stopwatch {
    fn from_millis(millis: u64) -> Self {
        Self {
            start: std::time::Instant::now(),
            duration: std::time::Duration::from_millis(millis),
        }
    }

    fn done(&self) -> bool {
        self.start.elapsed() >= self.duration
    }
}

fn i2c(i: usize, [grid_w, _]: Vector2<u32>) -> Vector2<u32> {
    [(i % grid_w as usize) as u32, (i / grid_w as usize) as u32]
}

#[derive(Debug, Default)]
struct Groups {
    grid_size: Vector2<u32>,
    group_size: Vector2<u32>,
    groups: BTreeMap<[u32; 2], Group>,
}

impl Groups {
    fn group_size_from_grid_size(grid_size: Vector2<u32>) -> Vector2<u32> {
        vec2_max(vec2_u32(vec2_log(vec2_f64(grid_size), 2.0)), [1, 1])
    }

    fn from(images: Vec<image::Image>, grid_size: Vector2<u32>) -> Self {
        let mut ret = Groups {
            grid_size,
            group_size: Self::group_size_from_grid_size(grid_size),
            ..Default::default()
        };

        for image in images.into_iter() {
            ret.insert(image);
        }

        ret
    }

    fn group_coords(&self, coords: Vector2<u32>) -> Vector2<u32> {
        vec2_div(coords, self.group_size)
    }

    fn insert(&mut self, image: image::Image) {
        let coords = i2c(image.i, self.grid_size);
        let group_coords = self.group_coords(coords);
        let group = self.groups.entry(group_coords).or_insert(Group::default());
        group.insert(coords, image);
    }

    fn regroup(&mut self, grid_size: Vector2<u32>) {
        let _s = ScopedDuration::new("regroup");

        let mut groups = BTreeMap::new();
        std::mem::swap(&mut groups, &mut self.groups);

        self.grid_size = grid_size;
        self.group_size = Self::group_size_from_grid_size(grid_size);

        for (_, group) in groups.into_iter() {
            for (_, image) in group.images.into_iter() {
                self.insert(image);
            }
        }
    }

    fn reset(&mut self) {
        for group in self.groups.values_mut() {
            group.reset();
        }
    }
}

// A sparse collection of images.
#[derive(Debug, Default)]
struct Group {
    min_extent: [u32; 2],
    max_extent: [u32; 2],
    tiles: BTreeMap<TileRef, G2dTexture>,
    images: BTreeMap<[u32; 2], image::Image>,
    cache_todo: VecDeque<[u32; 2]>,
    thumb_todo: VecDeque<[u32; 2]>,
    thumb_handles: BTreeMap<[u32; 2], Handle<image::ThumbRet>>,
}

impl Group {
    fn insert(&mut self, coords: Vector2<u32>, image: image::Image) {
        self.min_extent = vec2_min(self.min_extent, coords);
        self.max_extent = vec2_max(self.max_extent, vec2_add(coords, [1, 1]));
        self.images.insert(coords, image);
    }

    fn reset(&mut self) {
        for image in self.images.values_mut() {
            image.reset();
        }
        self.tiles.clear();
        self.thumb_todo.clear();
        self.cache_todo.clear();
    }

    fn recheck(&mut self) {
        self.thumb_todo.clear();
        self.cache_todo.clear();
        self.cache_todo.extend(self.images.keys());
        // TODO: reorder by mouse distance.
    }

    fn load_cache(
        &mut self,
        view: &view::View,
        db: &database::Database,
        target_size: u32,
        texture_settings: &TextureSettings,
        texture_context: &mut G2dTextureContext,
    ) {
        for coords in self.cache_todo.pop_front() {
            let image = self.images.get_mut(&coords).unwrap();

            if image.metadata == MetadataState::Unknown {
                image.metadata = match db.get_metadata(&*image.file) {
                    Ok(Some(metadata)) => MetadataState::Some(metadata),
                    Ok(None) => MetadataState::Missing,
                    Err(e) => {
                        error!("get metadata error: {:?}", e);
                        MetadataState::Errored
                    }
                };
            }

            let metadata = match &image.metadata {
                MetadataState::Unknown => unreachable!(),
                MetadataState::Missing => {
                    self.thumb_todo.push_back(coords);
                    continue;
                }
                MetadataState::Some(metadata) => metadata,
                MetadataState::Errored => continue,
            };

            let is_visible = view.is_visible(view.coords(image.i));

            let shift = if is_visible {
                0
            } else {
                let ratio = view.visible_ratio(view.coords(image.i));
                f64::max(0.0, ratio - 1.0).floor() as usize
            };

            let new_size = metadata.nearest(target_size >> shift);

            let current_size = image.size.unwrap_or(0);

            // Progressive resizing.
            let new_size = match new_size.cmp(&current_size) {
                Ordering::Less => current_size - 1,
                Ordering::Equal => {
                    // Already loaded target size.
                    continue;
                }
                Ordering::Greater => current_size + 1,
            };

            // Load new tiles.
            for tile_ref in &metadata.thumbs[new_size].tile_refs {
                // Already loaded.
                if self.tiles.contains_key(tile_ref) {
                    continue;
                }

                // load the tile from the cache
                let _s3 = ScopedDuration::new("load_tile");

                let data = db.get(*tile_ref).expect("db get").expect("missing tile");

                let image = ::image::load_from_memory(&data).expect("load image");

                // TODO: Would be great to move off thread.
                let image =
                    Texture::from_image(texture_context, &image.to_rgba(), texture_settings)
                        .expect("texture");

                self.tiles.insert(*tile_ref, image);
            }

            // Unload old tiles.
            for (j, thumb) in metadata.thumbs.iter().enumerate() {
                if j == new_size {
                    continue;
                }
                for tile_ref in &thumb.tile_refs {
                    self.tiles.remove(tile_ref);
                }
            }

            image.size = Some(new_size);
            self.cache_todo.push_back(coords);
        }
    }

    async fn update_db(
        res: R<(Arc<File>, Metadata, TileMap<Vec<u8>>)>,
        db: Arc<database::Database>,
    ) -> R<Metadata> {
        match res {
            Ok((file, metadata, tiles)) => {
                // Do before metadata write to prevent invalid metadata references.
                for (id, tile) in tiles {
                    db.set(id, &tile).expect("db set");
                }

                db.set_metadata(&*file, &metadata).expect("set metadata");

                Ok(metadata)
            }
            Err(e) => Err(e),
        }
    }

    fn make_thumb(
        &mut self,
        coords: [u32; 2],
        base_id: u64,
        db: &Arc<database::Database>,
        executor: &mut futures::executor::ThreadPool,
    ) {
        let image = &self.images[&coords];

        if !image.is_missing() {
            return;
        }

        if self.thumb_handles.contains_key(&coords) {
            return;
        }

        let tile_id_index = base_id + image.i as u64;
        let db = Arc::clone(&db);

        let fut = image
            .make_thumb(tile_id_index)
            .then(move |x| Self::update_db(x, db));

        let handle = executor.spawn_with_handle(fut).unwrap().fuse();

        self.thumb_handles.insert(coords, handle);
    }

    fn make_thumbs(
        &mut self,
        base_id: u64,
        db: &Arc<database::Database>,
        executor: &mut futures::executor::ThreadPool,
    ) {
        let _s = ScopedDuration::new("make_thumbs");
        loop {
            if self.thumb_handles.len() > 1 {
                return;
            }

            if let Some(coords) = self.thumb_todo.pop_front() {
                self.make_thumb(coords, base_id, db, executor);
            } else {
                break;
            }
        }
    }

    fn recv_thumbs(&mut self) {
        let _s = ScopedDuration::new("recv_thumbs");

        let mut done = Vec::new();

        let mut handles = BTreeMap::new();
        std::mem::swap(&mut handles, &mut self.thumb_handles);

        for (&coords, mut handle) in &mut handles {
            select! {
                thumb_res = handle => {
                    self.images.get_mut(&coords).unwrap().metadata = match thumb_res {
                        Ok(metadata) => {
                            self.cache_todo.push_front(coords);
                            MetadataState::Some(metadata)
                        }
                        Err(e) => {
                            error!("make_thumb: {}", e);
                            MetadataState::Errored
                        }
                    };

                    done.push(coords);
                }

                default => {}
            }
        }

        for coords in &done {
            handles.remove(coords);
        }

        std::mem::swap(&mut handles, &mut self.thumb_handles);
    }
}

impl App {
    fn new(
        files: Vec<Arc<File>>,
        db: Arc<database::Database>,
        thumbnailer_threads: usize,
        base_id: u64,
    ) -> Self {
        let images: Vec<image::Image> = files
            .into_iter()
            .enumerate()
            .map(|(i, file)| image::Image::from(i, file))
            .collect();

        let view = view::View::new(images.len());

        let groups = Groups::from(images, vec2_u32(view.grid_size));

        let window_settings = WindowSettings::new("pix", [800.0, 600.0])
            .exit_on_esc(true)
            .fullscreen(false);

        let mut window: PistonWindow = window_settings.build().expect("window build");
        window.set_ups(100);

        let texture_context = window.create_texture_context();

        Self {
            db,

            groups,

            new_window_settings: None,
            window_settings,
            window,
            texture_context,

            view,
            panning: false,
            zooming: None,
            cursor_captured: false,

            thumb_executor: futures::executor::ThreadPool::builder()
                .pool_size(thumbnailer_threads)
                .name_prefix("thumbnailer")
                .create()
                .unwrap(),

            thumb_threads: thumbnailer_threads,

            shift_held: false,

            focus: None,

            base_id,
        }
    }

    fn rebuild_window(&mut self, settings: WindowSettings) {
        self.groups.reset();

        self.window_settings = settings.clone();
        self.window = settings.build().expect("window build");

        self.focus = None;
        self.panning = false;
        self.cursor_captured = false;
        self.zooming = None;
    }

    fn target_size(&self) -> u32 {
        ((self.view.zoom * UPSIZE_FACTOR) as u32).next_power_of_two()
    }

    fn update(&mut self, args: UpdateArgs) {
        let _s = ScopedDuration::new("update");
        let _stopwatch = Stopwatch::from_millis(10);

        let grid_size = vec2_u32(self.view.grid_size);
        if grid_size != self.groups.grid_size {
            self.groups.regroup(grid_size);
        }

        if let Some(z) = self.zooming {
            self.zoom(z.mul_add(args.dt, 1.0));
        }

        if self.focus.is_none() {
            self.recalc_visible();
            self.focus = Some(vec2_add(self.view.coords(0), self.view.mouse()));
        }

        let target_size = self.target_size();

        let texture_settings = TextureSettings::new();

        for group in self.groups.groups.values_mut() {
            group.recv_thumbs();
            group.make_thumbs(self.base_id, &self.db, &mut self.thumb_executor);
            group.load_cache(
                &self.view,
                &*self.db,
                target_size,
                &texture_settings,
                &mut self.texture_context,
            )
        }
    }

    fn resize(&mut self, win_size: Vector2<u32>) {
        let _s = ScopedDuration::new("resize");
        self.view.resize_to(win_size);
        self.focus = None;
    }

    fn recalc_visible(&mut self) {
        let _s = ScopedDuration::new("recalc_visible");

        for group in self.groups.groups.values_mut() {
            group.recheck();
        }
    }

    fn mouse_move(&mut self, loc: Vector2<f64>) {
        self.view.mouse_to(loc);
        self.maybe_refocus();
    }

    fn force_refocus(&mut self) {
        self.focus = None;
    }

    fn maybe_refocus(&mut self) {
        if let Some(old) = self.focus {
            let new = self.view.mouse_dist(0);
            let delta = vec2_sub(new, old);
            if vec2_square_len(delta) > 500.0 {
                self.force_refocus();
            }
        }
    }

    fn mouse_zoom(&mut self, v: f64) {
        let _s = ScopedDuration::new("mouse_zoom");
        for _ in 0..(v as isize) {
            self.zoom(1.0 + self.zoom_increment());
        }
        for _ in (v as isize)..0 {
            self.zoom(1.0 - self.zoom_increment());
        }
    }

    fn mouse_pan(&mut self, delta: Vector2<f64>) {
        if self.panning {
            let _s = ScopedDuration::new("mouse_pan");
            if self.cursor_captured {
                self.view.center_mouse();
            }
            self.trans(vec2_scale(delta, 4.0));
        }
    }

    fn shift_increment(&self) -> f64 {
        if self.shift_held {
            // snap to zoom
            if self.view.zoom > 100.0 {
                self.view.zoom
            } else {
                100.0
            }
        } else {
            20.0
        }
    }

    fn zoom_increment(&self) -> f64 {
        if self.shift_held {
            0.5
        } else {
            0.1
        }
    }

    fn trans(&mut self, trans: Vector2<f64>) {
        self.view.trans_by(trans);
        self.maybe_refocus();
    }

    fn zoom(&mut self, ratio: f64) {
        self.view.zoom_by(ratio);
        self.maybe_refocus();
    }

    fn reset(&mut self) {
        self.view.reset();
        self.force_refocus();
    }

    fn button(&mut self, b: ButtonArgs) {
        let _s = ScopedDuration::new("button");
        match (b.state, b.button) {
            (ButtonState::Press, Button::Keyboard(Key::Z)) => {
                self.reset();
            }

            (ButtonState::Press, Button::Keyboard(Key::F)) => {
                let mut settings = self.window_settings.clone();
                settings.set_fullscreen(!settings.get_fullscreen());
                self.new_window_settings = Some(settings);
            }

            (ButtonState::Press, Button::Keyboard(Key::T)) => {
                self.cursor_captured = !self.cursor_captured;
                self.window.set_capture_cursor(self.cursor_captured);
                self.panning = self.cursor_captured;
                self.view.center_mouse();
            }

            (ButtonState::Press, Button::Keyboard(Key::Up)) => {
                self.trans([0.0, self.shift_increment()]);
            }

            (ButtonState::Press, Button::Keyboard(Key::Down)) => {
                self.trans([0.0, -self.shift_increment()]);
            }

            (ButtonState::Press, Button::Keyboard(Key::Left)) => {
                self.trans([self.shift_increment(), 0.0]);
            }

            (ButtonState::Press, Button::Keyboard(Key::Right)) => {
                self.trans([-self.shift_increment(), 0.0]);
            }

            (ButtonState::Press, Button::Keyboard(Key::PageUp)) => {
                self.view.center_mouse();
                self.zoom(1.0 - self.zoom_increment());
            }

            (ButtonState::Press, Button::Keyboard(Key::PageDown)) => {
                self.view.center_mouse();
                self.zoom(1.0 + self.zoom_increment());
            }

            (state, Button::Keyboard(Key::LShift)) | (state, Button::Keyboard(Key::RShift)) => {
                self.shift_held = state == ButtonState::Press;
            }

            (state, Button::Mouse(MouseButton::Middle)) => {
                self.panning = state == ButtonState::Press;
            }

            (state, Button::Mouse(MouseButton::Left)) => {
                self.zooming = (state == ButtonState::Press).as_some(5.0);
            }

            (state, Button::Mouse(MouseButton::Right)) => {
                self.zooming = (state == ButtonState::Press).as_some(-5.0);
            }

            _ => {}
        }
    }

    fn draw_2d(
        e: &Event,
        c: Context,
        g: &mut G2d,
        view: &view::View,
        groups: &BTreeMap<[u32; 2], Group>,
    ) {
        clear([0.0, 0.0, 0.0, 1.0], g);

        let args = e.render_args().expect("render args");
        let draw_state = DrawState::default().scissor([0, 0, args.draw_size[0], args.draw_size[1]]);

        let _black = color::hex("000000");
        let _missing_color = color::hex("888888");
        let op_color = color::hex("222222");

        //let zoom = (view.zoom * view.zoom) / (view.zoom + 1.0);
        let zoom = view.zoom;

        for group in groups.values() {
            //let [x, y] = vec2_add(vec2_f64(group.min_extent), view.trans);
            //let [w, h] = vec2_f64(vec2_sub(group.max_extent, group.min_extent));

            for image in group.images.values() {
                let [x, y] = view.coords(image.i);

                if !view.is_visible([x, y]) {
                    continue;
                }

                let trans = c.transform.trans(x, y);

                if image.draw(trans, zoom, &group.tiles, &draw_state, g) {
                    continue;
                }

                //if thumb_handles.contains_key(&i) {
                //    rectangle(op_color, [0.0, 0.0, zoom, zoom], trans, g);
                //    rectangle(black, [1.0, 1.0, zoom - 2.0, zoom - 2.0], trans, g);
                //} else {
                //    rectangle(missing_color, [zoom / 2.0, zoom / 2.0, 1.0, 1.0], trans, g);
                //}
            }
        }
    }

    fn run(&mut self) {
        loop {
            let _s = ScopedDuration::new("run_loop");

            if let Some(settings) = self.new_window_settings.take() {
                self.rebuild_window(settings);
            }

            if let Some(e) = self.window.next() {
                let _s = ScopedDuration::new("run_loop_next");

                e.update(|args| {
                    self.update(*args);
                });

                e.resize(|args| {
                    self.resize(args.draw_size);
                });

                e.mouse_scroll(|[_, v]| {
                    self.mouse_zoom(v);
                });

                e.mouse_cursor(|loc| {
                    self.mouse_move(loc);
                });

                e.mouse_relative(|delta| {
                    self.mouse_pan(delta);
                });

                e.button(|b| self.button(b));

                // borrowck
                let v = &self.view;
                let groups = &self.groups.groups;
                self.window.draw_2d(&e, |c, g, _device| {
                    let _s = ScopedDuration::new("draw_2d");
                    Self::draw_2d(&e, c, g, v, groups);
                });
            } else {
                break;
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct File {
    path: String,
    modified: u64,
    file_size: u64,
}

fn find_images(dirs: Vec<String>) -> Vec<Arc<File>> {
    let _s = ScopedDuration::new("find_images");

    let mut ret = Vec::new();

    for dir in dirs {
        for entry in walkdir::WalkDir::new(&dir) {
            let i = ret.len();
            if i > 0 && i % 1000 == 0 {
                info!("Found {} images...", i);
            }

            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    error!("Walkdir error: {:?}", e);
                    continue;
                }
            };

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(e) => {
                    error!("Metadata lookup error: {:?}: {:?}", entry, e);
                    continue;
                }
            };

            if metadata.is_dir() {
                info!("Searching in {:?}", entry.path());
                continue;
            }

            let file_size = metadata.len();

            let modified: u64 = metadata
                .modified()
                .expect("metadata modified")
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .expect("duration since unix epoch")
                .as_secs();

            let path = entry.path();

            let path = match path.canonicalize() {
                Ok(path) => path,
                Err(e) => {
                    error!("unable to canonicalize: {:?} {:?}", path, e);
                    continue;
                }
            };

            let path = if let Some(path) = path.to_str() {
                path.to_owned()
            } else {
                error!("Skipping non-utf8 path: {:?}", path);
                continue;
            };

            let file = File {
                path,
                modified,
                file_size,
            };

            ret.push(Arc::new(file));
        }
    }

    ret.sort();
    ret
}

fn main() {
    env_logger::init();

    /////////////////
    // PARSE FLAGS //
    /////////////////

    let matches = clap::App::new("pix")
        .version("1.0")
        .author("Mason Larobina <mason.larobina@gmail.com>")
        .arg(
            Arg::with_name("paths")
                .value_name("PATHS")
                .multiple(true)
                .help("Images or directories of images to view."),
        )
        .arg(
            Arg::with_name("threads")
                .long("--threads")
                .value_name("COUNT")
                .takes_value(true)
                .required(false)
                .help("Set number of background thumbnailer threads."),
        )
        .arg(
            Arg::with_name("db_path")
                .long("--db_path")
                .value_name("PATH")
                .takes_value(true)
                .help("Alternate thumbnail database path."),
        )
        .get_matches();

    let paths = matches
        .values_of_lossy("paths")
        .unwrap_or_else(|| vec![String::from(".")]);
    info!("Paths: {:?}", paths);

    let thumbnailer_threads: usize = if let Some(threads) = matches.value_of("threads") {
        threads.parse().expect("not an int")
    } else {
        num_cpus::get()
    };
    info!("Thumbnailer threads {}", thumbnailer_threads);

    let db_path: String = if let Some(db_path) = matches.value_of("db_path") {
        db_path.to_owned()
    } else {
        let mut db_path = dirs::cache_dir().expect("cache dir");
        db_path.push("pix/thumbs.db");
        db_path.to_str().expect("db path as str").to_owned()
    };
    info!("Database path: {}", db_path);

    /////////
    // RUN //
    /////////

    let files = find_images(paths);
    if files.is_empty() {
        error!("No files found, exiting.");
        std::process::exit(1);
    } else {
        info!("Found {} files", files.len());
    }

    let db = database::Database::open(&db_path).expect("db open");

    let base_id = db.reserve(files.len());

    {
        let _s = ScopedDuration::new("uptime");
        App::new(files, Arc::new(db), thumbnailer_threads, base_id).run();
    }

    stats::dump();
}
