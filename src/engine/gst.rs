//! GStreamer backend.
//!
//! `playbin3` does the hard parts — demuxing, decoder selection, hardware
//! decode, audio output and A/V sync — and we replace only its video sink with
//! an `appsink` that hands us NV12 frames. The audio sink owns the pipeline
//! clock, so synchronisation is GStreamer's problem, not ours.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

use super::frame::{FrameSlot, PlanarFrame};
use super::{
    ClipRequest, Container, Event, Export, MediaInfo, PlaybackEngine, State, Track, TrackKind,
};

/// Somewhere for the engine to post events without knowing what the UI is.
pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

/// Whether a decoder factory name belongs to a GPU decoder.
///
/// Matching a fixed list of names got this wrong: `vavp8dec` was reported as
/// software because the list happened not to mention VP8. Matching the vendor
/// prefixes instead covers every VA-API, NVIDIA, Direct3D, VideoToolbox and V4L2
/// stateless decoder, including ones that do not exist yet.
fn is_hardware_decoder(factory: &str) -> bool {
    let name = factory.to_lowercase();

    if !name.contains("dec") {
        return false;
    }

    // `va*dec` is the VA-API family; `vaapi*` the older one. Guard against
    // matching unrelated names that merely start with "va".
    let va = (name.starts_with("va") && name.ends_with("dec")) || name.starts_with("vaapi");

    va || name.starts_with("nv")
        || name.contains("d3d11")
        || name.contains("d3d12")
        || name.contains("videotoolbox")
        || name.starts_with("vtdec")
        || name.starts_with("v4l2sl")
        || name.contains("msdk")
        || name.contains("qsv")
        || name.contains("amfdec")
}

/// The stream ids currently selected, one per kind.
#[derive(Debug, Clone, Default)]
struct Selection {
    video: Option<String>,
    audio: Option<String>,
    text: Option<String>,
}

impl Selection {
    fn ids(&self) -> Vec<&str> {
        [self.video.as_deref(), self.audio.as_deref(), self.text.as_deref()]
            .into_iter()
            .flatten()
            .collect()
    }
}

pub struct GstEngine {
    playbin: gst::Element,
    slot: FrameSlot,
    emit: EventSink,
    reported_info: AtomicBool,
    selection: Mutex<Selection>,
    tracks: Mutex<Vec<Track>>,
    /// Kept so track labels can be rebuilt once language tags arrive.
    collection: Mutex<Option<gst::StreamCollection>>,
    /// Source of the current file, needed to export a clip from it.
    source: Mutex<Option<String>>,
    exporting: Arc<AtomicBool>,
    /// Running time of the most recent frame handed to the renderer, so the gap
    /// between picture and sound can be measured rather than guessed at.
    last_frame_ns: Arc<AtomicU64>,
    /// A flushing seek is in flight; the pipeline will refuse another.
    seeking: AtomicBool,
    /// Where to seek next once the current one lands.
    pending_seek: Mutex<Option<Duration>>,
}

impl GstEngine {
    pub fn new(emit: EventSink) -> Result<Arc<Self>> {
        gst::init().context("gst::init")?;

        let playbin = gst::ElementFactory::make("playbin3")
            .build()
            .or_else(|_| gst::ElementFactory::make("playbin").build())
            .context("neither playbin3 nor playbin is available — install gstreamer1.0-plugins-base")?;

        // Subtitles stay enabled, but `text-sink` diverts the cues to us instead
        // of playbin's overlay. That matters for more than layout: the overlay
        // burns text into the picture, and to keep it crisp it first scales the
        // video up to the display resolution — on a 2880x1800 panel that turns a
        // 1080p file into 2.5x the pixels, every frame, for nothing.
        playbin.set_property("text-sink", &build_text_sink(&emit)?);

        // Network sources otherwise buffer as much as they like. 16 MiB and
        // five seconds is generous for playback and bounded for memory.
        playbin.set_property("buffer-size", 16i32 * 1024 * 1024);
        playbin.set_property("buffer-duration", 5i64 * gst::ClockTime::SECOND.nseconds() as i64);

        let slot = FrameSlot::new();
        let last_frame_ns = Arc::new(AtomicU64::new(0));
        let sink = build_video_sink(&slot, &emit, last_frame_ns.clone())?;
        playbin.set_property("video-sink", &sink);
        playbin.set_property("audio-sink", &build_audio_sink()?);
        playbin.set_property("audio-filter", &build_audio_filter()?);

        let engine = Arc::new(Self {
            playbin,
            slot,
            emit,
            reported_info: AtomicBool::new(false),
            selection: Mutex::new(Selection::default()),
            tracks: Mutex::new(Vec::new()),
            collection: Mutex::new(None),
            source: Mutex::new(None),
            exporting: Arc::new(AtomicBool::new(false)),
            last_frame_ns,
            seeking: AtomicBool::new(false),
            pending_seek: Mutex::new(None),
        });

        engine.clone().watch_bus()?;
        Ok(engine)
    }

    /// Forward bus messages onto the event sink from a dedicated thread.
    fn watch_bus(self: Arc<Self>) -> Result<()> {
        let bus = self
            .playbin
            .bus()
            .ok_or_else(|| anyhow!("pipeline has no bus"))?;
        let pipeline = self.playbin.clone();

        std::thread::Builder::new()
            .name("myvid-gst-bus".into())
            .spawn(move || {
                use gst::MessageView;
                let mut info = MediaInfo::default();
                let mut selected_streams = false;

                for msg in bus.iter_timed(gst::ClockTime::NONE) {
                    match msg.view() {
                        MessageView::Eos(_) => {
                            (self.emit)(Event::State(State::Ended));
                            (self.emit)(Event::Eos);
                        }
                        MessageView::Error(err) => {
                            let source = err
                                .src()
                                .map(|s| s.path_string().to_string())
                                .unwrap_or_else(|| "pipeline".into());
                            let detail = err.debug().unwrap_or_default();
                            let message = format!(
                                "{}: {}{}",
                                source,
                                err.error(),
                                if detail.is_empty() {
                                    String::new()
                                } else {
                                    format!(" ({detail})")
                                }
                            );
                            // Always on stderr, not just under MYVID_DIAG: an
                            // error a user cannot copy out of the window is an
                            // error they cannot report.
                            eprintln!("myvid error: {message}");
                            (self.emit)(Event::Error(message));
                        }
                        MessageView::Warning(w) => {
                            eprintln!("gst warning: {}", w.error());
                        }
                        MessageView::Buffering(b) => {
                            let percent = b.percent().clamp(0, 100) as u8;
                            (self.emit)(Event::Buffering(percent));
                        }
                        MessageView::AsyncDone(_) => {
                            self.seek_settled();
                        }
                        MessageView::DurationChanged(_) => {
                            if let Some(d) = query_duration(&pipeline) {
                                (self.emit)(Event::Duration(d));
                            }
                        }
                        MessageView::Tag(tag) => {
                            let tags = tag.tags();
                            let mut changed = false;
                            if let Some(t) = tags.get::<gst::tags::Title>() {
                                info.title = t.get().to_string();
                                changed = true;
                            }
                            if let Some(t) = tags.get::<gst::tags::VideoCodec>() {
                                info.video_codec = t.get().to_string();
                                changed = true;
                            }
                            if let Some(t) = tags.get::<gst::tags::AudioCodec>() {
                                info.audio_codec = t.get().to_string();
                                changed = true;
                            }
                            if let Some(t) = tags.get::<gst::tags::ContainerFormat>() {
                                info.container = t.get().to_string();
                                changed = true;
                            }
                            if changed {
                                let mut merged = info.clone();
                                merged.hardware = uses_hardware_decoder(&pipeline);
                                (self.emit)(Event::Loaded(merged));
                            }
                            self.refresh_labels();
                        }
                        MessageView::StreamCollection(msg) => {
                            // playbin3 defaults to the first track of each kind.
                            // Prefer the audio stream with the most channels,
                            // breaking ties on bitrate.
                            if !selected_streams {
                                let collection = msg.stream_collection();
                                if self.adopt_collection(&collection, &mut info) {
                                    selected_streams = true;
                                    self.apply_selection();
                                    (self.emit)(Event::Loaded(info.clone()));
                                }
                            }
                        }
                        MessageView::StateChanged(sc) => {
                            // Only the pipeline's own transitions matter.
                            if sc.src().map(|s| s == &pipeline).unwrap_or(false) {
                                let state = match sc.current() {
                                    gst::State::Playing => State::Playing,
                                    gst::State::Paused => State::Paused,
                                    gst::State::Ready | gst::State::Null => State::Idle,
                                    _ => continue,
                                };
                                (self.emit)(Event::State(state));

                                if state == State::Playing {
                                    if std::env::var_os("MYVID_DIAG").is_some() {
                                        // Which element drives the pipeline
                                        // decides whether sound and picture can
                                        // drift apart at all.
                                        eprintln!(
                                            "[diag] clock: {}",
                                            pipeline
                                                .clock()
                                                .map(|c| c.name().to_string())
                                                .unwrap_or_else(|| "none".into())
                                        );
                                        let (video, audio) = decoders(&pipeline);
                                        eprintln!(
                                            "[diag] video: {} ({}) · audio: {}",
                                            video.unwrap_or_else(|| "?".into()),
                                            if uses_hardware_decoder(&pipeline) {
                                                "hardware"
                                            } else {
                                                "software"
                                            },
                                            audio.unwrap_or_else(|| "?".into()),
                                        );
                                    }
                                    if let Some(d) = query_duration(&pipeline) {
                                        (self.emit)(Event::Duration(d));
                                    }
                                    let mut merged = info.clone();
                                    merged.hardware = uses_hardware_decoder(&pipeline);
                                    (self.emit)(Event::Loaded(merged));
                                    self.refresh_labels();
                                }
                            }
                        }
                        _ => {}
                    }
                }
            })
            .context("spawning bus thread")?;

        Ok(())
    }

    /// Read a stream collection: describe every selectable track, and choose a
    /// sensible default for each kind — the best audio by channel count then
    /// bitrate, rather than whichever the container happened to list first.
    fn adopt_collection(
        self: &Arc<Self>,
        collection: &gst::StreamCollection,
        info: &mut MediaInfo,
    ) -> bool {
        let mut tracks = Vec::new();
        let mut selection = Selection::default();
        let mut best_audio: Option<(i32, u32)> = None;
        // Numbered per kind, so a file with no language tags still reads as
        // "Track 1 / Track 2" rather than two identical "Unknown" rows.
        let (mut audio_ordinal, mut text_ordinal) = (0usize, 0usize);

        for stream in collection.iter() {
            let Some(id) = stream.stream_id().map(|s| s.to_string()) else {
                continue;
            };
            let kind = stream.stream_type();

            if kind.contains(gst::StreamType::VIDEO) {
                selection.video.get_or_insert(id);
                continue;
            }

            let ordinal = if kind.contains(gst::StreamType::AUDIO) {
                audio_ordinal += 1;
                audio_ordinal
            } else {
                text_ordinal += 1;
                text_ordinal
            };
            let (label, detail) = describe(&stream, ordinal);

            if kind.contains(gst::StreamType::AUDIO) {
                let (channels, rate, bitrate) = audio_facts(&stream);
                let better = best_audio
                    .map(|(c, b)| (channels, bitrate) > (c, b))
                    .unwrap_or(true);
                if better {
                    best_audio = Some((channels, bitrate));
                    info.audio_channels = channels.max(0) as u32;
                    info.audio_rate = rate.max(0) as u32;
                    info.audio_bitrate = bitrate;
                    selection.audio = Some(id.clone());
                }
                tracks.push(Track {
                    id,
                    kind: TrackKind::Audio,
                    label,
                    detail,
                    selected: false,
                });
            } else if kind.contains(gst::StreamType::TEXT) {
                if selection.text.is_none() {
                    selection.text = Some(id.clone());
                }
                tracks.push(Track {
                    id,
                    kind: TrackKind::Text,
                    label,
                    detail,
                    selected: false,
                });
            }
        }

        if selection.video.is_none() && tracks.is_empty() {
            return false;
        }

        *self.selection.lock().unwrap() = selection;
        *self.tracks.lock().unwrap() = tracks;
        *self.collection.lock().unwrap() = Some(collection.clone());

        // A stream's tags can arrive after the collection does — whether they
        // have is a race, which is why the same file labelled its tracks
        // "Telugu" one run and "Track 1" the next. Watch each stream's `tags`
        // property instead of hoping they are already there.
        for stream in collection.iter() {
            let weak = Arc::downgrade(self);
            stream.connect_notify(Some("tags"), move |_, _| {
                if let Some(engine) = weak.upgrade() {
                    engine.refresh_labels();
                }
            });
        }

        self.publish_tracks();
        true
    }

    /// Rebuild track labels from the collection.
    ///
    /// Language tags are not always present when the collection first arrives —
    /// whether they are is a race — so the same file can label its tracks
    /// "Telugu" one run and "Track 1" the next. Re-describing when tags land
    /// settles it.
    fn refresh_labels(&self) {
        let Some(collection) = self.collection.lock().unwrap().clone() else {
            return;
        };

        let mut described = Vec::new();
        let (mut audio_ordinal, mut text_ordinal) = (0usize, 0usize);
        for stream in collection.iter() {
            let Some(id) = stream.stream_id().map(|s| s.to_string()) else {
                continue;
            };
            let kind = stream.stream_type();
            if kind.contains(gst::StreamType::VIDEO) {
                continue;
            }
            let ordinal = if kind.contains(gst::StreamType::AUDIO) {
                audio_ordinal += 1;
                audio_ordinal
            } else {
                text_ordinal += 1;
                text_ordinal
            };
            described.push((id, describe(&stream, ordinal)));
        }

        let mut changed = false;
        {
            let mut tracks = self.tracks.lock().unwrap();
            for track in tracks.iter_mut() {
                if let Some((_, (label, detail))) =
                    described.iter().find(|(id, _)| id == &track.id)
                {
                    if &track.label != label || &track.detail != detail {
                        track.label = label.clone();
                        track.detail = detail.clone();
                        changed = true;
                    }
                }
            }
        }

        if changed {
            self.publish_tracks();
        }
    }

    /// Send the current selection to the pipeline.
    fn apply_selection(&self) {
        let selection = self.selection.lock().unwrap().clone();
        let ids = selection.ids();
        if ids.is_empty() {
            return;
        }
        let _ = self
            .playbin
            .send_event(gst::event::SelectStreams::new(ids));
    }

    /// Mark which tracks are live and tell the UI.
    fn publish_tracks(&self) {
        let selection = self.selection.lock().unwrap().clone();
        let mut tracks = self.tracks.lock().unwrap().clone();
        for track in &mut tracks {
            let active = match track.kind {
                TrackKind::Audio => selection.audio.as_deref(),
                TrackKind::Text => selection.text.as_deref(),
            };
            track.selected = active == Some(track.id.as_str());
        }
        if std::env::var_os("MYVID_DIAG").is_some() {
            for track in &tracks {
                eprintln!(
                    "[diag] track {:?} {}{} — {}",
                    track.kind,
                    track.label,
                    if track.selected { " (selected)" } else { "" },
                    track.detail
                );
            }
        }

        (self.emit)(Event::Tracks(tracks));
    }

    /// Keep a seek inside the media.
    ///
    /// Seeking to exactly the duration - which dragging the bar to its end does,
    /// and which a file whose header overstates its length does at any position -
    /// puts the demuxer past the last byte. It reads nothing, hits end of
    /// stream, and reports `got eos and didn't receive a complete header
    /// object`, killing playback. A margin short of the end costs nothing: no
    /// one is trying to land on the final frame.
    fn clamp_target(&self, to: Duration) -> Duration {
        const MARGIN: Duration = Duration::from_millis(2000);

        match query_duration(&self.playbin) {
            Some(total) if total > MARGIN => to.min(total - MARGIN),
            Some(total) => to.min(total),
            None => to,
        }
    }

    fn issue_seek(&self, to: Duration) {
        self.seeking.store(true, Ordering::Release);
        let target = gst::ClockTime::from_nseconds(to.as_nanos() as u64);

        // SNAP_BEFORE matters as much as the clamp. KEY_UNIT alone snaps to the
        // *nearest* keyframe, which near the end of a file means snapping
        // forward past the last byte - the demuxer then reads nothing and
        // reports a missing header. Snapping backwards always lands on data,
        // and landing a fraction early is what every player does anyway.
        if self
            .playbin
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT | gst::SeekFlags::SNAP_BEFORE,
                target,
            )
            .is_err()
        {
            self.seeking.store(false, Ordering::Release);
            // Not surfaced: a refused seek is not something the user did, and
            // the next one along will land.
            eprintln!("myvid: pipeline refused a seek to {:.2}s", to.as_secs_f64());
        }
    }

    /// Called when the pipeline reports a seek has completed.
    /// Presentation time of the newest frame, for measuring A/V drift.
    pub fn last_frame(&self) -> Duration {
        Duration::from_nanos(self.last_frame_ns.load(Ordering::Relaxed))
    }

    fn seek_settled(&self) {
        self.seeking.store(false, Ordering::Release);
        let next = self.pending_seek.lock().unwrap().take();
        if let Some(next) = next {
            self.issue_seek(next);
        }
    }

    fn set_state(&self, state: gst::State) {
        if let Err(err) = self.playbin.set_state(state) {
            (self.emit)(Event::Error(format!("could not enter {state:?}: {err}")));
        }
    }
}

impl PlaybackEngine for GstEngine {
    fn open(&self, uri: &str) -> Result<()> {
        self.playbin
            .set_state(gst::State::Null)
            .context("resetting pipeline")?;
        self.slot.clear();
        self.reported_info.store(false, Ordering::Relaxed);
        self.seeking.store(false, Ordering::Release);
        *self.pending_seek.lock().unwrap() = None;
        *self.source.lock().unwrap() = Some(uri.to_owned());
        self.tracks.lock().unwrap().clear();
        *self.collection.lock().unwrap() = None;
        *self.selection.lock().unwrap() = Selection::default();
        self.playbin.set_property("uri", uri);
        self.playbin
            .set_state(gst::State::Paused)
            .context("prerolling")?;
        Ok(())
    }

    fn play(&self) {
        self.set_state(gst::State::Playing);
    }

    fn pause(&self) {
        self.set_state(gst::State::Paused);
    }

    /// Seek, coalescing anything that arrives while one is already running.
    ///
    /// Dragging the scrub bar produces a seek per pixel of movement. A flushing
    /// seek takes time to resolve and the pipeline refuses a second one until it
    /// has, so firing them all made most of them fail — reported to the user as
    /// "Failed to seek" for doing nothing but dragging. Only one seek is ever in
    /// flight; the newest target supersedes any waiting one, because an
    /// intermediate position during a drag is of no interest.
    fn seek(&self, to: Duration) {
        let to = self.clamp_target(to);
        if self.seeking.load(Ordering::Acquire) {
            *self.pending_seek.lock().unwrap() = Some(to);
            return;
        }
        self.issue_seek(to);
    }

    fn position(&self) -> Option<Duration> {
        self.playbin
            .query_position::<gst::ClockTime>()
            .map(|t| Duration::from_nanos(t.nseconds()))
    }

    fn duration(&self) -> Option<Duration> {
        query_duration(&self.playbin)
    }

    fn set_volume(&self, volume: f64) {
        self.playbin.set_property("volume", volume.clamp(0.0, 1.0));
    }

    fn set_rate(&self, rate: f64) {
        let position = self
            .playbin
            .query_position::<gst::ClockTime>()
            .unwrap_or(gst::ClockTime::ZERO);

        let event = if rate > 0.0 {
            gst::event::Seek::new(
                rate,
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::SeekType::Set,
                position,
                gst::SeekType::End,
                gst::ClockTime::ZERO,
            )
        } else {
            return;
        };

        if !self.playbin.send_event(event) {
            (self.emit)(Event::Error("playback rate change rejected".into()));
        }
    }

    fn frames(&self) -> FrameSlot {
        self.slot.clone()
    }

    fn select_track(&self, kind: TrackKind, id: Option<&str>) {
        {
            let mut selection = self.selection.lock().unwrap();
            let slot = match kind {
                TrackKind::Audio => &mut selection.audio,
                TrackKind::Text => &mut selection.text,
            };
            *slot = id.map(str::to_owned);
        }
        self.apply_selection();
        self.publish_tracks();

        // Turning subtitles off leaves the last cue on screen otherwise.
        if kind == TrackKind::Text && id.is_none() {
            (self.emit)(Event::Subtitle {
                text: None,
                start: Duration::ZERO,
                end: Duration::ZERO,
            });
        }
    }

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
                let result = run_export(&uri, &request, &emit);
                busy.store(false, Ordering::SeqCst);
                match result {
                    Ok(()) => (emit)(Event::Export(Export::Done(request.output))),
                    Err(err) => (emit)(Event::Export(Export::Failed(format!("{err:#}")))),
                }
            })
            .ok();
    }
}

impl Drop for GstEngine {
    fn drop(&mut self) {
        let _ = self.playbin.set_state(gst::State::Null);
    }
}

/// `videoconvert ! appsink(NV12)` wrapped in a bin that playbin can use as its
/// video sink. Hardware decoders already emit NV12, so the converter is usually
/// a passthrough.
fn build_video_sink(
    slot: &FrameSlot,
    emit: &EventSink,
    last_frame_ns: Arc<AtomicU64>,
) -> Result<gst::Element> {
    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .context("videoconvert missing — install gstreamer1.0-plugins-base")?;

    let caps = gst::Caps::builder("video/x-raw")
        .field("format", gst_video::VideoFormat::Nv12.to_str())
        .build();

    let appsink = gst_app::AppSink::builder()
        .caps(&caps)
        .max_buffers(2)
        .drop(true)
        .sync(true)
        .build();

    let slot = slot.clone();
    let emit = emit.clone();

    // MYVID_DIAG=1 reports decode throughput once a second. The only way to
    // tell "no frames" apart from "frames, but nothing on screen".
    let diag = std::env::var_os("MYVID_DIAG").is_some();
    let frames = AtomicU64::new(0);
    let mut window = std::time::Instant::now();

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                let info =
                    gst_video::VideoInfo::from_caps(caps).map_err(|_| gst::FlowError::Error)?;
                let buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;

                // Keep the mapping alive in the slot rather than copying the
                // pixels out. The GPU upload reads straight from decoder memory.
                let frame = gst_video::VideoFrame::from_buffer_readable(buffer, &info)
                    .map_err(|_| gst::FlowError::Error)?;

                let (width, height) = (info.width(), info.height());
                let strides = info.stride();

                // What the pipeline thinks this frame's moment is. Against the
                // playback position, which the audio sink drives, this is the
                // real A/V offset — measured rather than assumed.
                if let Some(pts) = sample.buffer().and_then(|b| b.pts()) {
                    last_frame_ns.store(pts.nseconds(), Ordering::Relaxed);
                }

                slot.write(|f| f.set(Box::new(GstFrame::new(frame, &info))));

                emit(Event::Frame(width, height));

                if diag {
                    let n = frames.fetch_add(1, Ordering::Relaxed) + 1;
                    if window.elapsed() >= std::time::Duration::from_secs(1) {
                        eprintln!(
                            "[diag] {n} frames · {}x{} · {} · strides {:?}",
                            width,
                            height,
                            info.format().to_str(),
                            &strides[..2.min(strides.len())],
                        );
                        window = std::time::Instant::now();
                    }
                }

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let bin = gst::Bin::new();
    let sink_element = appsink.upcast_ref::<gst::Element>();
    bin.add_many([&convert, sink_element])
        .context("assembling video sink")?;
    gst::Element::link_many([&convert, sink_element]).context("linking video sink")?;

    let pad = convert
        .static_pad("sink")
        .ok_or_else(|| anyhow!("videoconvert has no sink pad"))?;
    let ghost = gst::GhostPad::with_target(&pad).context("ghosting sink pad")?;
    bin.add_pad(&ghost).context("adding ghost pad")?;

    Ok(bin.upcast())
}

/// A mapped GStreamer frame, borrowed straight into the GPU upload.
///
/// Geometry is captured once at construction: it comes from the caps, not the
/// mapping, and re-deriving it per plane per frame would be pure overhead.
struct GstFrame {
    frame: gst_video::VideoFrame<gst_video::video_frame::Readable>,
    width: u32,
    height: u32,
    strides: Vec<u32>,
}

impl GstFrame {
    fn new(
        frame: gst_video::VideoFrame<gst_video::video_frame::Readable>,
        info: &gst_video::VideoInfo,
    ) -> Self {
        Self {
            frame,
            width: info.width(),
            height: info.height(),
            strides: info.stride().iter().map(|s| (*s).max(0) as u32).collect(),
        }
    }
}

impl PlanarFrame for GstFrame {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn plane(&self, index: usize) -> Option<(&[u8], u32)> {
        let data = self.frame.plane_data(index as u32).ok()?;
        Some((data, *self.strides.get(index)?))
    }
}

/// Receives subtitle cues as text so the UI can draw them.
///
/// `sync=true` means a buffer arrives when it should appear on screen, so the
/// cue needs no scheduling of its own — only a duration telling us when to clear
/// it again.
fn build_text_sink(emit: &EventSink) -> Result<gst::Element> {
    // Deliberately no caps filter. Constraining this to `text/x-raw` means
    // playsink cannot connect a bitmap subtitle track (PGS, VobSub) at all, and
    // it fails the whole file with a GstPlaySink error rather than simply not
    // showing subtitles. Accept anything, then ignore what we cannot draw.
    let appsink = gst_app::AppSink::builder()
        .max_buffers(4)
        .drop(true)
        .sync(true)
        .build();

    let emit = emit.clone();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                // Only text subtitles can be drawn today; bitmap formats arrive
                // here too and are skipped rather than rendered as mojibake.
                let is_text = sample
                    .caps()
                    .and_then(|caps| caps.structure(0).map(|s| s.name().starts_with("text/")))
                    .unwrap_or(false);
                if !is_text {
                    return Ok(gst::FlowSuccess::Ok);
                }

                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

                let raw = String::from_utf8_lossy(map.as_slice());
                let text = clean_cue(&raw);

                // Express the cue in media time so pausing and speed changes
                // both behave. A cue without a duration lingers until the next
                // one replaces it; five seconds caps a stuck line.
                let start = buffer
                    .pts()
                    .map(|t| Duration::from_nanos(t.nseconds()))
                    .unwrap_or_default();
                let duration = buffer
                    .duration()
                    .map(|d| Duration::from_nanos(d.nseconds()))
                    .unwrap_or(Duration::from_secs(5));

                if std::env::var_os("MYVID_DIAG").is_some() {
                    eprintln!(
                        "[diag] cue {:.2}s +{:.2}s: {}",
                        start.as_secs_f64(),
                        duration.as_secs_f64(),
                        text.replace('\n', " / ")
                    );
                }

                emit(Event::Subtitle {
                    text: (!text.is_empty()).then_some(text),
                    start,
                    end: start + duration,
                });

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    Ok(appsink.upcast())
}

/// Strip the markup that comes with subtitle formats and leave readable text.
///
/// Covers the three things that actually show up: ASS/SSA override blocks in
/// braces, the comma-separated ASS event fields, and the pango/HTML-ish tags
/// SubRip is allowed to carry.
fn clean_cue(raw: &str) -> String {
    let mut text = raw.trim().to_owned();

    // ASS event lines: nine comma-separated fields, then the text.
    if text.starts_with("Dialogue:") || text.matches(',').count() >= 9 {
        if let Some((_, tail)) = text.match_indices(',').nth(8).map(|(i, _)| text.split_at(i + 1))
        {
            text = tail.to_owned();
        }
    }

    let mut out = String::with_capacity(text.len());
    let mut depth_brace = 0usize;
    let mut depth_angle = 0usize;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '{' => depth_brace += 1,
            '}' => depth_brace = depth_brace.saturating_sub(1),
            '<' => depth_angle += 1,
            '>' => depth_angle = depth_angle.saturating_sub(1),
            '\\' if depth_brace == 0 => {
                // ASS line breaks: \N is hard, \n is soft.
                match chars.peek() {
                    Some('N') | Some('n') => {
                        let _ = chars.next();
                        out.push('\n');
                    }
                    _ => out.push(c),
                }
            }
            _ if depth_brace == 0 && depth_angle == 0 => out.push(c),
            _ => {}
        }
    }

    out.trim().to_owned()
}

/// The audio sink, chosen for its clock before anything else.
///
/// This is where "best quality" and "stays in sync" pull against each other, and
/// sync has to win. `pipewiresink` talks to PipeWire directly and skips the Pulse
/// compatibility layer — but it provides no clock, so the pipeline runs on the
/// system clock while the sound card consumes samples at its own crystal rate.
/// Those two oscillators differ by tens of parts per million: inaudible for a
/// second, a visible lip-sync error after ten minutes. That was this player's
/// drift, and it came of choosing a sink for throughput rather than timing.
///
/// `pulsesink` provides `GstPulseSinkClock`, and on any modern desktop it is
/// answered by `pipewire-pulse` anyway — so the audio still reaches PipeWire, and
/// the pipeline is driven by the device actually playing it.
fn build_audio_sink() -> Result<gst::Element> {
    for name in ["pulsesink", "alsasink", "autoaudiosink", "pipewiresink"] {
        let Ok(sink) = gst::ElementFactory::make(name).build() else {
            continue;
        };
        if sink
            .element_flags()
            .contains(gst::ElementFlags::PROVIDE_CLOCK)
        {
            return Ok(sink);
        }
    }

    // Nothing here provides a clock. Play anyway, and say why sync may wander.
    let sink = gst::ElementFactory::make("autoaudiosink")
        .build()
        .context("no usable audio sink — install gstreamer1.0-pulseaudio")?;
    eprintln!("myvid: no audio sink provides a clock; A/V sync may drift");
    Ok(sink)
}

/// Conversion and resampling, inserted by playbin *before* the sink.
///
/// This lives in `audio-filter` rather than in a bin wrapped around the sink,
/// because a bin around the sink hides the sink's clock from the pipeline — the
/// other half of the same bug. The quality of the chain is unchanged: everything
/// stays in 32-bit float so nothing quantises until the sink makes the single
/// conversion to the device format, resampling uses the best kernel GStreamer
/// has rather than the default, and TPDF dither is applied on the way down.
fn build_audio_filter() -> Result<gst::Element> {
    let convert = gst::ElementFactory::make("audioconvert")
        .property_from_str("dithering", "tpdf")
        .build()
        .context("audioconvert missing — install gstreamer1.0-plugins-base")?;

    let resample = gst::ElementFactory::make("audioresample")
        .property("quality", 10i32)
        .build()
        .context("audioresample missing — install gstreamer1.0-plugins-base")?;

    let float = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .build(),
        )
        .build()
        .context("capsfilter missing")?;

    let bin = gst::Bin::new();
    bin.add_many([&convert, &resample, &float])
        .context("assembling the audio filter")?;
    gst::Element::link_many([&convert, &resample, &float]).context("linking the audio filter")?;

    let sink_pad = convert
        .static_pad("sink")
        .ok_or_else(|| anyhow!("audioconvert has no sink pad"))?;
    let src_pad = float
        .static_pad("src")
        .ok_or_else(|| anyhow!("capsfilter has no src pad"))?;

    bin.add_pad(&gst::GhostPad::with_target(&sink_pad).context("ghosting the filter input")?)
        .context("adding the filter input")?;
    bin.add_pad(&gst::GhostPad::with_target(&src_pad).context("ghosting the filter output")?)
        .context("adding the filter output")?;

    Ok(bin.upcast())
}

/// Human labels for a stream: what language it is, and what it actually is.
fn describe(stream: &gst::Stream, ordinal: usize) -> (String, String) {
    let tags = stream.tags();

    let label = tags
        .as_ref()
        .and_then(|t| {
            t.get::<gst::tags::LanguageName>()
                .map(|v| v.get().to_string())
                .or_else(|| t.get::<gst::tags::LanguageCode>().map(|v| v.get().to_string()))
                .or_else(|| t.get::<gst::tags::Title>().map(|v| v.get().to_string()))
        })
        .filter(|s| !s.is_empty())
        .map(|code| language_name(&code))
        .unwrap_or_else(|| format!("Track {ordinal}"));

    let caps = stream.caps();
    let structure = caps.as_ref().and_then(|c| c.structure(0));
    let mut detail = structure.map(|s| codec_name(s.name().as_str())).unwrap_or_default();

    if let Some(structure) = structure {
        if let Ok(channels) = structure.get::<i32>("channels") {
            detail.push(' ');
            detail.push_str(&channel_layout(channels));
        }
        if let Ok(rate) = structure.get::<i32>("rate") {
            detail.push_str(&format!(" · {} kHz", rate / 1000));
        }
    }

    if let Some(bitrate) = tags.as_ref().and_then(|t| {
        t.get::<gst::tags::Bitrate>()
            .map(|v| v.get())
            .or_else(|| t.get::<gst::tags::NominalBitrate>().map(|v| v.get()))
    }) {
        if bitrate >= 1000 {
            detail.push_str(&format!(" · {} kb/s", bitrate / 1000));
        }
    }

    (label, detail.trim().to_owned())
}

/// ISO 639 codes are not readable. GStreamer's own lookup lives in libgsttag,
/// which has no Rust binding, so this covers the languages that actually turn up
/// in media files and passes anything else through untouched — a code is a
/// worse label than a name, but a better one than a wrong name.
fn language_name(code: &str) -> String {
    // Already a name rather than a code (some files tag it that way).
    if code.chars().count() > 3 {
        return code.to_owned();
    }

    match code.to_lowercase().as_str() {
        "en" | "eng" => "English",
        "hi" | "hin" => "Hindi",
        "te" | "tel" => "Telugu",
        "ta" | "tam" => "Tamil",
        "kn" | "kan" => "Kannada",
        "ml" | "mal" => "Malayalam",
        "mr" | "mar" => "Marathi",
        "bn" | "ben" => "Bengali",
        "gu" | "guj" => "Gujarati",
        "pa" | "pan" => "Punjabi",
        "ur" | "urd" => "Urdu",
        "es" | "spa" => "Spanish",
        "fr" | "fra" | "fre" => "French",
        "de" | "deu" | "ger" => "German",
        "it" | "ita" => "Italian",
        "pt" | "por" => "Portuguese",
        "ru" | "rus" => "Russian",
        "ja" | "jpn" => "Japanese",
        "ko" | "kor" => "Korean",
        "zh" | "zho" | "chi" => "Chinese",
        "ar" | "ara" => "Arabic",
        "nl" | "nld" | "dut" => "Dutch",
        "sv" | "swe" => "Swedish",
        "no" | "nor" => "Norwegian",
        "da" | "dan" => "Danish",
        "fi" | "fin" => "Finnish",
        "pl" | "pol" => "Polish",
        "tr" | "tur" => "Turkish",
        "th" | "tha" => "Thai",
        "vi" | "vie" => "Vietnamese",
        "id" | "ind" => "Indonesian",
        "he" | "heb" => "Hebrew",
        "el" | "ell" | "gre" => "Greek",
        "cs" | "ces" | "cze" => "Czech",
        "hu" | "hun" => "Hungarian",
        "ro" | "ron" | "rum" => "Romanian",
        "uk" | "ukr" => "Ukrainian",
        "fa" | "fas" | "per" => "Persian",
        "und" => "Undetermined",
        _ => return code.to_owned(),
    }
    .to_owned()
}

/// `audio/x-eac3` is not what anyone calls it.
fn codec_name(media: &str) -> String {
    let short = media
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .unwrap_or(media)
        .trim_start_matches("x-");

    // `text/x-raw` and `audio/x-raw` both reduce to "raw", and calling a
    // subtitle track "PCM" is worse than saying nothing.
    if media.starts_with("text/") || media.starts_with("subtitle/") {
        return match short {
            "raw" => "Text".to_owned(),
            other => codec_label(other),
        };
    }

    codec_label(short)
}

fn codec_label(short: &str) -> String {
    match short {
        "eac3" => "E-AC-3",
        "ac3" => "AC-3",
        "aac" | "mpeg" => "AAC",
        "dts" => "DTS",
        "opus" => "Opus",
        "vorbis" => "Vorbis",
        "flac" => "FLAC",
        "h264" => "H.264",
        "h265" => "HEVC",
        "av1" => "AV1",
        "vp9" => "VP9",
        "raw" => "PCM",
        "subrip" | "srt" => "SubRip",
        "ssa" | "ass" => "ASS",
        "pgs" => "PGS",
        other => other,
    }
    .to_owned()
}

fn channel_layout(channels: i32) -> String {
    match channels {
        1 => "mono".into(),
        2 => "stereo".into(),
        6 => "5.1".into(),
        8 => "7.1".into(),
        n => format!("{n}ch"),
    }
}

fn audio_facts(stream: &gst::Stream) -> (i32, i32, u32) {
    let caps = stream.caps();
    let structure = caps.as_ref().and_then(|c| c.structure(0));
    let channels = structure
        .and_then(|s| s.get::<i32>("channels").ok())
        .unwrap_or(0);
    let rate = structure.and_then(|s| s.get::<i32>("rate").ok()).unwrap_or(0);
    let bitrate = stream
        .tags()
        .and_then(|t| {
            t.get::<gst::tags::Bitrate>()
                .map(|v| v.get())
                .or_else(|| t.get::<gst::tags::NominalBitrate>().map(|v| v.get()))
        })
        .unwrap_or(0);
    (channels, rate, bitrate)
}

/// Copy a range of a file into a new container without re-encoding.
///
/// This shells out to `ffmpeg` rather than building a GStreamer pipeline, and
/// that is a deliberate retreat. A lossless cut needs a flushing seek, and a
/// flushing seek cannot pass through a muxer: `collectpads` refuses to forward
/// it ("forwarding flush start failed") and the muxer never recovers. Doing it
/// properly in GStreamer means re-timestamping every buffer after the seek,
/// which is what GStreamer Editing Services exists for — a library, not a
/// function. `ffmpeg -c copy` does this correctly in one command, keeps every
/// track including subtitles, and leaves the playback pipeline untouched.
///
/// The cost is a runtime dependency on the `ffmpeg` binary. `install.sh` pulls
/// it in; if it is missing, say so plainly rather than failing obscurely.
pub(crate) fn run_export(uri: &str, request: &ClipRequest, emit: &EventSink) -> Result<()> {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let path = gst::glib::filename_from_uri(uri)
        .map(|(path, _)| path)
        .map_err(|_| anyhow!("clip export needs a local file, not {uri}"))?;

    let span = request.end.saturating_sub(request.start);
    if span.is_zero() {
        anyhow::bail!("the clip has no length");
    }

    let mut command = Command::new("ffmpeg");
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        // Seeking before -i is the fast path: ffmpeg jumps to the keyframe at
        // or before this point rather than decoding its way there.
        .arg("-ss")
        .arg(format!("{:.3}", request.start.as_secs_f64()))
        .arg("-i")
        .arg(&path)
        .arg("-t")
        .arg(format!("{:.3}", span.as_secs_f64()))
        .arg("-c")
        .arg("copy");

    match request.container {
        // Matroska carries anything the source had.
        Container::Matroska => {
            command.arg("-map").arg("0");
        }
        // MP4 has no home for SubRip, so take only the picture and sound
        // rather than failing the whole export over a subtitle track.
        Container::Mp4 => {
            command
                .arg("-map")
                .arg("0:v?")
                .arg("-map")
                .arg("0:a?")
                .arg("-movflags")
                .arg("+faststart");
        }
    }

    command
        .arg("-avoid_negative_ts")
        .arg("make_zero")
        .arg("-progress")
        .arg("pipe:1")
        .arg("-nostdin")
        .arg(&request.output)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            anyhow!("ffmpeg is not installed — run: sudo apt install ffmpeg")
        } else {
            anyhow!("could not start ffmpeg: {err}")
        }
    })?;

    // `-progress pipe:1` emits `key=value` lines; `out_time_us` is how far
    // through the clip the copy has got.
    if let Some(stdout) = child.stdout.take() {
        let total = span.as_secs_f32();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(value) = line.strip_prefix("out_time_us=") {
                if let Ok(micros) = value.trim().parse::<i64>() {
                    let done = micros.max(0) as f32 / 1_000_000.0;
                    emit(Event::Export(Export::Progress(
                        (done / total).clamp(0.0, 1.0),
                    )));
                }
            }
        }
    }

    let output = child.wait_with_output().context("waiting for ffmpeg")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.lines().last().unwrap_or("ffmpeg failed").trim();
        anyhow::bail!("{detail}");
    }

    emit(Event::Export(Export::Progress(1.0)));
    Ok(())
}

fn query_duration(pipeline: &gst::Element) -> Option<Duration> {
    pipeline
        .query_duration::<gst::ClockTime>()
        .map(|t| Duration::from_nanos(t.nseconds()))
        .filter(|d| !d.is_zero())
}

/// The elements actually decoding video and audio, for diagnostics. Identified
/// by what their source pad carries rather than by name, since `avdec_eac3` and
/// `vah264dec` both merely contain "dec".
fn decoders(pipeline: &gst::Element) -> (Option<String>, Option<String>) {
    let (mut video, mut audio) = (None, None);

    let Ok(bin) = pipeline.clone().downcast::<gst::Bin>() else {
        return (video, audio);
    };

    let mut iter = bin.iterate_recurse();
    loop {
        match iter.next() {
            Ok(Some(element)) => {
                let Some(name) = element.factory().map(|f| f.name().to_string()) else {
                    continue;
                };
                if !name.contains("dec") || name.contains("decodebin") {
                    continue;
                }
                let media = element.static_pad("src").and_then(|pad| {
                    pad.current_caps()
                        .or_else(|| Some(pad.query_caps(None)))
                        .and_then(|caps| caps.structure(0).map(|s| s.name().to_string()))
                });

                match media.as_deref() {
                    Some(m) if m.starts_with("video/") => video.get_or_insert(name),
                    Some(m) if m.starts_with("audio/") => audio.get_or_insert(name),
                    _ => continue,
                };
            }
            Ok(None) => return (video, audio),
            Err(gst::IteratorError::Resync) => iter.resync(),
            Err(_) => return (video, audio),
        }
    }
}

/// Walk the pipeline looking for a decoder that runs on the GPU.
fn uses_hardware_decoder(pipeline: &gst::Element) -> bool {
    let Ok(bin) = pipeline.clone().downcast::<gst::Bin>() else {
        return false;
    };

    let mut iter = bin.iterate_recurse();
    loop {
        match iter.next() {
            Ok(Some(element)) => {
                let name = element
                    .factory()
                    .map(|f| f.name().to_string())
                    .unwrap_or_default();
                if is_hardware_decoder(&name) {
                    return true;
                }
            }
            Ok(None) => return false,
            Err(gst::IteratorError::Resync) => iter.resync(),
            Err(_) => return false,
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end check of the stream-copy export, against whatever media
    /// `MYVID_TEST_MEDIA` points at. Skips when it is unset, so the suite still
    /// passes on a machine without a sample file.
    #[test]
    fn exports_a_clip_without_reencoding() {
        let Some(media) = std::env::var_os("MYVID_TEST_MEDIA") else {
            eprintln!("skipped: set MYVID_TEST_MEDIA to a local video file");
            return;
        };

        gst::init().expect("gst init");
        let uri = crate::engine::to_uri(&media.to_string_lossy()).expect("uri");
        let output = std::env::temp_dir().join("myvid-clip-test.mkv");
        let _ = std::fs::remove_file(&output);

        let emit: EventSink = Arc::new(|event| {
            if let Event::Export(state) = event {
                eprintln!("  export: {state:?}");
            }
        });
        let request = ClipRequest {
            start: Duration::from_secs(5),
            end: Duration::from_secs(13),
            output: output.clone(),
            container: crate::engine::Container::Matroska,
        };

        run_export(&uri, &request, &emit).expect("export should succeed");

        let size = std::fs::metadata(&output).expect("clip exists").len();
        // Eight seconds of 1080p is megabytes; anything tiny means a header was
        // written and nothing else.
        assert!(size > 200_000, "clip is only {size} bytes");

        // The clip must actually be eight seconds long, and must still carry a
        // video stream — a copy that silently dropped one would still be "big".
        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=duration"])
            .args(["-show_entries", "stream=codec_type", "-of", "csv=p=0"])
            .arg(&output)
            .output()
            .expect("ffprobe");
        let report = String::from_utf8_lossy(&probe.stdout);
        assert!(report.contains("video"), "clip has no video stream: {report}");

        let duration: f64 = report
            .lines()
            .filter_map(|l| l.trim().parse::<f64>().ok())
            .next_back()
            .expect("clip has a duration");

        // A stream copy starts at the keyframe at or before the in point, so the
        // clip covers the requested range and usually a little more. How much
        // more depends on the source's keyframe interval, which for a web
        // encode can be ten seconds or so — but it must never be the whole file.
        assert!(
            (8.0..=45.0).contains(&duration),
            "clip should cover 8s and overshoot only to a keyframe, got {duration:.2}s"
        );

        let _ = std::fs::remove_file(&output);
    }

    use super::clean_cue;

    #[test]
    fn strips_html_style_tags() {
        assert_eq!(clean_cue("<i>Hello</i> there"), "Hello there");
    }

    #[test]
    fn strips_ass_override_blocks() {
        assert_eq!(clean_cue("{\\an8}{\\b1}Top line"), "Top line");
    }

    #[test]
    fn keeps_plain_text_untouched() {
        assert_eq!(clean_cue("  Just a line.  "), "Just a line.");
    }

    #[test]
    fn turns_ass_breaks_into_newlines() {
        assert_eq!(clean_cue("first\\Nsecond"), "first\nsecond");
    }

    #[test]
    fn drops_the_nine_ass_event_fields() {
        let line = "Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Actual text";
        assert_eq!(clean_cue(line), "Actual text");
    }
}

#[cfg(test)]
mod language_tests {
    use super::language_name;

    #[test]
    fn maps_two_and_three_letter_codes() {
        assert_eq!(language_name("te"), "Telugu");
        assert_eq!(language_name("tel"), "Telugu");
        assert_eq!(language_name("ENG"), "English");
    }

    #[test]
    fn passes_through_what_it_does_not_know() {
        assert_eq!(language_name("zzz"), "zzz");
    }

    #[test]
    fn leaves_names_alone() {
        assert_eq!(language_name("Brazilian Portuguese"), "Brazilian Portuguese");
    }
}

#[cfg(test)]
mod hardware_tests {
    use super::is_hardware_decoder;

    #[test]
    fn recognises_the_va_api_family() {
        for name in ["vah264dec", "vah265dec", "vavp8dec", "vavp9dec", "vaav1dec"] {
            assert!(is_hardware_decoder(name), "{name} should count as hardware");
        }
        assert!(is_hardware_decoder("vaapih264dec"));
    }

    #[test]
    fn rejects_software_decoders() {
        for name in ["avdec_h264", "avdec_mpeg4", "avdec_prores", "vp8dec", "openh264dec"] {
            assert!(!is_hardware_decoder(name), "{name} should count as software");
        }
    }

    #[test]
    fn ignores_elements_that_are_not_decoders() {
        assert!(!is_hardware_decoder("videoconvert"));
        assert!(!is_hardware_decoder("vaapipostproc"));
    }
}

#[cfg(test)]
mod reopen_tests {
    use super::*;
    use std::sync::mpsc;

    /// Does the source tell downstream how big the stream is?
    ///
    /// A demuxer that does not know where the file ends cannot clamp a seek to
    /// it. This compares the two sources directly, with no pipeline around them
    /// to muddy the answer.
    #[test]
    fn a_passed_descriptor_reports_its_size() {
        let Some(media) = std::env::var_os("MYVID_TEST_MEDIA") else {
            eprintln!("skipped: set MYVID_TEST_MEDIA");
            return;
        };
        gst::init().expect("gst init");

        let on_disk = std::fs::metadata(&media).expect("stat").len();
        let file = std::fs::File::open(&media).expect("open");

        let filesrc = gst::ElementFactory::make("filesrc")
            .property("location", media.to_string_lossy().as_ref())
            .build()
            .expect("filesrc");
        let fdsrc = gst::ElementFactory::make("fdsrc")
            .property("fd", std::os::fd::AsRawFd::as_raw_fd(&file))
            .build()
            .expect("fdsrc");

        let mut sizes = Vec::new();
        for (name, src) in [("filesrc", &filesrc), ("fdsrc", &fdsrc)] {
            let pipeline = gst::Pipeline::new();
            let sink = gst::ElementFactory::make("fakesink").build().expect("fakesink");
            pipeline.add_many([src, &sink]).expect("add");
            src.link(&sink).expect("link");
            let _ = pipeline.set_state(gst::State::Paused);
            let _ = pipeline.state(gst::ClockTime::from_seconds(5));

            let reported = src.query_duration::<gst::format::Bytes>().map(|b| *b);
            eprintln!("[probe] {name}: reports {reported:?}, file is {on_disk} bytes");
            sizes.push((name, reported));
            let _ = pipeline.set_state(gst::State::Null);
        }

        for (name, reported) in sizes {
            assert_eq!(
                reported,
                Some(on_disk),
                "{name} should report the real size so a demuxer can find the end"
            );
        }
    }

    /// Seeking past the end of the file.
    ///
    /// The UI computes a seek target from the reported duration. If that
    /// duration is wrong - or a drag reaches the very end of the bar - the
    /// target can land beyond the last byte, and what the demuxer does then is
    /// worth knowing rather than assuming.
    #[test]
    fn seeking_beyond_the_end_is_survivable() {
        let Some(media) = std::env::var_os("MYVID_TEST_MEDIA") else {
            eprintln!("skipped: set MYVID_TEST_MEDIA");
            return;
        };

        let (tx, rx) = mpsc::channel::<String>();
        let emit: EventSink = Arc::new(move |event| {
            if let Event::Error(message) = event {
                let _ = tx.send(message);
            }
        });

        let engine = GstEngine::new(emit).expect("engine");
        let file = std::fs::File::open(&media).expect("open the media");
        // MYVID_TEST_SCHEME=fd reproduces the regression this test was written
        // for: handing the decoder a descriptor instead of a path made seeks
        // near the end of a file fail 5 times out of 5, where a path passes 5
        // out of 5. The player uses a path.
        let uri = if std::env::var_os("MYVID_TEST_SCHEME").as_deref()
            == Some(std::ffi::OsStr::new("fd"))
        {
            format!("fd://{}", std::os::fd::AsRawFd::as_raw_fd(&file))
        } else {
            let _ = &file;
            crate::engine::to_uri(&media.to_string_lossy()).expect("uri")
        };

        engine.open(&uri).expect("open");
        engine.play();
        std::thread::sleep(Duration::from_secs(3));

        for minutes in [200u64, 500, 174, 30] {
            engine.seek(Duration::from_secs(minutes * 60));
            std::thread::sleep(Duration::from_millis(1500));
        }
        std::thread::sleep(Duration::from_secs(2));

        let errors: Vec<String> = rx.try_iter().collect();
        assert!(errors.is_empty(), "seeking past the end failed: {errors:#?}");
    }

    /// Seeking a file handed over as a descriptor.
    ///
    /// The sandbox means the decoder is given an open descriptor rather than a
    /// path, so it plays `fd://N` through `fdsrc` instead of `filesrc`. Seeking
    /// is where those two differ, and this is the path the player actually uses.
    #[test]
    fn seeking_works_on_a_passed_descriptor() {
        let Some(media) = std::env::var_os("MYVID_TEST_MEDIA") else {
            eprintln!("skipped: set MYVID_TEST_MEDIA");
            return;
        };

        let (tx, rx) = mpsc::channel::<String>();
        let emit: EventSink = Arc::new(move |event| {
            if let Event::Error(message) = event {
                let _ = tx.send(message);
            }
        });

        let engine = GstEngine::new(emit).expect("engine");

        // Held open for the whole test; closing it would pull the file out from
        // under the pipeline.
        let file = std::fs::File::open(&media).expect("open the media");
        let uri = format!("fd://{}", std::os::fd::AsRawFd::as_raw_fd(&file));

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
        assert!(errors.is_empty(), "seeking a descriptor failed: {errors:#?}");
    }

    /// Dragging the scrub bar fires a seek per pixel of movement. Every one of
    /// those used to be sent straight at the pipeline, which refuses a seek
    /// while a flush is still resolving, so most failed and each failure was
    /// reported to the user.
    #[test]
    fn a_burst_of_seeks_does_not_error() {
        let Some(media) = std::env::var_os("MYVID_TEST_MEDIA") else {
            eprintln!("skipped: set MYVID_TEST_MEDIA");
            return;
        };

        let (tx, rx) = mpsc::channel::<String>();
        let emit: EventSink = Arc::new(move |event| {
            if let Event::Error(message) = event {
                let _ = tx.send(message);
            }
        });

        let engine = GstEngine::new(emit).expect("engine");
        let uri = crate::engine::to_uri(&media.to_string_lossy()).expect("uri");
        engine.open(&uri).expect("open");
        engine.play();
        std::thread::sleep(Duration::from_secs(2));

        // A drag across the bar, at the rate a pointer actually produces.
        for step in 0..80 {
            engine.seek(Duration::from_millis(1_000 + step * 250));
            std::thread::sleep(Duration::from_millis(8));
        }
        std::thread::sleep(Duration::from_secs(2));

        let errors: Vec<String> = rx.try_iter().collect();
        assert!(errors.is_empty(), "a scrub produced errors: {errors:#?}");
    }

    /// Opening a second file into a live pipeline — what dragging a file onto a
    /// playing window does — must not error. `playsink` keeps state across a
    /// URI change, and getting the reset wrong shows up here rather than in the
    /// window.
    #[test]
    fn opening_a_second_file_does_not_error() {
        let Some(media) = std::env::var_os("MYVID_TEST_MEDIA") else {
            eprintln!("skipped: set MYVID_TEST_MEDIA");
            return;
        };
        let Some(second) = std::env::var_os("MYVID_TEST_MEDIA2") else {
            eprintln!("skipped: set MYVID_TEST_MEDIA2");
            return;
        };

        let (tx, rx) = mpsc::channel::<String>();
        let emit: EventSink = Arc::new(move |event| {
            if let Event::Error(message) = event {
                let _ = tx.send(message);
            }
        });

        let engine = GstEngine::new(emit).expect("engine");

        for (round, path) in [media, second].into_iter().enumerate() {
            let uri = crate::engine::to_uri(&path.to_string_lossy()).expect("uri");
            engine.open(&uri).unwrap_or_else(|e| panic!("open {round}: {e}"));
            engine.play();
            std::thread::sleep(Duration::from_secs(3));
        }

        let errors: Vec<String> = rx.try_iter().collect();
        assert!(errors.is_empty(), "reopening produced errors: {errors:#?}");
    }
}
