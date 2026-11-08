//! The Windows Phone 8 "Metro" look the SDK uses: flat, square, lowercase,
//! one accent colour on a black ground.
//!
//! The icons are drawn with the painter rather than from an icon font, so the
//! app needs no font assets and the glyphs stay crisp at any scale.

use egui::{
    Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response, RichText, Sense, Stroke,
    StrokeKind, Ui, Vec2,
};

/// The default accent. The SDK's own default, overridable with `--accent`.
pub const CYAN: Color32 = Color32::from_rgb(0x1b, 0xa1, 0xe2);
pub const BAR: Color32 = Color32::from_rgb(0x1f, 0x1f, 0x1f);
pub const TILE: Color32 = Color32::from_rgb(0x2b, 0x2b, 0x2b);
pub const DIM: Color32 = Color32::from_rgb(0x9a, 0x9a, 0x9a);
/// Errors are the one colour that is not the accent.
pub const ERROR: Color32 = Color32::from_rgb(0xe5, 0x14, 0x00);
pub const WARN: Color32 = Color32::from_rgb(0xf0, 0xa3, 0x0a);
pub const OK: Color32 = Color32::from_rgb(0x60, 0xa9, 0x17);

/// `RRGGBB` or `#RRGGBB`.
pub fn parse_accent(text: &str) -> Option<Color32> {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(Color32::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8))
}

/// Square corners, no shadows, the accent where egui would use its blue.
pub fn apply(ctx: &egui::Context, accent: Color32) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::BLACK;
    v.window_fill = BAR;
    v.extreme_bg_color = Color32::from_rgb(0x14, 0x14, 0x14);
    v.faint_bg_color = Color32::from_rgb(0x16, 0x16, 0x16);
    v.window_corner_radius = CornerRadius::ZERO;
    v.menu_corner_radius = CornerRadius::ZERO;
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    v.window_stroke = Stroke::new(1.0, Color32::from_gray(0x3a));

    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CornerRadius::ZERO;
    }
    v.widgets.inactive.bg_fill = TILE;
    v.widgets.inactive.weak_bg_fill = TILE;
    v.widgets.hovered.bg_fill = Color32::from_gray(0x3a);
    v.widgets.hovered.weak_bg_fill = Color32::from_gray(0x3a);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, accent);
    v.widgets.active.bg_fill = accent;
    v.widgets.active.weak_bg_fill = accent;
    v.selection.bg_fill = accent.gamma_multiply(0.45);
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    v.hyperlink_color = accent;
    ctx.set_visuals(v);

    ctx.global_style_mut(|s| {
        s.text_styles
            .insert(egui::TextStyle::Heading, FontId::proportional(28.0));
        s.spacing.item_spacing = Vec2::new(8.0, 6.0);
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Play,
    Reset,
    Stop,
    Log,
    Follow,
    Clear,
    Copy,
}

impl Icon {
    /// Draw centred on `c`, fitting a square of half-width `r`.
    fn paint(self, painter: &egui::Painter, c: Pos2, r: f32, colour: Color32) {
        let stroke = Stroke::new(2.0, colour);
        match self {
            Icon::Play => {
                painter.add(egui::Shape::convex_polygon(
                    vec![
                        c + Vec2::new(-0.55 * r, -r),
                        c + Vec2::new(-0.55 * r, r),
                        c + Vec2::new(0.95 * r, 0.0),
                    ],
                    colour,
                    Stroke::NONE,
                ));
            }
            Icon::Stop => {
                painter.rect_filled(
                    Rect::from_center_size(c, Vec2::splat(1.5 * r)),
                    CornerRadius::ZERO,
                    colour,
                );
            }
            Icon::Reset => {
                // Three quarters of a circle with an arrowhead on the open end.
                let (from, to) = (-0.35_f32, 4.9_f32);
                let points: Vec<Pos2> = (0..=24)
                    .map(|i| {
                        let a = from + (to - from) * i as f32 / 24.0;
                        c + Vec2::new(a.cos(), a.sin()) * r
                    })
                    .collect();
                painter.add(egui::Shape::line(points, stroke));
                let tip = c + Vec2::new(to.cos(), to.sin()) * r;
                painter.line_segment([tip, tip + Vec2::new(-0.2 * r, -0.9 * r)], stroke);
                painter.line_segment([tip, tip + Vec2::new(0.9 * r, -0.1 * r)], stroke);
            }
            Icon::Log => {
                for (i, w) in [2.0, 2.0, 1.2].into_iter().enumerate() {
                    let y = c.y + (i as f32 - 1.0) * 0.75 * r;
                    painter.line_segment(
                        [Pos2::new(c.x - r, y), Pos2::new(c.x - r + w * r, y)],
                        stroke,
                    );
                }
            }
            Icon::Follow => {
                painter.line_segment([c + Vec2::new(0.0, -r), c + Vec2::new(0.0, 0.8 * r)], stroke);
                painter.line_segment([c + Vec2::new(-0.7 * r, 0.1 * r), c + Vec2::new(0.0, 0.85 * r)], stroke);
                painter.line_segment([c + Vec2::new(0.7 * r, 0.1 * r), c + Vec2::new(0.0, 0.85 * r)], stroke);
            }
            Icon::Clear => {
                painter.line_segment([c + Vec2::splat(-0.8 * r), c + Vec2::splat(0.8 * r)], stroke);
                painter.line_segment(
                    [c + Vec2::new(0.8 * r, -0.8 * r), c + Vec2::new(-0.8 * r, 0.8 * r)],
                    stroke,
                );
            }
            Icon::Copy => {
                let back = Rect::from_center_size(c + Vec2::new(-0.25 * r, -0.25 * r), Vec2::splat(1.3 * r));
                let front = Rect::from_center_size(c + Vec2::new(0.25 * r, 0.25 * r), Vec2::splat(1.3 * r));
                painter.rect_stroke(back, CornerRadius::ZERO, stroke, StrokeKind::Middle);
                painter.rect_filled(front, CornerRadius::ZERO, BAR);
                painter.rect_stroke(front, CornerRadius::ZERO, stroke, StrokeKind::Middle);
            }
        }
    }
}

/// Height of the circle row; the dots button lines up with it.
const RING_CENTRE_Y: f32 = 22.0;

/// A circled icon, the Windows Phone application-bar button. The label is
/// shown only when the bar is expanded.
pub fn bar_button(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    enabled: bool,
    show_label: bool,
    accent: Color32,
) -> Response {
    let size = Vec2::new(56.0, if show_label { 62.0 } else { 46.0 });
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let painter = ui.painter();

    let centre = Pos2::new(rect.center().x, rect.top() + RING_CENTRE_Y);
    let hot = enabled && response.hovered();
    let pressed = enabled && response.is_pointer_button_down_on();
    let ink = if enabled { Color32::WHITE } else { Color32::from_gray(0x55) };

    if pressed {
        painter.circle_filled(centre, 19.0, Color32::WHITE);
    }
    painter.circle_stroke(centre, 19.0, Stroke::new(2.0, if hot { accent } else { ink }));
    icon.paint(painter, centre, 8.0, if pressed { BAR } else { ink });

    if show_label {
        painter.text(
            Pos2::new(centre.x, rect.bottom() - 4.0),
            Align2::CENTER_BOTTOM,
            label,
            FontId::proportional(11.0),
            ink,
        );
    }
    response
}

/// The `···` that expands the bar.
pub fn dots_button(ui: &mut Ui, accent: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(40.0, 46.0), Sense::click());
    let centre = Pos2::new(rect.center().x, rect.top() + RING_CENTRE_Y);
    let colour = if response.hovered() { accent } else { Color32::WHITE };
    for dx in [-8.0, 0.0, 8.0] {
        ui.painter().circle_filled(centre + Vec2::new(dx, 0.0), 2.0, colour);
    }
    response
}

/// The small upper-case app name that sits above a page title.
pub fn kicker(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).size(11.0).strong());
}

/// Pivot headings: big, light, lowercase, the current one at full strength.
/// Returns true when the selection changed.
pub fn pivot<T: PartialEq + Copy>(ui: &mut Ui, current: &mut T, items: &[(T, &str)]) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 20.0;
        for &(value, name) in items {
            let active = *current == value;
            let text = RichText::new(name)
                .size(30.0)
                .color(if active { Color32::WHITE } else { DIM });
            let response = ui.add(egui::Label::new(text).sense(Sense::click()));
            if response.clicked() && !active {
                *current = value;
                changed = true;
            }
        }
    });
    changed
}
