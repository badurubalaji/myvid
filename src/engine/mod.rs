//! Playback engine: the seam between the player and whatever decodes for it.
//!
//! Everything above this module talks to [`PlaybackEngine`], never to GStreamer
//! directly, so a libmpv or ffmpeg backend can be dropped in without the UI
//! noticing.

pub mod dsp;
pub mod frame;
pub mod gst;
pub mod protocol;
pub mod remote;
pub mod sandbox;
pub mod shm;
pub mod worker;

pub use frame::{FrameSlot, PlanarFrame};

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, bincode::Encode, bincode::Decode)]
pub enum State {
    Idle,
    Playing,
    Paused,
    Ended,
}

impl State {
    pub fn is_playing(self) -> bool {
        matches!(self, State::Playing)
    }
}

/// What the container and codecs actually turned out to be — read from pad caps
/// and stream tags, never guessed from the filename.
#[derive(Debug, Clone, Default, bincode::Encode, bincode::Decode)]
pub struct MediaInfo {
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub video_codec: String,
    pub audio_codec: String,
    pub audio_channels: u32,
    pub audio_rate: u32,
    pub audio_bitrate: u32,
    pub container: String,
    pub hardware: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, bincode::Encode, bincode::Decode)]
pub enum TrackKind {
    Audio,
    Text,
}

/// One selectable stream, described the way someone choosing between them needs
/// to see it — the codec and channel count, not just the language.
#[derive(Debug, Clone, bincode::Encode, bincode::Decode)]
pub struct Track {
    pub id: String,
    pub kind: TrackKind,
    pub label: String,
    pub detail: String,
    pub selected: bool,
}

/// Optional sound processing. Both off means the audio is left untouched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, bincode::Encode, bincode::Decode)]
pub struct AudioEffects {
    /// Lift the centre channel of surround tracks, where the voices are.
    pub dialogue: bool,
    /// Compress the dynamic range so quiet and loud scenes sit closer together.
    pub night: bool,
}

/// The highest volume the player offers. Anything above 1.0 is gain applied
/// ahead of a limiter, so it cannot clip.
pub const MAX_VOLUME: f64 = 1.5;

/// What a clip export should produce.
#[derive(Debug, Clone)]
pub struct ClipRequest {
    pub start: Duration,
    pub end: Duration,
    pub output: PathBuf,
    pub container: Container,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Matroska,
    Mp4,
}

impl Container {
    pub fn extension(self) -> &'static str {
        match self {
            Container::Matroska => "mkv",
            Container::Mp4 => "mp4",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Container::Matroska => "MKV",
            Container::Mp4 => "MP4",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Export {
    Progress(f32),
    Done(PathBuf),
    Failed(String),
}

/// Events flowing up from the engine to the UI.
#[derive(Clone)]
pub enum Event {
    /// First event: the engine is live and the UI may start driving it.
    Ready(Arc<dyn PlaybackEngine>),
    Loaded(MediaInfo),
    State(State),
    Duration(Duration),
    Buffering(u8),
    /// A new frame is in the slot, at this resolution.
    Frame(u32, u32),
    /// The selectable audio and subtitle streams of the current file.
    Tracks(Vec<Track>),
    /// Progress of a clip export.
    Export(Export),
    /// A subtitle cue and the media interval it belongs to.
    Subtitle {
        text: Option<String>,
        start: Duration,
        end: Duration,
    },
    Eos,
    Error(String),
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Ready(_) => write!(f, "Ready(..)"),
            Event::Loaded(i) => write!(f, "Loaded({}x{})", i.width, i.height),
            Event::State(s) => write!(f, "State({s:?})"),
            Event::Duration(d) => write!(f, "Duration({:.2}s)", d.as_secs_f64()),
            Event::Buffering(p) => write!(f, "Buffering({p}%)"),
            Event::Frame(w, h) => write!(f, "Frame({w}x{h})"),
            Event::Subtitle { text, start, end } => match text {
                Some(t) => write!(
                    f,
                    "Subtitle({} chars, {:.2}s..{:.2}s)",
                    t.len(),
                    start.as_secs_f64(),
                    end.as_secs_f64()
                ),
                None => write!(f, "Subtitle(clear)"),
            },
            Event::Tracks(t) => write!(f, "Tracks({})", t.len()),
            Event::Export(e) => write!(f, "Export({e:?})"),
            Event::Eos => write!(f, "Eos"),
            Event::Error(e) => write!(f, "Error({e})"),
        }
    }
}

/// The whole surface the UI is allowed to touch.
pub trait PlaybackEngine: Send + Sync + 'static {
    fn open(&self, uri: &str) -> anyhow::Result<()>;
    fn play(&self);
    fn pause(&self);
    fn seek(&self, to: Duration);
    fn position(&self) -> Option<Duration>;
    fn duration(&self) -> Option<Duration>;
    /// `0.0..=MAX_VOLUME`; above 1.0 is a boost.
    fn set_volume(&self, volume: f64);
    fn set_audio_effects(&self, effects: AudioEffects);
    fn set_rate(&self, rate: f64);
    fn frames(&self) -> FrameSlot;

    /// Switch the active audio or subtitle stream. `None` turns it off, which
    /// only subtitles allow.
    fn select_track(&self, kind: TrackKind, id: Option<&str>);

    /// Copy a range of the current file into a new one without re-encoding.
    fn export_clip(&self, request: ClipRequest);
}

/// Turn a path or a user-typed location into something the engine can open.
pub fn to_uri(input: &str) -> anyhow::Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty location");
    }
    // Anything with a scheme we recognise passes through untouched.
    if let Some((scheme, _)) = trimmed.split_once("://") {
        if matches!(
            scheme,
            "http" | "https" | "file" | "rtsp" | "rtmp" | "udp" | "srt" | "hls"
        ) {
            return Ok(trimmed.to_owned());
        }
    }
    let path = std::path::Path::new(trimmed);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if !absolute.exists() {
        anyhow::bail!("no such file: {}", absolute.display());
    }
    Ok(::gstreamer::glib::filename_to_uri(&absolute, None)?.to_string())
}

/// `01:12:44` for anything an hour or longer, `12:44` below that.
pub fn format_time(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}
