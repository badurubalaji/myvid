//! The player's handle on its decode process.
//!
//! Implements the same [`PlaybackEngine`] the UI has always talked to, so
//! nothing above this line knows the decoding moved out of process. What it
//! gains is that the code parsing untrusted media no longer shares an address
//! space — or a filesystem view — with the window.

use std::os::fd::{AsRawFd, OwnedFd};
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
    channel: Arc<Channel>,
    slot: FrameSlot,
    /// Reported by the decoder rather than queried, since it is not ours to ask.
    position: AtomicU64,
    duration: AtomicU64,
    source: Mutex<Option<String>>,
    child: Mutex<Option<Child>>,
    emit: EventSink,
    exporting: Arc<std::sync::atomic::AtomicBool>,
}

impl RemoteEngine {
    /// Start the decode process and wire it up.
    pub fn spawn(emit: EventSink) -> Result<Arc<Self>> {
        let (ours, theirs) = Channel::pair()?;

        // The socket is created without CLOEXEC precisely so it survives the
        // exec; the child is told which number to find it on rather than
        // shuffling descriptors around in a pre-exec hook.
        let handle = theirs.as_fd().as_raw_fd();
        let program = std::env::current_exe().context("locating our own binary")?;

        let child = Command::new(program)
            .arg("--decode-worker")
            .arg(handle.to_string())
            .spawn()
            .context("starting the decode process")?;

        // The parent must let go of its copy, or a dead decoder never looks
        // dead: the socket stays open because we are still holding an end.
        drop(theirs);

        let engine = Arc::new(RemoteEngine {
            channel: Arc::new(ours),
            slot: FrameSlot::new(),
            position: AtomicU64::new(0),
            duration: AtomicU64::new(0),
            source: Mutex::new(None),
            child: Mutex::new(Some(child)),
            emit,
            exporting: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });

        engine.clone().listen();
        Ok(engine)
    }

    fn listen(self: Arc<Self>) {
        std::thread::Builder::new()
            .name("myvid-decoder".into())
            .spawn(move || {
                let mut surface: Option<Arc<Surface>> = None;

                loop {
                    let received = self.channel.recv::<Notice>();
                    let Ok(Some((notice, attached))) = received else {
                        (self.emit)(Event::Error(
                            "the decode process stopped unexpectedly".into(),
                        ));
                        (self.emit)(Event::State(super::State::Idle));
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

                        Notice::Frame { slot, .. } => {
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

    fn request(&self, request: Request, attach: Option<&OwnedFd>) {
        use std::os::fd::AsFd;
        let attached = attach.map(|fd| fd.as_fd());
        if let Err(err) = self.channel.send(&request, attached) {
            (self.emit)(Event::Error(format!("the decoder is not listening: {err}")));
        }
    }
}

impl Drop for RemoteEngine {
    fn drop(&mut self) {
        self.request(Request::Shutdown, None);
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

        // A local file is opened here and handed over as a descriptor. The
        // decoder is never told where anything lives, which is what lets the
        // sandbox deny it the filesystem outright.
        if let Ok((path, _)) = ::gstreamer::glib::filename_from_uri(uri) {
            let file = std::fs::File::open(&path)
                .with_context(|| format!("opening {}", path.display()))?;
            let fd = OwnedFd::from(file);
            self.request(Request::PlayAttached, Some(&fd));
            return Ok(());
        }

        self.request(Request::PlayUri(uri.to_owned()), None);
        Ok(())
    }

    fn play(&self) {
        self.request(Request::Resume, None);
    }

    fn pause(&self) {
        self.request(Request::Pause, None);
    }

    fn seek(&self, to: Duration) {
        self.request(Request::Seek(to.as_nanos() as u64), None);
    }

    fn position(&self) -> Option<Duration> {
        Some(Duration::from_nanos(self.position.load(Ordering::Relaxed)))
    }

    fn duration(&self) -> Option<Duration> {
        let ns = self.duration.load(Ordering::Relaxed);
        (ns > 0).then(|| Duration::from_nanos(ns))
    }

    fn set_volume(&self, volume: f64) {
        self.request(Request::Volume(volume), None);
    }

    fn set_rate(&self, rate: f64) {
        self.request(Request::Rate(rate), None);
    }

    fn frames(&self) -> FrameSlot {
        self.slot.clone()
    }

    fn select_track(&self, kind: TrackKind, id: Option<&str>) {
        self.request(
            Request::SelectTrack {
                kind,
                id: id.map(str::to_owned),
            },
            None,
        );
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
