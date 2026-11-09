use eframe::egui::{self, Color32, FontId, RichText, Stroke, Vec2};

pub const BG: Color32 = Color32::from_rgb(19, 20, 25);
pub const CARD: Color32 = Color32::from_rgb(28, 29, 36);
pub const BORDER: Color32 = Color32::from_rgb(49, 51, 63);
pub const MUTED: Color32 = Color32::from_rgb(151, 155, 175);
pub const TEXT: Color32 = Color32::from_rgb(237, 238, 246);
pub const ACCENT: Color32 = Color32::from_rgb(175, 158, 255);

pub fn setup(ctx: &egui::Context) {
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = CARD;
    style.visuals.extreme_bg_color = BG;
    style.visuals.faint_bg_color = CARD;
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.selection.bg_fill = Color32::from_rgb(76, 61, 117);
    style.visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    style.visuals.hyperlink_color = ACCENT;
    for widget in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
    ] {
        widget.corner_radius = 8.into();
        widget.bg_stroke = Stroke::new(1.0, BORDER);
    }
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(38, 40, 49);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(55, 53, 72);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(68, 59, 95);
    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.spacing.button_padding = Vec2::new(14.0, 10.0);
    style.spacing.interact_size.y = 36.0;
    style
        .text_styles
        .insert(egui::TextStyle::Body, FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Heading, FontId::proportional(25.0));
    ctx.set_style_of(egui::Theme::Dark, style);
    ctx.set_theme(egui::ThemePreference::Dark);
}

pub fn card() -> egui::Frame {
    egui::Frame::NONE
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(14)
        .inner_margin(20)
}

pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).color(MUTED)
}

#[derive(Clone, Copy)]
pub enum Icon {
    Folder,
    File,
    Archive,
    Plus,
    Arrow,
    Image,
    Check,
    Settings,
    Trash,
}

pub fn icon(ui: &mut egui::Ui, kind: Icon, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    paint_icon(ui.painter(), rect, kind, color);
}

pub fn paint_icon(p: &egui::Painter, rect: egui::Rect, kind: Icon, color: Color32) {
    let pt = |x, y| rect.min + Vec2::new(x, y) * rect.size();
    let stroke = Stroke::new(1.6, color);
    let line = |a, b| {
        p.line_segment([a, b], stroke);
    };
    match kind {
        Icon::Folder => {
            let points = vec![
                pt(0.1, 0.3),
                pt(0.1, 0.19),
                pt(0.4, 0.19),
                pt(0.5, 0.3),
                pt(0.9, 0.3),
                pt(0.9, 0.81),
                pt(0.1, 0.81),
                pt(0.1, 0.3),
            ];
            p.add(egui::Shape::line(points, stroke));
        }
        Icon::File => {
            p.add(egui::Shape::line(
                vec![
                    pt(0.25, 0.1),
                    pt(0.61, 0.1),
                    pt(0.8, 0.3),
                    pt(0.8, 0.9),
                    pt(0.25, 0.9),
                    pt(0.25, 0.1),
                ],
                stroke,
            ));
            line(pt(0.6, 0.1), pt(0.6, 0.32));
            line(pt(0.6, 0.32), pt(0.8, 0.32));
        }
        Icon::Archive => {
            p.rect_stroke(
                egui::Rect::from_min_max(pt(0.18, 0.28), pt(0.82, 0.88)),
                3.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            p.rect_stroke(
                egui::Rect::from_min_max(pt(0.1, 0.12), pt(0.9, 0.29)),
                2.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            line(pt(0.42, 0.49), pt(0.58, 0.49));
        }
        Icon::Plus => {
            line(pt(0.5, 0.2), pt(0.5, 0.8));
            line(pt(0.2, 0.5), pt(0.8, 0.5));
        }
        Icon::Arrow => {
            line(pt(0.17, 0.5), pt(0.83, 0.5));
            line(pt(0.6, 0.25), pt(0.85, 0.5));
            line(pt(0.6, 0.75), pt(0.85, 0.5));
        }
        Icon::Image => {
            p.rect_stroke(
                rect.shrink(size_fraction(rect, 0.12)),
                3.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            p.circle_stroke(pt(0.35, 0.35), rect.width() * 0.07, stroke);
            p.add(egui::Shape::line(
                vec![
                    pt(0.15, 0.75),
                    pt(0.42, 0.53),
                    pt(0.57, 0.65),
                    pt(0.74, 0.45),
                    pt(0.87, 0.59),
                ],
                stroke,
            ));
        }
        Icon::Check => {
            line(pt(0.18, 0.52), pt(0.4, 0.73));
            line(pt(0.4, 0.73), pt(0.83, 0.25));
        }
        Icon::Trash => {
            line(pt(0.14, 0.27), pt(0.86, 0.27));
            line(pt(0.37, 0.12), pt(0.63, 0.12));
            p.add(egui::Shape::line(
                vec![
                    pt(0.24, 0.27),
                    pt(0.30, 0.88),
                    pt(0.70, 0.88),
                    pt(0.76, 0.27),
                ],
                stroke,
            ));
            line(pt(0.43, 0.42), pt(0.43, 0.73));
            line(pt(0.57, 0.42), pt(0.57, 0.73));
        }
        Icon::Settings => {
            for (y, x) in [(0.25, 0.4), (0.5, 0.65), (0.75, 0.33)] {
                line(pt(0.12, y), pt(0.88, y));
                p.circle_filled(pt(x, y), rect.width() * 0.07, CARD);
                p.circle_stroke(pt(x, y), rect.width() * 0.07, stroke);
            }
        }
    }
}
fn size_fraction(rect: egui::Rect, value: f32) -> f32 {
    rect.width() * value
}

pub fn button(ui: &mut egui::Ui, kind: Icon, label: &str, primary: bool) -> egui::Response {
    let width = ui.fonts_mut(|f| {
        f.layout_no_wrap(label.into(), FontId::proportional(14.0), TEXT)
            .size()
            .x
    }) + 58.0;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 40.0), egui::Sense::click());
    let color = if primary {
        ACCENT
    } else if response.hovered() {
        Color32::from_rgb(54, 52, 69)
    } else {
        Color32::from_rgb(38, 40, 49)
    };
    let text_color = if primary { BG } else { TEXT };
    ui.painter()
        .rect_filled(rect, 9.0, if ui.is_enabled() { color } else { CARD });
    if !primary {
        ui.painter().rect_stroke(
            rect,
            9.0,
            Stroke::new(1.0, BORDER),
            egui::StrokeKind::Inside,
        );
    }
    let text_color = if ui.is_enabled() { text_color } else { MUTED };
    paint_icon(
        ui.painter(),
        egui::Rect::from_min_size(rect.min + Vec2::new(12.0, 10.0), Vec2::splat(20.0)),
        kind,
        text_color,
    );
    ui.painter().text(
        rect.min + Vec2::new(42.0, 20.0),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(14.0),
        text_color,
    );
    response
}

pub fn brand(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    ui.painter().rect_filled(rect, size * 0.27, ACCENT);
    let p = |x, y| rect.min + Vec2::new(x, y) * size;
    let s = Stroke::new(size * 0.055, BG);
    for (a, b) in [
        (p(0.23, 0.68), p(0.23, 0.32)),
        (p(0.23, 0.32), p(0.47, 0.68)),
        (p(0.47, 0.68), p(0.47, 0.32)),
        (p(0.58, 0.34), p(0.81, 0.67)),
        (p(0.81, 0.34), p(0.58, 0.67)),
    ] {
        ui.painter().line_segment([a, b], s);
    }
}

/// The NX FORMAT wordmark, `height` points tall.
pub fn logo(ui: &mut egui::Ui, height: f32) {
    let id = egui::Id::unique("nxformat-logo");
    let cached = ui
        .ctx()
        .data_mut(|data| data.get_temp::<egui::TextureHandle>(id));
    let texture = cached.unwrap_or_else(|| {
        let image = image::load_from_memory(include_bytes!("../../assets/nxformat-logo.png"))
            .expect("bundled logo")
            .to_rgba8();
        let texture = ui.ctx().load_texture(
            "NX FORMAT",
            egui::ColorImage::from_rgba_unmultiplied(
                [image.width() as usize, image.height() as usize],
                image.as_raw(),
            ),
            egui::TextureOptions::LINEAR,
        );
        ui.ctx()
            .data_mut(|data| data.insert_temp(id, texture.clone()));
        texture
    });
    let size = texture.size_vec2();
    ui.add(
        egui::Image::new(&texture).fit_to_exact_size(Vec2::new(size.x * height / size.y, height)),
    );
}

/// A short tagline in small, spaced capitals.
pub fn tagline(text: &str) -> RichText {
    RichText::new(text.to_uppercase())
        .size(12.5)
        .extra_letter_spacing(2.5)
        .color(MUTED)
}

/// A small circled "i". On hover, `lines` (title, text, note) show in a
/// bubble under it, kept inside the window's right edge.
pub fn info(ui: &mut egui::Ui, lines: &[&str]) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(16.0), egui::Sense::hover());
    let color = if response.hovered() { ACCENT } else { MUTED };
    let p = ui.painter();
    p.circle_stroke(rect.center(), 7.0, Stroke::new(1.3, color));
    p.circle_filled(rect.center() + Vec2::new(0.0, -3.4), 1.0, color);
    p.line_segment(
        [
            rect.center() + Vec2::new(0.0, -1.2),
            rect.center() + Vec2::new(0.0, 3.8),
        ],
        Stroke::new(1.5, color),
    );
    if response.hovered() {
        const WIDTH: f32 = 230.0;
        let right = ui.ctx().content_rect().right() - 12.0;
        let x = (rect.left() - 12.0).min(right - WIDTH);
        egui::Area::new(response.id.with("bubble"))
            .order(egui::Order::Tooltip)
            .fixed_pos(egui::pos2(x, rect.bottom() + 8.0))
            .interactable(false)
            .show(ui.ctx(), |ui| {
                card().inner_margin(12).corner_radius(10).show(ui, |ui| {
                    ui.set_width(WIDTH - 24.0);
                    ui.spacing_mut().item_spacing.y = 4.0;
                    for (i, line) in lines.iter().enumerate() {
                        ui.label(match i {
                            0 => RichText::new(*line).strong().color(ACCENT),
                            1 => RichText::new(*line),
                            _ => muted(*line).small(),
                        });
                    }
                });
            });
    }
    response
}
