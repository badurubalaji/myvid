//! myvid — a cross-platform video player.
//!
//! Three layers, deliberately separated:
//!   `engine` decodes (GStreamer) and hands frames to a slot,
//!   `render` uploads those frames and converts NV12 -> RGB on the GPU,
//!   `ui`     draws the chrome and turns input into engine commands.

mod engine;
mod render;
mod ui;

fn main() -> iced::Result {
    ui::run()
}
