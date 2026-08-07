//! The emulated panel, drawn as a texture.
//!
//! The device model owns the pixels and the bus thread writes them; this only
//! uploads what changed. Reuploading a 320x240 frame every repaint would work
//! and would also burn a texture upload per frame on an image that is usually
//! identical, so the model's write counter decides.

use devices::st7789::ScreenHandle;
use egui::{Color32, ColorImage, TextureHandle, TextureOptions};

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
    fn refresh(&mut self, ctx: &egui::Context, screen: &ScreenHandle) {
        // A poisoned lock means a device model panicked mid-frame. The pixels
        // are still structurally fine and a blank window is the worse outcome.
        let guard = match screen.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        let size = (guard.width, guard.height);
        let unchanged = self.texture.is_some() && self.shown == guard.generation && self.size == size;
        if unchanged {
            return;
        }

        // A panel the driver has not switched on shows black, whatever is in
        // its RAM -- which is also what you see on the real device while it
        // is still running its init sequence.
        let image = if guard.on {
            ColorImage::from_rgb([size.0.into(), size.1.into()], &guard.rgb888())
        } else {
            let count = usize::from(size.0) * usize::from(size.1);
            ColorImage::new([size.0.into(), size.1.into()], vec![Color32::BLACK; count])
        };
        self.shown = guard.generation;
        self.size = size;
        drop(guard);

        let dims: [usize; 2] = [size.0.into(), size.1.into()];
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
