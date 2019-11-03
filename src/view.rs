use crate::vec2_div;
use vecmath::{vec2_add, vec2_mul, vec2_scale, vec2_sub, Vector2};

#[derive(Debug, Default)]
pub struct View {
    num_images: f64,

    // Window dimensions.
    win_size: Vector2<f64>,

    // Logical dimensions.
    grid_size: Vector2<f64>,

    // View offsets.
    trans: Vector2<f64>,

    // Scale from logical to physical coordinates.
    pub zoom: f64,

    min_zoom: f64,

    // Mouse coordinates.
    pub mouse: Vector2<f64>,

    // Has the user panned or zoomed?
    auto: bool,
}

impl View {
    pub fn new(num_images: usize) -> Self {
        Self {
            num_images: num_images as f64,
            win_size: [800., 600.],
            grid_size: [1.0, 1.0],
            auto: true,
            ..Default::default()
        }
    }

    pub fn center_mouse(&mut self) {
        self.mouse = vec2_scale(self.win_size, 0.5);
    }

    pub fn reset(&mut self) {
        self.auto = true;

        let [w, h] = self.win_size;

        self.zoom = {
            let px_per_image = (w * h) / self.num_images;
            px_per_image.sqrt()
        };

        self.grid_size = {
            let grid_w = f64::max(1.0, (w / self.zoom).floor());
            let grid_h = (self.num_images / grid_w).ceil();
            [grid_w, grid_h]
        };

        // Numer of rows takes the overflow, rescale to ensure the grid fits the window.
        let grid_px = vec2_scale(self.grid_size, self.zoom);
        if h < grid_px[1] {
            self.zoom *= h / grid_px[1];
        }

        // Add black border.
        self.zoom *= 0.95;

        self.min_zoom = self.zoom * 0.5;

        self.trans = {
            let grid_px = vec2_scale(self.grid_size, self.zoom);
            let border_px = vec2_sub(self.win_size, grid_px);
            vec2_scale(border_px, 0.5)
        };
    }

    pub fn resize(&mut self, win_size: Vector2<f64>) {
        self.win_size = win_size;
        if self.auto {
            self.reset();
        }
    }

    pub fn trans(&mut self, trans: Vector2<f64>) {
        self.auto = false;
        self.trans = vec2_add(self.trans, trans);
    }

    pub fn zoom(&mut self, ratio: f64) {
        self.auto = false;

        let zoom = self.zoom;
        self.zoom = f64::max(self.min_zoom, zoom * ratio);

        let bias = {
            let grid_pos = vec2_sub(self.mouse, self.trans);
            let grid_px = vec2_scale(self.grid_size, zoom);
            vec2_div(grid_pos, grid_px)
        };

        let trans = {
            let grid_delta = vec2_scale(self.grid_size, self.zoom - zoom);
            vec2_mul(grid_delta, bias)
        };

        self.trans = vec2_sub(self.trans, trans);
    }

    pub fn coords(&self, i: usize) -> Vector2<f64> {
        let grid_w = self.grid_size[0] as usize;
        let coords = [(i % grid_w) as f64, (i / grid_w) as f64];
        vec2_add(self.trans, vec2_scale(coords, self.zoom))
    }

    pub fn is_visible(&self, min: Vector2<f64>) -> bool {
        let max = vec2_add(min, [self.zoom, self.zoom]);
        let [w, h] = self.win_size;
        (max[0] > 0.0 && min[0] < w) && (max[1] > 0.0 && min[1] < h)
    }

    pub fn visible_ratio(&self, [x_min, y_min]: Vector2<f64>) -> f64 {
        let [x_max, y_max] = vec2_add([x_min, y_min], [self.zoom, self.zoom]);
        let [w, h] = self.win_size;
        f64::max(
            f64::min(((x_max / w) - 0.5).abs(), ((x_min / w) - 0.5).abs()),
            f64::min(((y_max / h) - 0.5).abs(), ((y_min / h) - 0.5).abs()),
        ) + 0.5
    }
}

#[test]
fn view_vis_test() {
    let view = View {
        win_size: [200.0, 100.0],
        grid_size: [20.0, 10.0],
        zoom: 10.0,
        ..Default::default()
    };

    assert_eq!(view.coords(0), [0.0, 0.0]);
    assert_eq!(view.coords(1), [10.0, 0.0]);
    assert_eq!(view.coords(20), [0.0, 10.0]);

    assert_eq!(view.visible_ratio([0.0, 0.0]), 0.95);
    assert_eq!(view.visible_ratio([190.0, 0.0]), 0.95);
    assert_eq!(view.visible_ratio([190.0, 90.0]), 0.95);
    assert_eq!(view.visible_ratio([0.0, 90.0]), 0.95);

    assert_eq!(view.visible_ratio([-20.0, 0.0]), 1.05);
    assert_eq!(view.visible_ratio([210.0, 0.0]), 1.05);

    assert_eq!(view.visible_ratio([0.0, -20.0]), 1.1);
    assert_eq!(view.visible_ratio([0.0, 110.0]), 1.1);
}
