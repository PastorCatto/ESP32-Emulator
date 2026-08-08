//! The emulated panel, drawn as a texture.
//!
//! The device model owns the pixels and the bus thread writes them; this only
//! uploads what changed. Reuploading a 320x240 frame every repaint would work
//! and would also burn a texture upload per frame on an image that is usually
//! identical, so the model's write counter decides.

use devices::st7789::ScreenHandle;
use egui::{Color32, ColorImage, TextureHandle, TextureOptions};

/// A frame copied out from under the framebuffer lock.
///
/// Deliberately owns its pixels. The whole point is that the conversion runs
/// after the lock is released, so anything borrowed from the model would
/// defeat it.
struct Snapshot {
    size: (u16, u16),
    generation: u64,
    on: bool,
    inverted: bool,
    bgr: bool,
    pixels: Vec<u16>,
}

impl Snapshot {
    /// 5-6-5 to 8-8-8, the same conversion the model used to do inline.
    fn rgb888(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pixels.len() * 3);
        for &pixel in &self.pixels {
            let p = if self.inverted { !pixel } else { pixel };
            // Repeat the high bits when widening, so full white stays 0xff
            // rather than landing on 0xf8.
            let hi = ((p >> 11) & 0x1f) as u8;
            let mid = ((p >> 5) & 0x3f) as u8;
            let lo = (p & 0x1f) as u8;
            let hi = (hi << 3) | (hi >> 2);
            let mid = (mid << 2) | (mid >> 4);
            let lo = (lo << 3) | (lo >> 2);

            if self.bgr {
                out.extend_from_slice(&[lo, mid, hi]);
            } else {
                out.extend_from_slice(&[hi, mid, lo]);
            }
        }
        out
    }
}

#[derive(Default)]
pub struct ScreenView {
    texture: Option<TextureHandle>,
    /// Write counter of the frame currently uploaded.
    shown: u64,
    /// Size of the uploaded frame, so a board change forces a new texture.
    size: (u16, u16),
}

impl ScreenView {
    /// Draw the panel, filling the available space while keeping its aspect.
    ///
    /// Returns where it was drawn, so a caller can map a click back to a
    /// panel coordinate.
    pub fn show(&mut self, ui: &mut egui::Ui, screen: &ScreenHandle) -> egui::Response {
        self.refresh(ui.ctx(), screen);

        let Some(texture) = &self.texture else {
            return ui.label("no display");
        };

        let (w, h) = self.size;
        let available = ui.available_size();
        // Whole-number scaling wherever it fits: these are small panels shown
        // large, and a fractional factor turns crisp pixel art into mush.
        let fit = (available.x / f32::from(w)).min(available.y / f32::from(h));
        let scale = if fit >= 1.0 { fit.floor() } else { fit };
        let size = egui::vec2(f32::from(w) * scale, f32::from(h) * scale);

        // Centred by hand, and allocated before painting, for two reasons.
        //
        // An `Image` carries no `Sense`, so adding one returns a response that
        // never reports a click however hard you press it -- mimic touch was
        // wired up and silently dead because of exactly that.
        //
        // And the response's rect has to be the *image*, not the space around
        // it. `centered_and_justified` hands back the full panel, so a click
        // would be measured against the wrong rectangle and land somewhere
        // else on the panel -- worse than not working, because it looks like
        // it nearly works.
        let area = ui.available_rect_before_wrap();
        let rect = egui::Rect::from_center_size(area.center(), size);
        let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());

        egui::Image::new(texture)
            // Nearest, for the same reason as the integer scale.
            .texture_options(TextureOptions::NEAREST)
            .paint_at(ui, rect);

        response
    }

    /// Upload the frame if it has changed since the last one.
    ///
    /// Nothing expensive happens while the framebuffer is locked, and that
    /// constraint is load-bearing rather than tidiness. The bus thread holds
    /// the *registry* lock while it dispatches, and the ST7789 model takes
    /// this lock inside that -- so a slow frame here does not merely stall the
    /// panel, it stalls every device behind the registry, the SD card
    /// included. Converting under the lock made SD init time out in the
    /// window while the identical image mounted in 1.6 seconds headless.
    ///
    /// So take a snapshot -- a 150 KB memcpy of raw 5-6-5 -- release, and do
    /// the 76 800-pixel conversion and the texture upload on our own time.
    fn refresh(&mut self, ctx: &egui::Context, screen: &ScreenHandle) {
        let snapshot = {
            // A poisoned lock means a device model panicked mid-frame. The
            // pixels are still structurally fine and a blank window is the
            // worse outcome.
            let guard = match screen.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };

            let size = (guard.width, guard.height);
            let unchanged =
                self.texture.is_some() && self.shown == guard.generation && self.size == size;
            if unchanged {
                return;
            }
            // `on` and the inversion decide what to draw, but resolving them
            // is cheap; only the pixels are worth copying.
            Snapshot {
                size,
                generation: guard.generation,
                on: guard.on,
                inverted: guard.shows_inverted(),
                bgr: guard.bgr,
                pixels: guard.pixels.clone(),
            }
        };

        let (width, height) = snapshot.size;
        let dims: [usize; 2] = [width.into(), height.into()];

        // A panel the driver has not switched on shows black, whatever is in
        // its RAM -- which is also what you see on the real device while it
        // is still running its init sequence.
        let image = if snapshot.on {
            ColorImage::from_rgb(dims, &snapshot.rgb888())
        } else {
            ColorImage::new(dims, vec![Color32::BLACK; dims[0] * dims[1]])
        };
        self.shown = snapshot.generation;
        self.size = snapshot.size;

        match &mut self.texture {
            Some(texture) if texture.size() == dims => {
                texture.set(image, TextureOptions::NEAREST);
            }
            slot => {
                *slot = Some(ctx.load_texture("panel", image, TextureOptions::NEAREST));
            }
        }
    }
}
