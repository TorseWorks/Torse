//! Standalone CPU preview without a client, world or GPU.
use torse_ui::{Canvas, Control, Part, Pointer, Rect, Text, Theme};

fn run() -> Result<(), String> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: widgets <output.png> [dark|blue]")?;
    let theme = match std::env::args().nth(2).as_deref() {
        None | Some("dark") => Theme::load_default()?,
        Some("blue") => Theme::load_blue()?,
        _ => return Err("skin must be dark or blue".into()),
    };
    let mut canvas = Canvas::try_new(320, 160, 2, None)?;
    theme.paint(&mut canvas, Part::Window, Rect::new(0, 0, 320, 160));
    theme.paint(&mut canvas, Part::Title, Rect::new(1, 1, 318, 26));
    theme.button(&mut canvas, Rect::new(16, 36, 288, 24), true, false);
    theme.checkbox(&mut canvas, Rect::new(16, 76, 14, 14), true);
    theme.paint(&mut canvas, Part::Track, Rect::new(16, 108, 288, 14));
    theme.paint(&mut canvas, Part::Thumb, Rect::new(160, 109, 8, 12));
    if let Ok(mut text) = Text::load() {
        for (label, rect) in [
            ("UI module preview", Rect::new(16, 10, 280, 16)),
            ("Button", Rect::new(136, 41, 80, 16)),
            ("Checkbox", Rect::new(40, 76, 220, 16)),
        ] {
            text.draw(&mut canvas, label, rect, 12, [226, 229, 233, 255]);
        }
    }
    let controls = [Control::Button {
        key: "button",
        rect: Rect::new(16, 36, 288, 24),
    }];
    let mut pointer = Pointer::default();
    pointer.motion([20, 40], &controls);
    pointer.button(true, &controls);
    println!("event={:?}", pointer.button(false, &controls));
    let file = std::fs::File::create(path).map_err(|error| error.to_string())?;
    let mut encoder = png::Encoder::new(file, canvas.pixel_width(), canvas.pixel_height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .map_err(|error| error.to_string())?
        .write_image_data(canvas.as_pixels())
        .map_err(|error| error.to_string())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("UI preview failed: {error}");
        std::process::exit(1);
    }
}
