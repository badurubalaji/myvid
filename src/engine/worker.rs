//! The decode process.
//!
//! This is the same binary re-executed with `--decode-worker`. It owns the
//! GStreamer pipeline and nothing else: no window, no GPU, no access to the
//! user's files. It receives the media as an already-open descriptor, writes
//! frames into a shared buffer, and talks to the player over one socket.
//!
//! The order here is the whole point. GStreamer initialises, builds its plugin
//! registry and constructs the pipeline first — all of which reads widely — and
//! only then is the process confined. Everything after that line is parsing
//! untrusted input with nothing worth stealing in reach.

use std::os::fd::OwnedFd;
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::Duration;

use super::gst::{EventSink, GstEngine};
use super::protocol::{Channel, Layout, Notice, Request};
use super::sandbox;
use super::shm::{Surface, SLOTS};
use super::{Event, PlaybackEngine};

enum Incoming {
    Engine(Event),
    Peer(Request),
    PeerGone,
    Tick,
}

/// Run as the decode process on the socket the player left open. Never returns.
pub fn run(socket: i32) -> ! {
    let channel = Channel::from_fd(unsafe { owned_fd(socket) });
    let code = match serve(channel) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("myvid decoder: {err:#}");
            1
        }
    };
    std::process::exit(code)
}

/// # Safety
/// `fd` must be a valid open descriptor this process owns.
unsafe fn owned_fd(fd: i32) -> OwnedFd {
    use std::os::fd::FromRawFd;
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn serve(channel: Channel) -> anyhow::Result<()> {
    // If the player dies, this process must not outlive it. Without this, a
    // killed player leaves a decoder running — still holding the audio device
    // and the file — because the loop that would notice can itself be blocked.
    #[cfg(target_os = "linux")]
    {
        if let Err(err) = rustix::process::set_parent_process_death_signal(Some(
            rustix::process::Signal::KILL,
        )) {
            eprintln!("myvid decoder: could not arm the parent-death signal: {err}");
        }
        // The player may already have gone in the gap before that call.
        if rustix::process::getppid().is_none() {
            std::process::exit(0);
        }
    }

    let channel = Arc::new(channel);
    let (tx, rx) = mpsc::channel::<Incoming>();

    let engine_tx = tx.clone();
    let emit: EventSink = Arc::new(move |event| {
        let _ = engine_tx.send(Incoming::Engine(event));
    });

    // Everything that reads widely happens before the sandbox: plugin registry,
    // element factories, the audio sink's connection to the session bus.
    let engine = GstEngine::new(emit)?;

    // Confinement waits for the first file. Nothing untrusted has been parsed
    // yet, and knowing the file lets the ruleset name it — so this decoder ends
    // up able to read exactly one film and nothing else. A new file means a new
    // decode process, since Landlock can only ever be narrowed.
    channel.send(
        &Notice::Ready {
            confinement: "awaiting media".to_owned(),
        },
        None,
    )?;

    spawn_reader(channel.clone(), tx.clone());
    spawn_ticker(tx);

    let diagnostics = std::env::var_os("MYVID_DIAG").is_some();
    let mut last_report = std::time::Instant::now();

    let slot = engine.frames();
    let mut surface: Option<Arc<Surface>> = None;
    let mut generation: u64 = 0;
    let mut confined = false;

    for incoming in rx {
        match incoming {
            Incoming::Peer(request) => match request {
                Request::PlayPath(path) => {
                    if confined {
                        channel.send(
                            &Notice::Failed(
                                "this decoder is already confined to another file".into(),
                            ),
                            None,
                        )?;
                        continue;
                    }

                    let confinement = sandbox::confine(Some(std::path::Path::new(&path)));
                    confined = true;
                    eprintln!("myvid decoder: {}", confinement.describe());
                    channel.send(
                        &Notice::Ready {
                            confinement: confinement.describe(),
                        },
                        None,
                    )?;

                    if std::env::var_os("MYVID_DIAG").is_some() {
                        if let Some(home) = std::env::var_os("HOME") {
                            let reachable = std::fs::read_dir(&home).is_ok();
                            eprintln!(
                                "[diag] decoder can read $HOME: {}",
                                if reachable { "YES - NOT CONFINED" } else { "no" }
                            );
                        }
                    }

                    let uri = match ::gstreamer::glib::filename_to_uri(&path, None) {
                        Ok(uri) => uri.to_string(),
                        Err(err) => {
                            channel.send(&Notice::Failed(err.to_string()), None)?;
                            continue;
                        }
                    };
                    if let Err(err) = engine.open(&uri) {
                        channel.send(&Notice::Failed(format!("{err:#}")), None)?;
                    } else {
                        engine.play();
                    }
                }
                Request::PlayUri(uri) => {
                    if !confined {
                        // A network source needs no file at all.
                        let confinement = sandbox::confine(None);
                        confined = true;
                        eprintln!("myvid decoder: {}", confinement.describe());
                        channel.send(
                            &Notice::Ready {
                                confinement: confinement.describe(),
                            },
                            None,
                        )?;
                    }
                    if let Err(err) = engine.open(&uri) {
                        channel.send(&Notice::Failed(format!("{err:#}")), None)?;
                    } else {
                        engine.play();
                    }
                }
                Request::Resume => engine.play(),
                Request::Pause => engine.pause(),
                Request::Seek(ns) => engine.seek(Duration::from_nanos(ns)),
                Request::Volume(v) => engine.set_volume(v),
                Request::AudioEffects(effects) => engine.set_audio_effects(effects),
                Request::Rate(r) => engine.set_rate(r),
                Request::SelectTrack { kind, id } => engine.select_track(kind, id.as_deref()),
                Request::Shutdown => break,
            },

            Incoming::Tick => {
                if let Some(position) = engine.position() {
                    // Non-blocking: a stalled player must not be able to wedge
                    // the decode loop, and the next tick supersedes this one.
                    let _ = channel.try_send(&Notice::Position(position.as_nanos() as u64))?;

                    if diagnostics && last_report.elapsed() >= Duration::from_secs(1) {
                        last_report = std::time::Instant::now();
                        let picture = engine.last_frame();
                        // Positive means the picture is behind the sound.
                        let gap =
                            position.as_secs_f64() - picture.as_secs_f64();
                        eprintln!(
                            "[diag] a/v gap {gap:+.3}s (sound {:.2}s, picture {:.2}s)",
                            position.as_secs_f64(),
                            picture.as_secs_f64()
                        );
                    }
                }
            }

            Incoming::Engine(event) => match event {
                Event::Frame(width, height) => {
                    publish_frame(
                        &channel,
                        &slot,
                        &mut surface,
                        &mut generation,
                        width,
                        height,
                    )?;
                }
                other => {
                    if let Some(notice) = translate(other) {
                        channel.send(&notice, None)?;
                    }
                }
            },

            Incoming::PeerGone => break,
        }
    }

    Ok(())
}

/// Copy the newest decoded frame into the shared buffer and say where it went.
fn publish_frame(
    channel: &Channel,
    slot: &super::FrameSlot,
    surface: &mut Option<Arc<Surface>>,
    generation: &mut u64,
    width: u32,
    height: u32,
) -> anyhow::Result<()> {
    let mut fresh_layout = None;

    let written = slot.read(|frame| {
        let (Some((luma, y_stride)), Some((chroma, uv_stride))) = (frame.plane(0), frame.plane(1))
        else {
            return None;
        };

        let layout = Layout::new(width, height, y_stride, uv_stride, SLOTS);

        // A resolution change means a new buffer, which the player has to be
        // handed before any frame in it makes sense.
        if surface.as_ref().map(|s| s.layout()) != Some(layout) {
            let created = match Surface::create(layout) {
                Ok(created) => Arc::new(created),
                Err(_) => return None,
            };
            fresh_layout = Some(layout);
            *surface = Some(created);
            *generation = 0;
        }

        let surface = surface.as_ref()?;
        let index = (*generation % SLOTS as u64) as u32;
        surface.write(index, luma, chroma);
        *generation += 1;
        Some(index)
    });

    if let (Some(layout), Some(surface)) = (fresh_layout, surface.as_ref()) {
        channel.send(&Notice::Surface(layout), Some(surface.as_fd()))?;
    }

    if let Some(slot_index) = written {
        // Never block on this. A frame notice is superseded by the next one.
        let _ = channel.try_send(&Notice::Frame {
            slot: slot_index,
            generation: *generation,
            sent_ns: epoch_nanos(),
        })?;
    }

    Ok(())
}

fn epoch_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn translate(event: Event) -> Option<Notice> {
    Some(match event {
        Event::Ready(_) | Event::Frame(..) => return None,
        Event::Loaded(info) => Notice::Loaded(info),
        Event::State(state) => Notice::State(state),
        Event::Duration(d) => Notice::Duration(d.as_nanos() as u64),
        Event::Buffering(p) => Notice::Buffering(p),
        Event::Tracks(tracks) => Notice::Tracks(tracks),
        Event::Subtitle { text, start, end } => Notice::Subtitle {
            text,
            start: start.as_nanos() as u64,
            end: end.as_nanos() as u64,
        },
        Event::Eos => Notice::Eos,
        Event::Error(message) => Notice::Failed(message),
        // Clip export runs in the player, which is the process that has a path
        // to write to.
        Event::Export(_) => return None,
    })
}

fn spawn_reader(channel: Arc<Channel>, tx: Sender<Incoming>) {
    std::thread::Builder::new()
        .name("myvid-decoder-rx".into())
        .spawn(move || loop {
            match channel.recv::<Request>() {
                Ok(Some((request, _attached))) => {
                    if tx.send(Incoming::Peer(request)).is_err() {
                        return;
                    }
                }
                Ok(None) | Err(_) => {
                    let _ = tx.send(Incoming::PeerGone);
                    return;
                }
            }
        })
        .expect("decoder reader thread");
}

/// Position has to be asked for; nothing announces it.
fn spawn_ticker(tx: Sender<Incoming>) {
    std::thread::Builder::new()
        .name("myvid-decoder-tick".into())
        .spawn(move || {
            while tx.send(Incoming::Tick).is_ok() {
                std::thread::sleep(Duration::from_millis(120));
            }
        })
        .expect("decoder ticker thread");
}
