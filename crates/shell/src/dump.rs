//! A text description of what is on the panel, written to a file.
//!
//! Checking "does the firmware draw anything" otherwise means looking at the
//! window, and a screenshot is not always available: on Windows a background
//! window cannot raise itself while another application holds the foreground,
//! so an automated capture photographs whatever the user is actually doing.
//! This writes what the panel holds instead, which is checkable from a script
//! and reviewable in a log.
//!
//! Set `ESP32_SCREEN_DUMP` to a path to turn it on.

use devices::st7789::ScreenHandle;
use std::io::Write;

/// Columns and rows in the thumbnail. Small enough to read in a terminal.
const COLS: usize = 48;
const ROWS: usize = 18;

/// Darker than this counts as unlit, for the "is anything drawn" question.
const LIT: u32 = 24;

pub struct ScreenDump {
    path: std::path::PathBuf,
    last: std::time::Instant,
    /// Frame counter as of the previous dump, to report whether it is moving.
    previous_generation: u64,
}

impl ScreenDump {
    /// `None` unless `ESP32_SCREEN_DUMP` names a file.
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os("ESP32_SCREEN_DUMP")?;
        Some(ScreenDump {
            path: path.into(),
            // Far enough back that the first frame dumps immediately.
            last: std::time::Instant::now() - std::time::Duration::from_secs(60),
            previous_generation: 0,
        })
    }

    /// Write a report, at most once a second. Rewrites the file each time, so
    /// the reader always sees the current panel rather than a growing log.
    pub fn tick(&mut self, screen: &ScreenHandle) {
        if self.last.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        self.last = std::time::Instant::now();

        let (width, height, generation, pixels) = {
            let guard = match screen.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            (guard.width as usize, guard.height as usize, guard.generation, guard.pixels.clone())
        };
        if width == 0 || height == 0 {
            return;
        }

        let report = render(width, height, generation, self.previous_generation, &pixels);
        self.previous_generation = generation;
        if let Ok(mut f) = std::fs::File::create(&self.path) {
            let _ = f.write_all(report.as_bytes());
        }
    }
}

/// RGB565 to 8-bit channels.
fn rgb(p: u16) -> (u32, u32, u32) {
    let r = ((p >> 11) & 0x1f) as u32;
    let g = ((p >> 5) & 0x3f) as u32;
    let b = (p & 0x1f) as u32;
    ((r * 255) / 31, (g * 255) / 63, (b * 255) / 31)
}

fn render(
    width: usize,
    height: usize,
    generation: u64,
    previous: u64,
    pixels: &[u16],
) -> String {
    let mut lit = 0usize;
    let mut counts: std::collections::HashMap<u16, usize> = std::collections::HashMap::new();
    for &p in pixels.iter().take(width * height) {
        let (r, g, b) = rgb(p);
        if r + g + b > LIT {
            lit += 1;
        }
        *counts.entry(p).or_default() += 1;
    }

    let total = width * height;
    let mut out = String::new();
    out.push_str(&format!("panel      {width}x{height}\n"));
    out.push_str(&format!(
        "frames     {generation} ({} since last dump)\n",
        generation.saturating_sub(previous)
    ));
    out.push_str(&format!(
        "lit        {lit}/{total} pixels ({:.1}%)\n",
        if total > 0 { lit as f32 * 100.0 / total as f32 } else { 0.0 }
    ));

    let mut top: Vec<(u16, usize)> = counts.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    out.push_str("colours    ");
    for (colour, n) in top.iter().take(4) {
        let (r, g, b) = rgb(*colour);
        out.push_str(&format!("#{r:02x}{g:02x}{b:02x}x{n} "));
    }
    out.push('\n');

    // Thumbnail. Each cell averages the block of pixels under it, so text and
    // shapes survive well enough to tell a menu from an empty screen.
    const RAMP: &[u8] = b" .:-=+*#%@";
    out.push_str("\nthumbnail\n");
    for row in 0..ROWS {
        for col in 0..COLS {
            let x0 = col * width / COLS;
            let x1 = ((col + 1) * width / COLS).max(x0 + 1);
            let y0 = row * height / ROWS;
            let y1 = ((row + 1) * height / ROWS).max(y0 + 1);
            let mut sum = 0u32;
            let mut n = 0u32;
            for y in y0..y1.min(height) {
                for x in x0..x1.min(width) {
                    let (r, g, b) = rgb(pixels[y * width + x]);
                    sum += (r + g + b) / 3;
                    n += 1;
                }
            }
            let mean = if n > 0 { sum / n } else { 0 };
            let idx = (mean as usize * (RAMP.len() - 1)) / 255;
            out.push(RAMP[idx] as char);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(colour: u16, w: usize, h: usize) -> Vec<u16> {
        vec![colour; w * h]
    }

    #[test]
    fn an_unlit_panel_reports_nothing_drawn() {
        let report = render(32, 16, 0, 0, &solid(0x0000, 32, 16));
        assert!(report.contains("lit        0/512"), "{report}");
    }

    #[test]
    fn a_white_panel_reports_every_pixel_lit() {
        let report = render(32, 16, 5, 0, &solid(0xffff, 32, 16));
        assert!(report.contains("lit        512/512"), "{report}");
        // Fully lit should be the densest ramp character throughout.
        assert!(report.contains("@@@@"), "{report}");
    }

    #[test]
    fn frame_movement_is_reported() {
        let report = render(8, 8, 40, 30, &solid(0, 8, 8));
        assert!(report.contains("frames     40 (10 since last dump)"), "{report}");
    }

    #[test]
    fn the_thumbnail_has_the_expected_shape() {
        let report = render(320, 240, 1, 0, &solid(0x1234, 320, 240));
        let thumb: Vec<&str> = report
            .lines()
            .skip_while(|l| !l.starts_with("thumbnail"))
            .skip(1)
            .collect();
        assert_eq!(thumb.len(), ROWS);
        assert!(thumb.iter().all(|l| l.chars().count() == COLS));
    }

    #[test]
    fn half_a_screen_of_content_shows_in_the_thumbnail() {
        // The case that matters: firmware drew something, but not everywhere.
        let (w, h) = (64usize, 32usize);
        let mut pixels = solid(0x0000, w, h);
        for y in 0..h / 2 {
            for x in 0..w {
                pixels[y * w + x] = 0xffff;
            }
        }
        let report = render(w, h, 1, 0, &pixels);
        assert!(report.contains("(50.0%)"), "{report}");
        let thumb: Vec<&str> = report
            .lines()
            .skip_while(|l| !l.starts_with("thumbnail"))
            .skip(1)
            .collect();
        assert!(thumb[0].contains('@'), "top half lit: {:?}", thumb[0]);
        assert!(thumb[ROWS - 1].trim().is_empty(), "bottom dark: {:?}", thumb[ROWS - 1]);
    }
}
