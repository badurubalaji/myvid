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
    // Two hidden modes, both of which are this same binary re-executed.
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        // The sandboxed decode process. Started by the player, never by hand.
        Some("--decode-worker") => {
            let socket = args
                .next()
                .and_then(|fd| fd.parse().ok())
                .expect("--decode-worker needs the socket descriptor");
            engine::worker::run(socket);
        }
        // Landlock cannot be undone once applied, so proving it works needs a
        // process of its own. This is that process.
        Some("--sandbox-selftest") => {
            let path = args.next().unwrap_or_else(|| "/etc/hostname".to_owned());
            engine::sandbox::selftest(std::path::Path::new(&path));
        }
        _ => {}
    }

    ui::run()
}
