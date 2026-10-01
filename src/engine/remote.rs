//! The player's handle on its decode process.
//!
//! Implements the same [`PlaybackEngine`] the UI has always talked to, so
//! nothing above this line knows the decoding moved out of process. What it
//! gains is that the code parsing untrusted media no longer shares an address
//! space — or a filesystem view — with the window.

use std::os::fd::AsRawFd;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::frame::FrameSlot;
use super::gst::EventSink;
use super::protocol::{Channel, Notice, Request};
use super::shm::{SharedFrame, Surface};
use super::{AudioEffects, ClipRequest, Event, Export, PlaybackEngine, TrackKind};

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
    exporting: Arc<AtomicBool>,
    /// The current decoder has been given something to play, and so is confined
    /// to it. The next open needs a new one.
    spent: AtomicBool,
    /// What the player last asked for, so a fresh decoder sounds the same as
    /// the one it replaced.
    volume: Mutex<f64>,
    effects: Mutex<AudioEffects>,
    /// `open` only has `&self`, but restarting hands the listener an `Arc`.
    this: Weak<Self>,
    /// What the watchdog needs to tell a stalled decoder from a paused one.
    watch: Mutex<Watch>,
}

/// If playback is wanted but the position has not moved for this long, the
/// decoder is replaced.
///
/// Rarely, pausing and resuming wedges the pipeline inside the audio sink: the
/// state change never completes, and every later pause or play is ignored, so
/// the picture stays frozen until the file is reopened. It is rare enough that
/// it could not be pinned down, and it happens below any code of ours. A
/// decoder is a disposable process, though, so rather than leave someone
/// staring at a still frame, start a new one at the same position.
const STALL: Duration = Duration::from_secs(3);
/// A file that stalls again straight after being restarted is not going to be
/// fixed by restarting it in a loop.
const RECOVERY_COOLDOWN: Duration = Duration::from_secs(15);

struct Watch {
    /// The person wants it playing: they pressed play, or opened a file.
    playing: bool,
    buffering: bool,
    /// The last time the position moved, or anything happened that could
    /// legitimately hold it still for a moment (a seek, a track change).
    progress: Instant,
    last_position: u64,
    last_recovery: Option<Instant>,
}

impl Watch {
    fn new() -> Self {
        Watch {
            playing: false,
            buffering: false,
            progress: Instant::now(),
            last_position: 0,
            last_recovery: None,
        }
    }
}

impl RemoteEngine {
    /// Create the player's side. The decode process starts when a file does.
    pub fn spawn(emit: EventSink) -> Result<Arc<Self>> {
        let orphan = Arc::new(Channel::orphan()?);
        let engine = Arc::new_cyclic(|this| RemoteEngine {
            channel: Mutex::new(orphan),
            slot: FrameSlot::new(),
            position: AtomicU64::new(0),
            duration: AtomicU64::new(0),
            source: Mutex::new(None),
            child: Mutex::new(None),
            epoch: AtomicU64::new(0),
            emit,
            exporting: Arc::new(AtomicBool::new(false)),
            spent: AtomicBool::new(false),
            volume: Mutex::new(1.0),
            effects: Mutex::new(AudioEffects::default()),
            this: this.clone(),
            watch: Mutex::new(Watch::new()),
        });

        engine.restart()?;
        Self::watchdog(Arc::downgrade(&engine));
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

        // A new decoder starts at playbin's defaults; carry the settings over.
        self.request(Request::Volume(*self.volume.lock().unwrap()));
        self.request(Request::AudioEffects(*self.effects.lock().unwrap()));
        Ok(())
    }

    /// Checks twice a second whether playback has wedged. Holds only a weak
    /// reference, so it ends with the engine.
    fn watchdog(engine: Weak<Self>) {
        std::thread::Builder::new()
            .name("myvid-watchdog".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(500));
                let Some(engine) = engine.upgrade() else {
                    return;
                };
                if engine.stalled() {
                    engine.recover();
                }
            })
            .expect("watchdog thread");
    }

    fn stalled(&self) -> bool {
        let watch = self.watch.lock().unwrap();
        watch.playing
            && !watch.buffering
            && watch.progress.elapsed() >= STALL
            && watch
                .last_recovery
                .is_none_or(|at| at.elapsed() >= RECOVERY_COOLDOWN)
    }

    /// Something happened that may hold the position still for a moment.
    fn touch(&self, playing: Option<bool>) {
        let mut watch = self.watch.lock().unwrap();
        watch.progress = Instant::now();
        if let Some(playing) = playing {
            watch.playing = playing;
        }
    }

    /// Replace a wedged decoder with a fresh one, resuming where it froze.
    ///
    /// Only for local files. A network stream that stops moving is far more
    /// likely to be the network, and reconnecting would not help.
    fn recover(self: &Arc<Self>) {
        let Some(uri) = self.source.lock().unwrap().clone() else {
            return;
        };
        let Ok((path, _)) = ::gstreamer::glib::filename_from_uri(&uri) else {
            return;
        };
        let at = self.position.load(Ordering::Relaxed);
        {
            let mut watch = self.watch.lock().unwrap();
            watch.last_recovery = Some(Instant::now());
            watch.progress = Instant::now();
        }
        eprintln!(
            "myvid: playback stalled at {:.1}s; restarting the decoder",
            at as f64 / 1e9
        );

        if let Err(err) = self.restart() {
            (self.emit)(Event::Error(format!("could not restart the decoder: {err:#}")));
            return;
        }
        self.spent.store(true, Ordering::SeqCst);
        self.request(Request::PlayPath {
            path: path.to_string_lossy().into_owned(),
            start: at,
        });
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

                    // Notices still queued from a decoder we have replaced
                    // describe the previous file: its tracks, its position, its
                    // end. Delivering them would scribble over the new one.
                    if self.epoch.load(Ordering::SeqCst) != epoch {
                        return;
                    }

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
                            let mut watch = self.watch.lock().unwrap();
                            if watch.last_position != ns {
                                watch.last_position = ns;
                                watch.progress = Instant::now();
                            }
                        }
                        Notice::Duration(ns) => {
                            self.duration.store(ns, Ordering::Relaxed);
                            (self.emit)(Event::Duration(Duration::from_nanos(ns)));
                        }
                        Notice::State(state) => (self.emit)(Event::State(state)),
                        Notice::Buffering(p) => {
                            self.watch.lock().unwrap().buffering = p < 100;
                            (self.emit)(Event::Buffering(p))
                        }
                        Notice::Loaded(info) => (self.emit)(Event::Loaded(info)),
                        Notice::Tracks(tracks) => (self.emit)(Event::Tracks(tracks)),
                        Notice::Subtitle { text, start, end } => (self.emit)(Event::Subtitle {
                            text,
                            start: Duration::from_nanos(start),
                            end: Duration::from_nanos(end),
                        }),
                        Notice::Eos => {
                            self.touch(Some(false));
                            (self.emit)(Event::Eos)
                        }
                        Notice::Failed(message) => {
                            // An error is reported, not retried behind the
                            // person's back.
                            self.touch(Some(false));
                            (self.emit)(Event::Error(message))
                        }
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
        {
            // The decoder starts playing on its own once the file is open.
            // Finding the streams in a large file takes a moment, so allow for
            // that before counting a still position as a stall.
            let mut watch = self.watch.lock().unwrap();
            *watch = Watch::new();
            watch.playing = true;
            watch.progress = Instant::now() + Duration::from_secs(5);
        }

        // Whatever the previous decoder was given, it can never read this.
        if self.spent.swap(true, Ordering::SeqCst) {
            let this = self.this.upgrade().context("the engine is shutting down")?;
            this.restart()?;
        }

        match ::gstreamer::glib::filename_from_uri(uri) {
            // A local file: give it a decoder of its own, confined to it.
            Ok((path, _)) => {
                self.request(Request::PlayPath {
                    path: path.to_string_lossy().into_owned(),
                    start: 0,
                });
            }
            // A network source needs no file access at all.
            Err(_) => self.request(Request::PlayUri(uri.to_owned())),
        }

        Ok(())
    }

    fn play(&self) {
        self.touch(Some(true));
        self.request(Request::Resume);
    }

    fn pause(&self) {
        self.touch(Some(false));
        self.request(Request::Pause);
    }

    fn seek(&self, to: Duration) {
        self.touch(None);
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
        *self.volume.lock().unwrap() = volume;
        self.request(Request::Volume(volume));
    }

    fn set_audio_effects(&self, effects: AudioEffects) {
        *self.effects.lock().unwrap() = effects;
        self.request(Request::AudioEffects(effects));
    }

    fn set_rate(&self, rate: f64) {
        self.touch(None);
        self.request(Request::Rate(rate));
    }

    fn frames(&self) -> FrameSlot {
        self.slot.clone()
    }

    fn select_track(&self, kind: TrackKind, id: Option<&str>) {
        self.touch(None);
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

    /// Opening one file after another, as anyone working through a folder does.
    ///
    /// A decoder confines itself to the first file it is given and refuses any
    /// other, so the second open must get a fresh decoder — and that decoder
    /// must still be playing at the volume the player asked for.
    #[test]
    fn opening_a_second_file_gets_a_fresh_decoder() {
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

        // The same file twice as well as a different one: both need a new
        // decoder, since confinement is per process, not per path.
        let other = std::env::var_os("MYVID_TEST_MEDIA_2")
            .map(|m| to_uri(&m.to_string_lossy()).expect("uri"))
            .unwrap_or_else(|| uri.clone());

        engine.set_volume(0.5);
        for uri in [&uri, &other, &uri] {
            engine.open(uri).expect("open");
            engine.play();
            std::thread::sleep(Duration::from_secs(2));
            assert!(
                engine.position().is_some_and(|p| p > Duration::ZERO),
                "the file should be playing"
            );
        }

        let errors: Vec<String> = rx.try_iter().collect();
        assert!(errors.is_empty(), "reopening produced errors: {errors:#?}");
    }

    /// A decoder that stops making progress is replaced, and playback carries
    /// on from where it froze.
    ///
    /// The real wedge is rare and cannot be caused on demand, so this freezes
    /// the decode process outright with SIGSTOP, which looks the same from the
    /// player's side: no frames, no position, no reply to pause or play.
    /// Played at zero volume.
    #[test]
    fn a_frozen_decoder_is_replaced() {
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
        engine.set_volume(0.0);
        let uri = to_uri(&media.to_string_lossy()).expect("uri");
        engine.open(&uri).expect("open");
        engine.play();
        std::thread::sleep(Duration::from_secs(4));

        let frozen_at = engine.position().unwrap_or_default();
        let pid = engine.child.lock().unwrap().as_ref().expect("a decoder").id();
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(pid as i32).expect("pid"),
            rustix::process::Signal::STOP,
        )
        .expect("freeze the decoder");

        // Stall window, restart, preroll, and a little playback.
        std::thread::sleep(STALL + Duration::from_secs(5));

        let replaced = engine.child.lock().unwrap().as_ref().map(|c| c.id()) != Some(pid);
        let resumed_at = engine.position().unwrap_or_default();
        drop(engine);

        assert!(replaced, "the frozen decoder should have been replaced");
        assert!(
            resumed_at > frozen_at + Duration::from_secs(1),
            "playback should continue from {frozen_at:?}, got {resumed_at:?}"
        );
        // Resuming mid-file, not from the beginning.
        assert!(resumed_at < frozen_at + Duration::from_secs(10));
        let errors: Vec<String> = rx.try_iter().collect();
        assert!(errors.is_empty(), "recovery produced errors: {errors:#?}");
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
