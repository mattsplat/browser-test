//! Canvas 2D, a basic subset: a `<canvas>`'s drawing, kept as a list of drawing commands and
//! rasterised on the CPU with vello_cpu (through anyrender, as Blitz's screenshots are). The
//! pixels are shown as the canvas element's image (`show`): Blitz paints any element's raster
//! image data, and lays a canvas out at its `width` and `height` either way, so no fork
//! change was needed.
//!
//! The commands are replayed onto a fresh pixel buffer each time the canvas is shown, after
//! the page's code returns (`RunningPack::show_canvases`), not on every call. Clearing the
//! whole canvas drops the commands before it, so an animation that clears each frame keeps a
//! short list.

use anyrender::{Glyph, PaintScene};
use peniko::kurbo::{Affine, Arc, BezPath, Point, Rect, Shape, Stroke};
use peniko::{BlendMode, Color, Compose, Fill, FontData, Mix};

/// The most commands a canvas keeps (and path elements a path holds) before refusing more.
const MAX_COMMANDS: usize = 1 << 20;
/// The biggest canvas, in pixels (64 MB of RGBA), and side.
const MAX_PIXELS: u64 = 16 << 20;
const MAX_SIDE: u32 = 16384;

#[derive(Clone)]
struct State {
    fill: (Color, String),
    stroke: (Color, String),
    line_width: f64,
    font: String,
    font_size: f32,
}

impl Default for State {
    fn default() -> Self {
        Self { fill: (Color::BLACK, "#000000".into()), stroke: (Color::BLACK, "#000000".into()), line_width: 1.0, font: "10px sans-serif".into(), font_size: 10.0 }
    }
}

enum Command {
    Fill(BezPath, Color),
    Stroke(BezPath, Color, f64),
    Clear(Rect),
    Text { font: FontData, size: f32, coords: Vec<i16>, glyphs: Vec<Glyph>, at: (f64, f64), color: Color },
}

/// One canvas's 2D context.
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    commands: Vec<Command>,
    state: State,
    saved: Vec<State>,
    path: BezPath,
    /// Drawn on since last shown.
    pub dirty: bool,
}

/// A colour as the page wrote it, parsed (CSS colours: names, hex, `rgb()`...).
fn parse_color(text: &str) -> Option<Color> {
    peniko::color::parse_color(text.trim()).ok().map(|c| c.to_alpha_color())
}

/// A colour as `fillStyle` reads back: `#rrggbb`, or `rgba(r, g, b, a)` if not opaque.
fn serialize(color: Color) -> String {
    let [r, g, b, a] = color.to_rgba8().to_u8_array();
    match a {
        255 => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => format!("rgba({r}, {g}, {b}, {})", crate::host::format_number((a as f64 / 255.0 * 1000.0).round() / 1000.0)),
    }
}

impl Canvas {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, commands: Vec::new(), state: State::default(), saved: Vec::new(), path: BezPath::new(), dirty: true }
    }

    /// The size the page asked for, if a canvas may be that big.
    pub fn check_size(width: f64, height: f64) -> Result<(u32, u32), String> {
        let ok = |v: f64| v.is_finite() && v >= 0.0 && v <= MAX_SIDE as f64;
        if !ok(width) || !ok(height) || width as u64 * height as u64 > MAX_PIXELS {
            return Err(format!("RangeError: a canvas can be at most {MAX_SIDE} pixels a side and {} million pixels", MAX_PIXELS >> 20));
        }
        Ok((width as u32, height as u32))
    }

    /// Setting `width` or `height` resets the canvas, as in browsers.
    pub fn resize(&mut self, width: u32, height: u32) {
        *self = Canvas::new(width, height);
    }

    fn push(&mut self, command: Command) -> Result<(), String> {
        if self.commands.len() >= MAX_COMMANDS {
            return Err("the canvas has too many drawing commands; clear it first".into());
        }
        self.commands.push(command);
        self.dirty = true;
        Ok(())
    }

    // ---- State

    pub fn fill_style(&self) -> &str {
        &self.state.fill.1
    }

    pub fn stroke_style(&self) -> &str {
        &self.state.stroke.1
    }

    /// Set `fillStyle` (`fill`) or `strokeStyle`; a value that isn't a colour is ignored, as
    /// in browsers (gradients and patterns aren't supported).
    pub fn set_style(&mut self, fill: bool, text: &str) {
        if let Some(color) = parse_color(text) {
            let slot = if fill { &mut self.state.fill } else { &mut self.state.stroke };
            *slot = (color, serialize(color));
        }
    }

    pub fn line_width(&self) -> f64 {
        self.state.line_width
    }

    pub fn set_line_width(&mut self, width: f64) {
        if width.is_finite() && width > 0.0 {
            self.state.line_width = width;
        }
    }

    pub fn font(&self) -> &str {
        &self.state.font
    }

    /// `font`: only the size (`<n>px`) is used; the family is the system's sans-serif.
    pub fn set_font(&mut self, text: &str) {
        let size = text.split_whitespace().find_map(|w| w.strip_suffix("px")?.parse::<f32>().ok());
        if let Some(size) = size.filter(|s| s.is_finite() && *s > 0.0) {
            self.state.font = text.to_string();
            self.state.font_size = size;
        }
    }

    pub fn save(&mut self) {
        if self.saved.len() < 1024 {
            self.saved.push(self.state.clone());
        }
    }

    pub fn restore(&mut self) {
        if let Some(state) = self.saved.pop() {
            self.state = state;
        }
    }

    // ---- Rectangles

    pub fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64) -> Result<(), String> {
        let rect = Rect::new(x, y, x + w, y + h);
        self.push(Command::Fill(rect.to_path(0.1), self.state.fill.0))
    }

    pub fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64) -> Result<(), String> {
        let rect = Rect::new(x, y, x + w, y + h);
        self.push(Command::Stroke(rect.to_path(0.1), self.state.stroke.0, self.state.line_width))
    }

    pub fn clear_rect(&mut self, x: f64, y: f64, w: f64, h: f64) -> Result<(), String> {
        let rect = Rect::new(x, y, x + w, y + h).abs();
        // Clearing everything: what came before can't show any more.
        if rect.contains_rect(Rect::new(0.0, 0.0, self.width as f64, self.height as f64)) {
            self.commands.clear();
            self.dirty = true;
            return Ok(());
        }
        self.push(Command::Clear(rect))
    }

    // ---- Paths

    fn path_element(&mut self) -> Result<(), String> {
        if self.path.elements().len() >= MAX_COMMANDS {
            return Err("the path is too long".into());
        }
        Ok(())
    }

    pub fn begin_path(&mut self) {
        self.path = BezPath::new();
    }

    pub fn move_to(&mut self, x: f64, y: f64) -> Result<(), String> {
        self.path_element()?;
        self.path.move_to((x, y));
        Ok(())
    }

    /// `lineTo`: from the current point, or a `moveTo` if there's none.
    pub fn line_to(&mut self, x: f64, y: f64) -> Result<(), String> {
        self.path_element()?;
        match self.path.elements().is_empty() {
            true => self.path.move_to((x, y)),
            false => self.path.line_to((x, y)),
        }
        Ok(())
    }

    pub fn close_path(&mut self) {
        if !self.path.elements().is_empty() {
            self.path.close_path();
        }
    }

    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64) -> Result<(), String> {
        self.path_element()?;
        self.path.extend(Rect::new(x, y, x + w, y + h).path_elements(0.1));
        Ok(())
    }

    /// `arc(x, y, radius, start, end, counterclockwise)`: joined to the path by a line from
    /// the current point, as in browsers.
    pub fn arc(&mut self, x: f64, y: f64, radius: f64, start: f64, end: f64, counterclockwise: bool) -> Result<(), String> {
        if radius < 0.0 {
            return Err(format!("IndexSizeError: the radius {radius} is negative"));
        }
        self.path_element()?;
        let tau = std::f64::consts::TAU;
        let mut sweep = end - start;
        sweep = match counterclockwise {
            false if sweep >= tau => tau,
            false => sweep.rem_euclid(tau),
            true if sweep <= -tau => -tau,
            true => -((-sweep).rem_euclid(tau)),
        };
        let arc = Arc::new(Point::new(x, y), (radius, radius), start, sweep, 0.0);
        let mut elements = arc.path_elements(0.1);
        let first = elements.next();
        if let Some(peniko::kurbo::PathEl::MoveTo(p)) = first {
            match self.path.elements().is_empty() {
                true => self.path.move_to(p),
                false => self.path.line_to(p),
            }
        }
        self.path.extend(elements);
        Ok(())
    }

    pub fn fill(&mut self) -> Result<(), String> {
        self.push(Command::Fill(self.path.clone(), self.state.fill.0))
    }

    pub fn stroke(&mut self) -> Result<(), String> {
        self.push(Command::Stroke(self.path.clone(), self.state.stroke.0, self.state.line_width))
    }

    // ---- Text

    /// `fillText(text, x, y)`: shaped with parley in the system's sans-serif at the `font`
    /// size, `y` the baseline (`textBaseline` "alphabetic"), left-aligned.
    pub fn fill_text(&mut self, text: &str, x: f64, y: f64) -> Result<(), String> {
        thread_local! {
            static FONTS: std::cell::RefCell<(parley::FontContext, parley::LayoutContext<()>)> = Default::default();
        }
        let runs = FONTS.with_borrow_mut(|(fonts, layouts)| {
            let mut builder = layouts.ranged_builder(fonts, text, 1.0, true);
            builder.push_default(parley::StyleProperty::FontSize(self.state.font_size));
            let mut layout = builder.build(text);
            layout.break_all_lines(None);
            let mut runs = Vec::new();
            for line in layout.lines().take(1) {
                for item in line.items() {
                    if let parley::PositionedLayoutItem::GlyphRun(glyph_run) = item {
                        let run = glyph_run.run();
                        let baseline = glyph_run.baseline();
                        let glyphs = glyph_run.positioned_glyphs().map(|g| Glyph { id: g.id as _, x: g.x, y: g.y - baseline }).collect();
                        let coords: Vec<i16> = run.normalized_coords().iter().map(|c| c.to_bits()).collect();
                        runs.push((run.font().clone(), run.font_size(), coords, glyphs));
                    }
                }
            }
            runs
        });
        for (font, size, coords, glyphs) in runs {
            self.push(Command::Text { font, size, coords, glyphs, at: (x, y), color: self.state.fill.0 })?;
        }
        Ok(())
    }

    // ---- Pixels

    /// The canvas as RGBA pixels (not premultiplied), `width` × `height`.
    pub fn render(&self) -> Vec<u8> {
        if self.width == 0 || self.height == 0 {
            return Vec::new();
        }
        anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(
            |scene| {
                for command in &self.commands {
                    match command {
                        Command::Fill(path, color) => scene.fill(Fill::NonZero, Affine::IDENTITY, *color, None, path),
                        Command::Stroke(path, color, width) => scene.stroke(&Stroke::new(*width), Affine::IDENTITY, *color, None, path),
                        Command::Clear(rect) => {
                            scene.push_layer(Fill::NonZero, BlendMode::new(Mix::Normal, Compose::Clear), 1.0, Affine::IDENTITY, rect, None, None);
                            scene.fill(Fill::NonZero, Affine::IDENTITY, Color::BLACK, None, rect);
                            scene.pop_layer();
                        }
                        Command::Text { font, size, coords, glyphs, at, color } => scene.draw_glyphs(
                            font,
                            *size,
                            true,
                            coords,
                            Default::default(),
                            Fill::NonZero,
                            *color,
                            1.0,
                            Affine::translate(*at),
                            None,
                            glyphs.iter().copied(),
                        ),
                    }
                }
            },
            self.width,
            self.height,
        )
    }

    /// `toDataURL()`: the pixels as a PNG `data:` URL.
    pub fn to_data_url(&self) -> String {
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, self.width.max(1), self.height.max(1));
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let pixels = match self.width * self.height {
            0 => vec![0; 4],
            _ => self.render(),
        };
        if let Ok(mut writer) = encoder.write_header() {
            let _ = writer.write_image_data(&pixels);
        }
        format!("data:image/png;base64,{}", base64(&png))
    }
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { TABLE[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * width + x) * 4) as usize;
        pixels[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn draws_rectangles_paths_and_clears() {
        let mut c = Canvas::new(20, 20);
        c.set_style(true, "red");
        c.fill_rect(0.0, 0.0, 10.0, 10.0).unwrap();
        c.set_style(true, "#0000ff");
        c.begin_path();
        c.arc(15.0, 15.0, 4.0, 0.0, 7.0, false).unwrap();
        c.fill().unwrap();
        c.clear_rect(0.0, 0.0, 3.0, 3.0).unwrap();
        let p = c.render();
        assert_eq!(pixel(&p, 20, 5, 5), [255, 0, 0, 255]);
        assert_eq!(pixel(&p, 20, 1, 1), [0, 0, 0, 0], "cleared");
        assert_eq!(pixel(&p, 20, 15, 15), [0, 0, 255, 255]);
        assert_eq!(pixel(&p, 20, 19, 0), [0, 0, 0, 0]);
        assert_eq!(c.fill_style(), "#0000ff");
        c.set_style(true, "not a colour");
        assert_eq!(c.fill_style(), "#0000ff");
        c.clear_rect(0.0, 0.0, 20.0, 20.0).unwrap();
        assert!(c.commands.is_empty(), "a full clear drops what came before");
        assert!(c.to_data_url().starts_with("data:image/png;base64,iVBORw0KGgo"));
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert!(Canvas::check_size(100000.0, 1.0).is_err());
    }
}
