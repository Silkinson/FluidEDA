//! 2D camera: maps database-unit (nm) world coordinates to clip space.
//!
//! **Origin rebasing (Phase 1):** the app sets `world_origin` to the viewport
//! centre each frame, and the renderer subtracts it (in f64) from every instance
//! translation before the f32 cast. The vertices this camera then sees are all
//! *near the origin*, so `view_proj` — which subtracts `world_origin` from
//! `center` and works in f32 — never loses precision on million-nm chips. Both
//! sides must use the same `world_origin`; that is the whole trick.

use glam::{Mat4, Vec2};

#[derive(Debug, Clone, Copy)]
pub struct Camera {
    /// World point (nm) at the centre of the viewport.
    pub center: Vec2,
    /// Pixels per nm. Larger = more zoomed in.
    pub scale: f32,
    /// Viewport size in physical pixels.
    pub viewport: Vec2,
    /// Rebasing origin (Phase 1); 0 for now.
    pub world_origin: Vec2,
}

impl Camera {
    pub fn new(viewport: Vec2) -> Self {
        Self {
            center: Vec2::ZERO,
            scale: 1.0,
            viewport,
            world_origin: Vec2::ZERO,
        }
    }

    /// Frame a bbox (nm) with a little margin.
    pub fn fit(&mut self, min: Vec2, max: Vec2) {
        let size = (max - min).max(Vec2::splat(1.0));
        self.center = (min + max) * 0.5;
        let sx = self.viewport.x / size.x;
        let sy = self.viewport.y / size.y;
        self.scale = sx.min(sy) * 0.9;
    }

    /// Column-major view-projection: world nm -> clip [-1,1].
    pub fn view_proj(&self) -> Mat4 {
        let half = self.viewport * 0.5;
        // world -> pixels (y up) -> clip
        let sx = self.scale / half.x;
        let sy = self.scale / half.y;
        let c = self.center - self.world_origin;
        Mat4::from_cols_array(&[
            sx, 0.0, 0.0, 0.0,
            0.0, sy, 0.0, 0.0,
            0.0, 0.0, 1.0, 0.0,
            -c.x * sx, -c.y * sy, 0.0, 1.0,
        ])
    }

    /// Pan by a pixel delta (drag).
    pub fn pan_pixels(&mut self, dx: f32, dy: f32) {
        self.center.x -= dx / self.scale;
        self.center.y += dy / self.scale; // screen y is down, world y is up
    }

    /// Zoom by a factor about a screen-space anchor (wheel), keeping the point
    /// under the cursor fixed.
    pub fn zoom_at(&mut self, factor: f32, screen: Vec2) {
        let before = self.screen_to_world(screen);
        self.scale = (self.scale * factor).clamp(1e-6, 1e6);
        let after = self.screen_to_world(screen);
        self.center += before - after;
    }

    pub fn screen_to_world(&self, screen: Vec2) -> Vec2 {
        let half = self.viewport * 0.5;
        let dx = (screen.x - half.x) / self.scale;
        let dy = -(screen.y - half.y) / self.scale;
        self.center + Vec2::new(dx, dy)
    }
}
