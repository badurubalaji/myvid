//! The player's handle on its decode process.
//!
//! Implements the same [`PlaybackEngine`] the UI has always talked to, so
//! nothing above this line knows the decoding moved out of process. What it
//! gains is that the code parsing untrusted media no longer shares an address
//! space — or a filesystem view — with the window.

use std::os::fd::AsRawFd;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};

use super::frame::FrameSlot;
use super::gst::EventSink;
use super::protocol::{Channel, Notice, Request};
use super::shm::{SharedFrame, Surface};
use super::{ClipRequest, Event, Export, PlaybackEngine, TrackKind};

pub struct RemoteEngine {
    channel: Mutex<Arc<Channel>>,
    slot: FrameSlot,
    /// Reported by the decoder rather than queried, since it is not ours to ask.
    position: AtomicU64,
    duration: AtomicU64,
    source: Mutex<Option<String>>,
    child: Mutex<Option<Child>>,
    /// Bumped on every restart, so a listener for a decoder we have replaced
    /// knows its silence is expected rather than a crash.
    epoch: AtomicU64,
    emit: EventSink,
    exporting: Arc<std::sync::atomic::AtomicBool>,
}

impl RemoteEngine {
    /// Create the player's side. The decode process starts when a file does.
    pub fn spawn(emit: EventSink) -> Result<Arc<Self>> {
        let engine = Arc::new(RemoteEngine {
            channel: Mutex::new(Arc::new(Channel::orphan()?)),
            slot: FrameSlot::new(),
            position: AtomicU64::new(0),
            duration: AtomicU64::new(0),
            source: Mutex::new(None),
            child: Mutex::new(None),
            epoch: AtomicU64::new(0),
            emit,
            exporting: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });

        engine.restart()?;
        Ok(engine)
    }

    /// Replace the decode process with a fresh one.
    ///
    /// Every file gets its own, because the sandbox names the file it may read
    /// and Landlock can only ever be narrowed — a decoder confined for one film
    /// can never be widened to another. The cost is a process start per file,
    /// paid at the moment a person opens something.
    fn restart(self: &Arc<Self>) -> Result<()> {
        let (ours, theirs) = Channel::pair()?;

        // The socket is created without CLOEXEC precisely so it survives the
        // exec; the child is told which number to find it on rather than
        // shuffling descriptors in a pre-exec hook.
        let handle = theirs.as_fd().as_raw_fd();
        let program = match std::env::var_os("MYVID_WORKER") {
            Some(path) => std::path::PathBuf::from(path),
            None => std::env::current_exe().context("locating our own binary")?,
        };

        let child = Command::new(program)
            .arg("--decode-worker")
            .arg(handle.to_string())
            .spawn()
            .context("starting the decode process")?;

        // The parent must let go of its copy, or a dead decoder never looks
        // dead: the socket stays open because we are still holding an end.
        drop(theirs);

        // Retire the previous decoder before announcing the new one.
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(mut previous) = self.child.lock().unwrap().replace(child) {
            let _ = previous.kill();
            let _ = previous.wait();
        }
        *self.channel.lock().unwrap() = Arc::new(ours);

        self.clone().listen(epoch);
        Ok(())
    }

    fn listen(self: Arc<Self>, epoch: u64) {
        std::thread::Builder::new()
            .name("myvid-decoder".into())
            .spawn(move || {
                let channel = self.channel.lock().unwrap().clone();
                let mut surface: Option<Arc<Surface>> = None;
                let diagnostics = std::env::var_os("MYVID_DIAG").is_some();
                let mut delivered: u64 = 0;
                let mut transit_ns: u64 = 0;
                let mut last_report = std::time::Instant::now();

                loop {
                    let Ok(Some((notice, attached))) = channel.recv::<Notice>() else {
                        // A decoder we deliberately replaced going quiet is not
                        // news; only the current one dying is.
                        if self.epoch.load(Ordering::SeqCst) == epoch {
                            (self.emit)(Event::Error(
                                "the decode process stopped unexpectedly".into(),
                            ));
                            (self.emit)(Event::State(super::State::Idle));
                        }
                        return;
                    };

                    match notice {
                        Notice::Ready { confinement } => {
                            eprintln!("myvid: decode process {confinement}");
                        }

                        Notice::Surface(layout) => {
                            surface = attached
                                .and_then(|fd| Surface::adopt(fd, layout).ok())
                                .map(Arc::new);
                            if surface.is_none() {
                                (self.emit)(Event::Error(
                                    "could not map the decoder's frame buffer".into(),
                                ));
                            }
                        }

                        Notice::Frame {
                            slot,
                            generation,
                            sent_ns,
                        } => {
                            if diagnostics {
                                delivered += 1;
                                let now = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_nanos() as u64)
                                    .unwrap_or(0);
                                transit_ns += now.saturating_sub(sent_ns);
                                if last_report.elapsed() >= Duration::from_secs(1) {
                                    last_report = std::time::Instant::now();
                                    eprintln!(
                                        "[diag] delivery: {delivered} frames, {} behind decoder, transit {:.1} ms avg",
                                        generation.saturating_sub(delivered),
                                        transit_ns as f64 / delivered.max(1) as f64 / 1e6
                                    );
                                }
                            }
                            let Some(surface) = surface.as_ref() else {
                                continue;
                            };
                            let layout = surface.layout();
                            self.slot.write(|frame| {
                                frame.set(Box::new(SharedFrame::new(surface.clone(), slot)))
                            });
                            (self.emit)(Event::Frame(layout.width, layout.height));
                        }

                        Notice::Position(ns) => {
                            self.position.store(ns, Ordering::Relaxed);
                        }
                        Notice::Duration(ns) => {
                            self.duration.store(ns, Ordering::Relaxed);
                            (self.emit)(Event::Duration(Duration::from_nanos(ns)));
                        }
                        Notice::State(state) => (self.emit)(Event::State(state)),
                        Notice::Buffering(p) => (self.emit)(Event::Buffering(p)),
                        Notice::Loaded(info) => (self.emit)(Event::Loaded(info)),
                        Notice::Tracks(tracks) => (self.emit)(Event::Tracks(tracks)),
                        Notice::Subtitle { text, start, end } => (self.emit)(Event::Subtitle {
                            text,
                            start: Duration::from_nanos(start),
                            end: Duration::from_nanos(end),
                        }),
                        Notice::Eos => (self.emit)(Event::Eos),
                        Notice::Failed(message) => (self.emit)(Event::Error(message)),
                    }
                }
            })
            .expect("decoder listener thread");
    }

    fn request(&self, request: Request) {
        let channel = self.channel.lock().unwrap().clone();
        if let Err(err) = channel.send(&request, None) {
            (self.emit)(Event::Error(format!("the decoder is not listening: {err}")));
        }
    }
}

impl Drop for RemoteEngine {
    fn drop(&mut self) {
        self.request(Request::Shutdown);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            // It has been asked politely; do not wait forever for an answer.
            std::thread::sleep(Duration::from_millis(120));
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl PlaybackEngine for RemoteEngine {
    fn open(&self, uri: &str) -> Result<()> {
        self.slot.clear();
        self.position.store(0, Ordering::Relaxed);
        self.duration.store(0, Ordering::Relaxed);
        *self.source.lock().unwrap() = Some(uri.to_owned());

        match ::gstreamer::glib::filename_from_uri(uri) {
            // A local file: give it a decoder of its own, confined to it.
            Ok((path, _)) => {
                self.request(Request::PlayPath(path.to_string_lossy().into_owned()));
            }
            // A network source needs no file access at all.
            Err(_) => self.request(Request::PlayUri(uri.to_owned())),
        }

        Ok(())
    }

    fn play(&self) {
        self.request(Request::Resume);
    }

    fn pause(&self) {
        self.request(Request::Pause);
    }

    fn seek(&self, to: Duration) {
        self.request(Request::Seek(to.as_nanos() as u64));
    }

    fn position(&self) -> Option<Duration> {
        Some(Duration::from_nanos(self.position.load(Ordering::Relaxed)))
    }

    fn duration(&self) -> Option<Duration> {
        let ns = self.duration.load(Ordering::Relaxed);
        (ns > 0).then(|| Duration::from_nanos(ns))
    }

    fn set_volume(&self, volume: f64) {
        self.request(Request::Volume(volume));
    }

    fn set_rate(&self, rate: f64) {
        self.request(Request::Rate(rate));
    }

    fn frames(&self) -> FrameSlot {
        self.slot.clone()
    }

    fn select_track(&self, kind: TrackKind, id: Option<&str>) {
        self.request(Request::SelectTrack {
            kind,
            id: id.map(str::to_owned),
        });
    }

    /// Deliberately not delegated to the decoder.
    ///
    /// Exporting a clip means reading a path and writing a new one, which is
    /// exactly the ability the sandbox exists to take away. It stays here, in
    /// the process the user is actually driving.
    fn export_clip(&self, request: ClipRequest) {
        let Some(uri) = self.source.lock().unwrap().clone() else {
            (self.emit)(Event::Export(Export::Failed("nothing is open".into())));
            return;
        };

        if self.exporting.swap(true, Ordering::SeqCst) {
            (self.emit)(Event::Export(Export::Failed(
                "an export is already running".into(),
            )));
            return;
        }

        let emit = self.emit.clone();
        let busy = self.exporting.clone();
        std::thread::Builder::new()
            .name("myvid-export".into())
            .spawn(move || {
                let result = super::gst::run_export(&uri, &request, &emit);
                busy.store(false, Ordering::SeqCst);
                match result {
                    Ok(()) => emit(Event::Export(Export::Done(request.output))),
                    Err(err) => emit(Event::Export(Export::Failed(format!("{err:#}")))),
                }
            })
            .ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::to_uri;
    use std::sync::mpsc;

    /// Dragging the scrub bar across half an hour.
    ///
    /// Not the same as a few discrete jumps: a drag produces a burst of seeks to
    /// wildly different positions, so the demuxer is repeatedly told to restart
    /// somewhere it has never read, while a previous seek may still be settling.
    #[test]
    fn dragging_the_bar_a_long_way() {
        let (Some(media), Some(_worker)) = (
            std::env::var_os("MYVID_TEST_MEDIA"),
            std::env::var_os("MYVID_WORKER"),
        ) else {
            eprintln!("skipped: set MYVID_TEST_MEDIA and MYVID_WORKER");
            return;
        };

        let (tx, rx) = mpsc::channel::<String>();
        let emit: EventSink = Arc::new(move |event| {
            if let Event::Error(message) = event {
                let _ = tx.send(message);
            }
        });

        let engine = RemoteEngine::spawn(emit).expect("decode process starts");
        let uri = to_uri(&media.to_string_lossy()).expect("uri");
        engine.open(&uri).expect("open");
        engine.play();
        std::thread::sleep(Duration::from_secs(3));

        // A pointer sweeping from the start to half an hour in, then back, then
        // further - at the rate a pointer actually moves.
        for pass in 0..3 {
            for step in 0..40u64 {
                let seconds = if pass % 2 == 0 {
                    step * 45
                } else {
                    1800 - step * 40
                };
                engine.seek(Duration::from_secs(seconds));
                std::thread::sleep(Duration::from_millis(25));
            }
            std::thread::sleep(Duration::from_millis(400));
        }
        std::thread::sleep(Duration::from_secs(3));

        let errors: Vec<String> = rx.try_iter().collect();
        assert!(errors.is_empty(), "dragging produced errors: {errors:#?}");
    }

    /// Seeking through the decode process, which is what the player does.
    ///
    /// The in-process tests seek a `GstEngine` directly; the player instead
    /// hands a descriptor across a socket and sends seeks over it. Those are
    /// different code paths, and only one of them is the shipped one.
    #[test]
    fn seeking_through_the_decode_process() {
        let (Some(media), Some(worker)) = (
            std::env::var_os("MYVID_TEST_MEDIA"),
            std::env::var_os("MYVID_WORKER"),
        ) else {
            eprintln!("skipped: set MYVID_TEST_MEDIA and MYVID_WORKER");
            return;
        };
        let _ = worker;

        let (tx, rx) = mpsc::channel::<String>();
        let emit: EventSink = Arc::new(move |event| {
            if let Event::Error(message) = event {
                let _ = tx.send(message);
            }
        });

        let engine = RemoteEngine::spawn(emit).expect("decode process starts");
        let uri = to_uri(&media.to_string_lossy()).expect("uri");
        engine.open(&uri).expect("open");
        engine.play();
        std::thread::sleep(Duration::from_secs(3));

        // Long jumps, not a nudge forward. Matroska keeps its cue index at the
        // end of the file, so seeking half an hour in makes the demuxer read
        // from a part of the source playback has never touched — which is where
        // this actually fails.
        for minutes in [30u64, 62, 45, 90, 15] {
            engine.seek(Duration::from_secs(minutes * 60));
            std::thread::sleep(Duration::from_millis(1800));
        }
        std::thread::sleep(Duration::from_secs(2));

        let errors: Vec<String> = rx.try_iter().collect();
        assert!(errors.is_empty(), "seeking through the decoder failed: {errors:#?}");
    }
}
