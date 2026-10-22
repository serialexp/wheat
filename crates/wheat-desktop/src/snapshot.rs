//! Renders the window into a PNG without a window: the same `App::build_ui`
//! the live window draws, on a headless GPU, with the backend answering
//! inline so every diff is in by the captured frame.

use std::path::Path;

use wgpu_gameui::{HeadlessGpu, write_png};

use crate::app::{App, Screen, Source};

pub const SIZE: (u32, u32) = (1600, 1000);

pub fn capture(
    path: &Path,
    source: Source,
    screen: Screen,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut gpu = HeadlessGpu::new().ok_or("no headless wgpu adapter available")?;
    let mut app = App::inline(source);
    app.screen = screen;
    // The first frame lays out and asks for diffs; the second shows them.
    for frame in 0..2 {
        let mut layers = gpu.layer_stack();
        app.build_ui(&mut layers, SIZE.0 as f32, SIZE.1 as f32);
        app.input.end_frame();
        app.after_frame();
        app.drain_updates();
        if frame == 1 {
            let clear = gpu.clear_color(app.theme.background);
            let pixels = gpu.capture_layers(&layers, SIZE, clear);
            if pixels.len() != SIZE.0 as usize * SIZE.1 as usize * 4 {
                return Err("unexpected GPU readback size".into());
            }
            write_png(path, &pixels, SIZE)?;
        }
    }
    Ok(())
}
