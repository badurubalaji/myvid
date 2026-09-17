//! The Modern direction: an edge-to-edge frame with one floating control
//! surface that gets out of the way when you stop touching it.

mod icons;
mod theme;

use std::sync::Arc;
use std::time::{Duration, Instant};

use iced::keyboard::{key::Named, Key};
use iced::widget::{
    button, column, container, row, slider, stack, text, text_input, Space,
};
use iced::window;
use iced::{Alignment, Element, Length, Subscription, Task};

use crate::engine::{
    self, format_time, AudioEffects, ClipRequest, Container, Export, FrameSlot, MediaInfo,
    PlaybackEngine, Track, TrackKind, MAX_VOLUME,
};
use crate::render::VideoSurface;
use icons::Glyph;

/// How long the pointer must sit still before the chrome retreats.
const IDLE_TIMEOUT: Duration = Duration::from_millis(2400);
const TICK: Duration = Duration::from_millis(150);
const SKIP: i64 = 10;
/// Identifier for the URL field, so it can be focused when the prompt opens.
const URL_FIELD: &str = "myvid-url-field";
/// Playback speeds `[` and `]` step through.
const RATES: &[f64] = &[0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0];

pub fn run() -> iced::Result {
    iced::application(Myvid::boot, Myvid::update, Myvid::view)
        .subscription(Myvid::subscription)
        .title(Myvid::title)
        .theme(Myvid::theme)
        .window(window::Settings {
            min_size: Some(iced::Size::new(480.0, 300.0)),
            // Wayland takes the window icon from the desktop entry whose
            // basename matches this id, so the icon costs nothing in the binary.
            platform_specific: window::settings::PlatformSpecific {
                application_id: "myvid".to_owned(),
                ..Default::default()
            },
            ..window::Settings::default()
        })
        .window_size((1280.0, 720.0))
        .antialiasing(true)
        .run()
}

pub struct Myvid {
    engine: Option<Arc<dyn PlaybackEngine>>,
    slot: FrameSlot,
    state: engine::State,
    info: MediaInfo,
    source: Option<String>,
    position: Duration,
    duration: Option<Duration>,
    volume: f32,
    muted: bool,
    effects: AudioEffects,
    rate: f64,
    buffering: Option<u8>,
    subtitle: Option<String>,
    /// Media position at which the current cue stops being shown.
    subtitle_end: Option<Duration>,
    tracks: Vec<Track>,
    panel: bool,
    clip_in: Option<Duration>,
    clip_out: Option<Duration>,
    container: Container,
    export: Option<Export>,
    error: Option<String>,
    chrome: bool,
    last_activity: Instant,
    fullscreen: bool,
    /// A file is being dragged over the window.
    hovering: bool,
    /// Contents of the URL prompt while it is open.
    url_prompt: Option<String>,
    /// A file named on the command line, held until the engine exists.
    pending: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Engine(engine::Event),
    Tick(Instant),
    Key(iced::keyboard::Event),
    Activity,
    TogglePlay,
    Skip(i64),
    SeekFraction(f32),
    SetVolume(f32),
    ToggleMute,
    SetAudioEffects(AudioEffects),
    StepRate(i32),
    OpenDialog,
    Picked(Option<std::path::PathBuf>),
    ToggleFullscreen,
    TogglePanel,
    Dropped(std::path::PathBuf),
    DragOver(bool),
    SelectTrack(TrackKind, Option<String>),
    SetRate(f64),
    MarkIn,
    MarkOut,
    ClearClip,
    SetContainer(Container),
    ExportClip,
    ShowUrlPrompt,
    UrlInput(String),
    SubmitUrl,
    CloseUrlPrompt,
    CopyError,
    DismissError,
}

impl Myvid {
    fn boot() -> (Self, Task<Message>) {
        let pending = std::env::args().nth(1);

        (
            Self {
                engine: None,
                slot: FrameSlot::new(),
                state: engine::State::Idle,
                info: MediaInfo::default(),
                source: None,
                position: Duration::ZERO,
                duration: None,
                volume: 1.0,
                muted: false,
                effects: AudioEffects::default(),
                rate: 1.0,
                buffering: None,
                subtitle: None,
                subtitle_end: None,
                tracks: Vec::new(),
                panel: false,
                clip_in: None,
                clip_out: None,
                container: Container::Matroska,
                export: None,
                error: None,
                chrome: true,
                last_activity: Instant::now(),
                fullscreen: false,
                hovering: false,
                url_prompt: None,
                pending,
            },
            Task::none(),
        )
    }

    fn theme(&self) -> iced::Theme {
        iced::Theme::Dark
    }

    fn title(&self) -> String {
        if self.info.title.is_empty() {
            match &self.source {
                Some(uri) => format!("{} — myvid", short_name(uri)),
                None => "myvid".to_owned(),
            }
        } else {
            format!("{} — myvid", self.info.title)
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Engine(event) => return self.on_engine_event(event),

            Message::Tick(now) => {
                if let Some(engine) = &self.engine {
                    if let Some(position) = engine.position() {
                        self.position = position;
                    }
                    if self.duration.is_none() {
                        self.duration = engine.duration();
                    }
                }
                if self.subtitle_end.is_some_and(|end| self.position > end) {
                    self.subtitle = None;
                    self.subtitle_end = None;
                }
                // Chrome only hides while something is actually playing.
                let idle = now.duration_since(self.last_activity) > IDLE_TIMEOUT;
                self.chrome = !(idle && self.state.is_playing() && self.buffering.is_none());
            }

            Message::Activity => {
                self.last_activity = Instant::now();
                self.chrome = true;
            }

            Message::Key(event) => return self.on_key(event),

            Message::TogglePlay => {
                self.wake();
                if let Some(engine) = &self.engine {
                    if self.state.is_playing() {
                        engine.pause();
                    } else {
                        engine.play();
                    }
                }
            }

            Message::Skip(seconds) => {
                self.wake();
                // Nothing open, or a live source with no duration, cannot be
                // seeked. Saying nothing beats reporting a failure the user did
                // not cause.
                if self.source.is_none() || self.duration.is_none() {
                    return Task::none();
                }
                if let Some(engine) = &self.engine {
                    let target = if seconds.is_negative() {
                        self.position
                            .saturating_sub(Duration::from_secs(seconds.unsigned_abs()))
                    } else {
                        let next = self.position + Duration::from_secs(seconds as u64);
                        match self.duration {
                            Some(total) if next > total => total,
                            _ => next,
                        }
                    };
                    self.position = target;
                    self.subtitle = None;
                    engine.seek(target);
                }
            }

            Message::SeekFraction(fraction) => {
                self.wake();
                if let (Some(engine), Some(total)) = (&self.engine, self.duration) {
                    let target = total.mul_f32(fraction.clamp(0.0, 1.0));
                    self.position = target;
                    self.subtitle = None;
                    engine.seek(target);
                }
            }

            Message::SetVolume(volume) => {
                self.wake();
                self.volume = volume.clamp(0.0, MAX_VOLUME as f32);
                self.muted = self.volume == 0.0;
                if let Some(engine) = &self.engine {
                    engine.set_volume(self.volume as f64);
                }
            }

            Message::ToggleMute => {
                self.wake();
                self.muted = !self.muted;
                if let Some(engine) = &self.engine {
                    engine.set_volume(if self.muted { 0.0 } else { self.volume as f64 });
                }
            }

            Message::SetAudioEffects(effects) => {
                self.effects = effects;
                if let Some(engine) = &self.engine {
                    engine.set_audio_effects(effects);
                }
            }

            Message::StepRate(direction) => {
                self.wake();
                // Snap to the nearest listed rate, then step from there.
                let current = RATES
                    .iter()
                    .position(|r| (r - self.rate).abs() < f64::EPSILON)
                    .unwrap_or(RATES.len() / 2) as i32;
                let next = (current + direction).clamp(0, RATES.len() as i32 - 1) as usize;
                self.rate = RATES[next];
                if let Some(engine) = &self.engine {
                    engine.set_rate(self.rate);
                }
            }

            Message::OpenDialog => {
                self.wake();
                return Task::perform(pick_file(), Message::Picked);
            }

            Message::Picked(Some(path)) => {
                return self.open(&path.to_string_lossy());
            }
            Message::Picked(None) => {}

            Message::ToggleFullscreen => {
                self.wake();
                self.fullscreen = !self.fullscreen;
                let mode = if self.fullscreen {
                    window::Mode::Fullscreen
                } else {
                    window::Mode::Windowed
                };
                return window::latest().and_then(move |id| window::set_mode(id, mode));
            }

            Message::Dropped(path) => {
                self.hovering = false;
                self.wake();
                return self.open(&path.to_string_lossy());
            }

            Message::DragOver(hovering) => {
                self.hovering = hovering;
                if hovering {
                    self.wake();
                }
            }

            Message::TogglePanel => {
                self.wake();
                self.panel = !self.panel;
            }

            Message::SelectTrack(kind, id) => {
                self.wake();
                if let Some(engine) = &self.engine {
                    engine.select_track(kind, id.as_deref());
                }
                if kind == TrackKind::Text && id.is_none() {
                    self.subtitle = None;
                    self.subtitle_end = None;
                }
            }

            Message::SetRate(rate) => {
                self.wake();
                self.rate = rate;
                if let Some(engine) = &self.engine {
                    engine.set_rate(rate);
                }
            }

            Message::MarkIn => {
                self.wake();
                self.clip_in = Some(self.position);
                // An in point past the out point is meaningless; drop the out.
                if self.clip_out.is_some_and(|out| out <= self.position) {
                    self.clip_out = None;
                }
                self.export = None;
            }

            Message::MarkOut => {
                self.wake();
                if self.clip_in.is_none_or(|start| start < self.position) {
                    self.clip_out = Some(self.position);
                    self.export = None;
                } else {
                    self.error = Some("the out point must come after the in point".into());
                }
            }

            Message::ClearClip => {
                self.wake();
                self.clip_in = None;
                self.clip_out = None;
                self.export = None;
            }

            Message::SetContainer(container) => {
                self.wake();
                self.container = container;
            }

            Message::ExportClip => {
                self.wake();
                let (Some(start), Some(end)) = (self.clip_in, self.clip_out) else {
                    self.error = Some("set an in and an out point first".into());
                    return Task::none();
                };
                let Some(output) = self.clip_path(start, end) else {
                    self.error = Some("clip export needs a local file".into());
                    return Task::none();
                };
                if let Some(engine) = &self.engine {
                    self.export = Some(Export::Progress(0.0));
                    engine.export_clip(ClipRequest {
                        start,
                        end,
                        output,
                        container: self.container,
                    });
                }
            }

            Message::ShowUrlPrompt => {
                self.wake();
                self.url_prompt = Some(String::new());
                return iced::widget::operation::focus(URL_FIELD);
            }

            Message::UrlInput(value) => {
                self.url_prompt = Some(value);
            }

            Message::SubmitUrl => {
                let Some(url) = self.url_prompt.take().filter(|u| !u.trim().is_empty()) else {
                    self.url_prompt = None;
                    return Task::none();
                };
                return self.open(url.trim());
            }

            Message::CloseUrlPrompt => {
                self.url_prompt = None;
            }

            Message::CopyError => {
                if let Some(message) = &self.error {
                    return iced::clipboard::write(message.clone());
                }
            }

            Message::DismissError => self.error = None,
        }

        Task::none()
    }

    fn on_engine_event(&mut self, event: engine::Event) -> Task<Message> {
        match event {
            engine::Event::Ready(engine) => {
                self.slot = engine.frames();
                engine.set_volume(self.volume as f64);
                engine.set_audio_effects(self.effects);
                self.engine = Some(engine);

                if let Some(pending) = self.pending.take() {
                    return self.open(&pending);
                }
            }
            engine::Event::Loaded(info) => {
                // Keep whatever we already know; tags arrive piecemeal.
                if !info.title.is_empty() {
                    self.info.title = info.title;
                }
                if !info.video_codec.is_empty() {
                    self.info.video_codec = info.video_codec;
                }
                if !info.audio_codec.is_empty() {
                    self.info.audio_codec = info.audio_codec;
                }
                if !info.container.is_empty() {
                    self.info.container = info.container;
                }
                if info.audio_channels > 0 {
                    self.info.audio_channels = info.audio_channels;
                    self.info.audio_rate = info.audio_rate;
                    self.info.audio_bitrate = info.audio_bitrate;
                }
                self.info.hardware = info.hardware;
            }
            engine::Event::State(state) => {
                self.state = state;
                if state.is_playing() {
                    self.buffering = None;
                }
            }
            engine::Event::Duration(duration) => self.duration = Some(duration),
            engine::Event::Buffering(percent) => {
                self.buffering = (percent < 100).then_some(percent);
            }
            engine::Event::Frame(width, height) => {
                // Carried on the event so the UI thread never takes the frame
                // lock just to learn the resolution.
                self.info.width = width;
                self.info.height = height;
            }
            engine::Event::Subtitle { text, start, end } => {
                // The cue arrives at its presentation time, but position is
                // polled — trust the buffer's own timestamps over our sample.
                self.subtitle_end = text.is_some().then_some(end);
                if text.is_some() {
                    self.position = self.position.max(start);
                }
                self.subtitle = text;
            }
            engine::Event::Tracks(tracks) => self.tracks = tracks,
            engine::Event::Export(state) => {
                if let Export::Failed(message) = &state {
                    self.error = Some(format!("clip export failed: {message}"));
                }
                self.export = Some(state);
            }
            engine::Event::Eos => {
                self.state = engine::State::Ended;
                self.chrome = true;
            }
            engine::Event::Error(message) => {
                self.error = Some(message);
                self.chrome = true;
            }
        }

        Task::none()
    }

    fn on_key(&mut self, event: iced::keyboard::Event) -> Task<Message> {
        let iced::keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return Task::none();
        };

        self.wake();

        // While the prompt is open only Escape is a shortcut; everything else
        // is someone typing a URL.
        if self.url_prompt.is_some() {
            return match key.as_ref() {
                Key::Named(Named::Escape) => self.update(Message::CloseUrlPrompt),
                _ => Task::none(),
            };
        }

        let (letter, ctrl) = chord(&key, modifiers);

        // Ctrl chords are their own namespace. Keeping them in the same match as
        // the single-key shortcuts meant that when the modifier did not come
        // through, Ctrl+L fell past its guard onto plain L and seeked instead of
        // opening the URL prompt — and Ctrl+O marked a clip point.
        if ctrl {
            let message = match letter.as_deref() {
                Some("l") => Some(Message::ShowUrlPrompt),
                Some("o") => Some(Message::OpenDialog),
                _ => None,
            };
            return match message {
                Some(message) => self.update(message),
                None => Task::none(),
            };
        }

        let message = match key.as_ref() {
            Key::Named(Named::Space) => Some(Message::TogglePlay),
            Key::Named(Named::ArrowLeft) => Some(Message::Skip(-5)),
            Key::Named(Named::ArrowRight) => Some(Message::Skip(5)),
            Key::Named(Named::ArrowUp) => Some(Message::SetVolume(self.volume + 0.05)),
            Key::Named(Named::ArrowDown) => Some(Message::SetVolume(self.volume - 0.05)),
            Key::Named(Named::Escape) if self.fullscreen => Some(Message::ToggleFullscreen),
            _ => match letter.as_deref() {
                Some("j") => Some(Message::Skip(-SKIP)),
                Some("l") => Some(Message::Skip(SKIP)),
                Some("k") => Some(Message::TogglePlay),
                Some("f") => Some(Message::ToggleFullscreen),
                Some("m") => Some(Message::ToggleMute),
                Some("o") => Some(Message::MarkOut),
                Some("i") => Some(Message::MarkIn),
                Some("t") => Some(Message::TogglePanel),
                Some("e") if self.clip_in.is_some() => Some(Message::ExportClip),
                Some("[") => Some(Message::StepRate(-1)),
                Some("]") => Some(Message::StepRate(1)),
                _ => None,
            },
        };

        match message {
            Some(message) => self.update(message),
            None => Task::none(),
        }
    }

    fn open(&mut self, location: &str) -> Task<Message> {
        let Some(engine) = &self.engine else {
            self.pending = Some(location.to_owned());
            return Task::none();
        };

        match engine::to_uri(location) {
            Ok(uri) => {
                self.info = MediaInfo::default();
                self.position = Duration::ZERO;
                self.duration = None;
                self.rate = 1.0;
                self.subtitle = None;
                self.subtitle_end = None;
                self.tracks.clear();
                self.clip_in = None;
                self.clip_out = None;
                self.export = None;
                self.error = None;
                self.source = Some(uri.clone());

                if let Err(err) = engine.open(&uri) {
                    self.error = Some(err.to_string());
                } else {
                    engine.play();
                }
            }
            Err(err) => self.error = Some(err.to_string()),
        }

        Task::none()
    }

    fn wake(&mut self) {
        self.last_activity = Instant::now();
        self.chrome = true;
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            Subscription::run(engine_events).map(Message::Engine),
            iced::time::every(TICK).map(Message::Tick),
            iced::keyboard::listen().map(Message::Key),
            iced::event::listen_with(|event, _status, _window| match event {
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { .. })
                | iced::Event::Mouse(iced::mouse::Event::ButtonPressed(_)) => {
                    Some(Message::Activity)
                }
                iced::Event::Window(window::Event::FileDropped(path)) => {
                    Some(Message::Dropped(path))
                }
                iced::Event::Window(window::Event::FileHovered(_)) => {
                    Some(Message::DragOver(true))
                }
                iced::Event::Window(window::Event::FilesHoveredLeft) => {
                    Some(Message::DragOver(false))
                }
                _ => None,
            }),
        ])
    }

    fn view(&self) -> Element<'_, Message> {
        let stage = container(
            iced::widget::Shader::new(VideoSurface::new(self.slot.clone()))
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::stage);

        let mut layers: Vec<Element<'_, Message>> = vec![stage.into()];

        if self.source.is_none() {
            layers.push(self.empty_state());
        } else if self.hovering {
            layers.push(self.drop_overlay());
        } else if self.chrome {
            layers.push(self.chrome_layer());
        }

        if self.panel && self.source.is_some() {
            layers.push(self.panel_layer());
        }

        if let Some(cue) = &self.subtitle {
            layers.push(self.subtitle_layer(cue));
        }

        if let Some(percent) = self.buffering {
            layers.push(self.buffering_layer(percent));
        }

        if let Some(message) = &self.error {
            layers.push(self.error_layer(message));
        }

        if let Some(value) = &self.url_prompt {
            layers.push(self.url_layer(value));
        }

        stack(layers).width(Length::Fill).height(Length::Fill).into()
    }

    // --- layers ---------------------------------------------------------

    fn empty_state(&self) -> Element<'_, Message> {
        let open = button(
            row![
                icons::icon(Glyph::Folder, 16.0, theme::SURFACE),
                text("Open file").size(13)
            ]
            .spacing(9)
            .align_y(Alignment::Center),
        )
        .padding([9, 16])
        .style(theme::accent_button)
        .on_press(Message::OpenDialog);

        let open_url = button(
            row![
                text("Open URL").size(12.5).color(theme::TEXT),
                text("Ctrl L").size(10).color(theme::FAINT),
            ]
            .spacing(9)
            .align_y(Alignment::Center),
        )
        .padding([9, 16])
        .style(theme::ghost_button)
        .on_press(Message::ShowUrlPrompt);

        let shortcuts = row![
            hint("Space", "play / pause"),
            hint("J / L", "±10 s"),
            hint("F", "fullscreen"),
            hint("M", "mute"),
            hint("[ ]", "speed"),
            hint("Ctrl O", "open file"),
        ]
        .spacing(22);

        let (icon_color, prompt) = if self.hovering {
            (theme::ACCENT, "Drop to play")
        } else {
            (theme::FAINT, "Drop a video here")
        };

        let body = column![
            icons::icon(Glyph::Folder, 40.0, icon_color),
            column![
                text(prompt).size(18).color(theme::TEXT),
                text("or open a stream URL").size(12).color(theme::FAINT),
            ]
            .spacing(7)
            .align_x(Alignment::Center),
            row![open, open_url].spacing(11).align_y(Alignment::Center),
            Space::new().height(6),
            shortcuts,
            text(
                "MKV · MP4 · WebM · AVI · MOV · TS · FLV   —   H.264 · HEVC · AV1 · VP9 · MPEG-2/4 · ProRes"
            )
            .size(10)
            .color(theme::FAINT),
        ]
        .spacing(19)
        .align_x(Alignment::Center);

        container(body)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }

    /// Somewhere to paste a stream URL. Reaching this only from the command
    /// line was not a feature.
    fn url_layer<'a>(&self, value: &'a str) -> Element<'a, Message> {
        let field = text_input("https://example.com/stream.m3u8", value)
            .id(URL_FIELD)
            .size(14)
            .padding([11, 14])
            .style(theme::field)
            .on_input(Message::UrlInput)
            .on_submit(Message::SubmitUrl);

        let body = column![
            text("Open a stream").size(15).color(theme::TEXT),
            text("HTTP, HTTPS, HLS, DASH, RTSP, RTMP, UDP and SRT")
                .size(10)
                .color(theme::FAINT),
            field,
            row![
                text("Enter to play · Esc to cancel")
                    .size(10)
                    .color(theme::FAINT),
                Space::new().width(Length::Fill),
                button(text("Cancel").size(12).color(theme::MUTED))
                    .padding([8, 14])
                    .style(theme::ghost_button)
                    .on_press(Message::CloseUrlPrompt),
                button(text("Play").size(12))
                    .padding([8, 16])
                    .style(theme::accent_button)
                    .on_press(Message::SubmitUrl),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        ]
        .spacing(13);

        container(container(body).width(460).padding([20, 22]).style(theme::glass))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }

    /// Feedback while a file is dragged over a window that is already playing.
    fn drop_overlay(&self) -> Element<'_, Message> {
        container(
            container(text("Drop to play").size(16).color(theme::TEXT))
                .padding([14, 22])
                .style(theme::glass),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
    }

    fn chrome_layer(&self) -> Element<'_, Message> {
        column![
            self.title_bar(),
            Space::new().height(Length::Fill),
            self.control_bar(),
        ]
        .into()
    }

    fn title_bar(&self) -> Element<'_, Message> {
        let name = self
            .source
            .as_deref()
            .map(short_name)
            .unwrap_or_else(|| "—".to_owned());

        container(
            row![
                text("MYVID").size(11).color(theme::ACCENT),
                text(name).size(12).color(theme::TEXT),
                text(self.format_summary()).size(10).color(theme::FAINT),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        )
        .padding([14, 20])
        .into()
    }

    fn control_bar(&self) -> Element<'_, Message> {
        let total = self.duration.unwrap_or(Duration::ZERO);
        let fraction = if total.is_zero() {
            0.0
        } else {
            (self.position.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0)
        };

        let scrub = slider(0.0..=1.0, fraction, Message::SeekFraction)
            .step(0.0001_f32)
            .style(theme::scrub);

        let play_glyph = if self.state.is_playing() {
            Glyph::Pause
        } else {
            Glyph::Play
        };

        let transport = row![
            icon_button(Glyph::Back10, 21.0, Message::Skip(-SKIP)),
            button(
                container(icons::icon(play_glyph, 17.0, theme::TEXT))
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
            )
            .width(42)
            .height(42)
            .padding(0)
            .style(theme::primary_button)
            .on_press(Message::TogglePlay),
            icon_button(Glyph::Forward10, 21.0, Message::Skip(SKIP)),
            Space::new().width(6),
            icon_button(
                if self.muted { Glyph::Mute } else { Glyph::Volume },
                19.0,
                Message::ToggleMute
            ),
            container(
                slider(
                    0.0..=MAX_VOLUME as f32,
                    if self.muted { 0.0 } else { self.volume },
                    Message::SetVolume
                )
                .step(0.01_f32)
                .style(theme::volume)
            )
            .width(96),
            // Only worth saying once it is past what the file itself provides.
            text(if !self.muted && self.volume > 1.0 {
                format!("{:.0}%", self.volume * 100.0)
            } else {
                String::new()
            })
            .size(11)
            .color(theme::ACCENT),
            Space::new().width(8),
            text(format!(
                "{} / {}",
                format_time(self.position),
                format_time(total)
            ))
            .size(12)
            .color(theme::TEXT),
            Space::new().width(Length::Fill),
            text(if self.rate == 1.0 {
                String::new()
            } else {
                // 1.25 -> "1.25x", 2.0 -> "2x"
                format!("{}x", self.rate)
            })
            .size(12)
            .color(theme::ACCENT),
            text(self.accelerator_label())
                .size(10)
                .color(if self.info.hardware {
                    theme::OK
                } else {
                    theme::MUTED
                }),
            icon_button(Glyph::Scissors, 20.0, Message::MarkIn),
            icon_button(Glyph::Sliders, 21.0, Message::TogglePanel),
            icon_button(
                if self.fullscreen {
                    Glyph::ExitFullscreen
                } else {
                    Glyph::Fullscreen
                },
                21.0,
                Message::ToggleFullscreen
            ),
        ]
        .spacing(14)
        .align_y(Alignment::Center);

        let mut stack = column![scrub, transport].spacing(15);
        if let Some(clip) = self.clip_bar() {
            stack = stack.push(iced::widget::rule::horizontal(1));
            stack = stack.push(clip);
        }

        let bar = container(stack)
            .padding([18, 22])
            .style(theme::glass);

        container(bar).padding([26, 32]).into()
    }

    /// Cues sit above the control bar when it is up, and drop into the space it
    /// vacates when it is not — so a line never hides behind the chrome, and
    /// never floats oddly high once the chrome is gone.
    fn subtitle_layer<'a>(&self, cue: &'a str) -> Element<'a, Message> {
        let bottom = if self.chrome { 190 } else { 64 };

        container(
            container(text(cue).size(23).color(iced::Color::WHITE).center())
                .padding([7, 15])
                .style(theme::caption),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::End)
        .padding(iced::Padding::default().bottom(bottom))
        .into()
    }

    /// Tracks and speed, floating over the picture rather than in a dialog —
    /// playback continues, so you can hear the track you just chose.
    fn panel_layer(&self) -> Element<'_, Message> {
        let mut body = column![].spacing(20);

        body = body.push(section("Playback speed"));
        let mut speeds = row![].spacing(6);
        for rate in RATES {
            speeds = speeds.push(chip(
                format!("{rate}x"),
                (self.rate - rate).abs() < f64::EPSILON,
                Message::SetRate(*rate),
            ));
        }
        body = body.push(speeds);

        let audio: Vec<&Track> = self
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio)
            .collect();
        if !audio.is_empty() {
            body = body.push(section("Audio track"));
            let mut list = column![].spacing(4);
            for track in audio {
                list = list.push(track_row(track, Some(track.id.clone())));
            }
            body = body.push(list);
        }

        let text: Vec<&Track> = self
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Text)
            .collect();
        if !text.is_empty() {
            body = body.push(section("Subtitles"));
            let none_selected = !text.iter().any(|t| t.selected);
            let mut list = column![].spacing(4).push(simple_row(
                "Off",
                "",
                none_selected,
                Message::SelectTrack(TrackKind::Text, None),
            ));
            for track in text {
                list = list.push(track_row(track, Some(track.id.clone())));
            }
            body = body.push(list);
        }

        body = body.push(section("Sound"));
        body = body.push(
            column![
                simple_row(
                    "Clear dialogue",
                    // Stereo has no centre channel to lift, and guessing one
                    // from the mix colours the music as much as the voices.
                    "Lifts voices in 5.1 and 7.1 tracks",
                    self.effects.dialogue,
                    Message::SetAudioEffects(AudioEffects {
                        dialogue: !self.effects.dialogue,
                        ..self.effects
                    }),
                ),
                simple_row(
                    "Night mode",
                    "Quiet scenes up, loud scenes down",
                    self.effects.night,
                    Message::SetAudioEffects(AudioEffects {
                        night: !self.effects.night,
                        ..self.effects
                    }),
                ),
            ]
            .spacing(4),
        );

        body = body.push(section("Video"));
        body = body.push(
            column![
                fact("Resolution", format!("{}x{}", self.info.width, self.info.height)),
                fact(
                    "Decoder",
                    if self.info.hardware {
                        "hardware".to_owned()
                    } else {
                        "software".to_owned()
                    }
                ),
            ]
            .spacing(6),
        );

        let panel = container(body)
            .width(340)
            .padding([16, 18])
            .style(theme::glass);

        container(panel)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::End)
            .align_y(Alignment::Start)
            .padding(iced::Padding::from([60, 26]))
            .into()
    }

    /// The clip strip, shown only once an in or out point exists.
    fn clip_bar(&self) -> Option<Element<'_, Message>> {
        let (start, end) = (self.clip_in?, self.clip_out);

        let span = end.map(|e| e.saturating_sub(start));
        let mut left = row![
            marker("In", format_time(start)),
            marker("Out", end.map(format_time).unwrap_or_else(|| "—".into())),
            marker(
                "Length",
                span.map(format_time).unwrap_or_else(|| "—".into())
            ),
        ]
        .spacing(20)
        .align_y(Alignment::Center);

        left = left.push(
            button(text("Clear").size(11).color(theme::MUTED))
                .padding([5, 10])
                .style(theme::ghost_button)
                .on_press(Message::ClearClip),
        );

        let mut formats = row![].spacing(5);
        for container_kind in [Container::Matroska, Container::Mp4] {
            formats = formats.push(chip(
                format!("{} copy", container_kind.label()),
                self.container == container_kind,
                Message::SetContainer(container_kind),
            ));
        }

        let status: Element<'_, Message> = match &self.export {
            Some(Export::Progress(p)) => text(format!("Exporting {:.0}%", p * 100.0))
                .size(11)
                .color(theme::ACCENT)
                .into(),
            Some(Export::Done(path)) => text(format!(
                "Saved {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ))
            .size(11)
            .color(theme::OK)
            .into(),
            Some(Export::Failed(_)) => text("Export failed").size(11).color(theme::DANGER).into(),
            None => Space::new().width(0).into(),
        };

        let mut export = button(text("Export clip").size(12))
            .padding([9, 16])
            .style(theme::accent_button);
        if end.is_some() && !matches!(self.export, Some(Export::Progress(_))) {
            export = export.on_press(Message::ExportClip);
        }

        let caveat = text(
            "Stream copy — no quality loss. The cut starts at the keyframe at or before the in point.",
        )
        .size(10)
        .color(theme::FAINT);

        Some(
            column![
                row![
                    left,
                    Space::new().width(Length::Fill),
                    status,
                    formats,
                    export
                ]
                .spacing(14)
                .align_y(Alignment::Center),
                caveat,
            ]
            .spacing(9)
            .into(),
        )
    }

    /// Where a clip lands: beside the source, named for the range it covers.
    fn clip_path(&self, start: Duration, end: Duration) -> Option<std::path::PathBuf> {
        let uri = self.source.as_ref()?;
        let (path, _) = ::gstreamer::glib::filename_from_uri(uri).ok()?;
        let stem = path.file_stem()?.to_string_lossy().to_string();
        let name = format!(
            "{stem}-clip-{}-{}.{}",
            format_time(start).replace(':', ""),
            format_time(end).replace(':', ""),
            self.container.extension()
        );
        Some(path.with_file_name(name))
    }

    fn buffering_layer(&self, percent: u8) -> Element<'_, Message> {
        container(
            column![
                text("Buffering").size(14).color(theme::TEXT),
                text(format!("{percent}%")).size(11).color(theme::MUTED),
            ]
            .spacing(5)
            .align_x(Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
    }

    fn error_layer<'a>(&self, message: &'a str) -> Element<'a, Message> {
        let chip = container(
            row![
                text("Playback error").size(12).color(theme::DANGER),
                text(message).size(11).color(theme::MUTED),
                button(text("Copy").size(11).color(theme::TEXT))
                    .padding([4, 8])
                    .style(theme::ghost_button)
                    .on_press(Message::CopyError),
                button(text("Dismiss").size(11).color(theme::TEXT))
                    .padding([4, 8])
                    .style(theme::ghost_button)
                    .on_press(Message::DismissError),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        )
        .padding([9, 13])
        .style(theme::error_chip);

        container(chip)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Start)
            .padding(70)
            .into()
    }

    // --- readouts -------------------------------------------------------

    fn format_summary(&self) -> String {
        let mut parts = Vec::new();
        if self.info.width > 0 {
            parts.push(format!("{}×{}", self.info.width, self.info.height));
        }
        if !self.info.video_codec.is_empty() {
            parts.push(self.info.video_codec.clone());
        }
        let mut audio = String::new();
        if !self.info.audio_codec.is_empty() {
            audio.push_str(&self.info.audio_codec);
        }
        if self.info.audio_channels > 0 {
            if !audio.is_empty() {
                audio.push(' ');
            }
            audio.push_str(&channel_layout(self.info.audio_channels));
        }
        if self.info.audio_bitrate > 0 {
            audio.push_str(&format!(" {} kb/s", self.info.audio_bitrate / 1000));
        }
        if self.info.audio_rate > 0 {
            audio.push_str(&format!(" {} kHz", self.info.audio_rate / 1000));
        }
        if !audio.is_empty() {
            parts.push(audio);
        }
        parts.join(" · ")
    }

    fn accelerator_label(&self) -> &'static str {
        if self.info.hardware {
            "GPU decode"
        } else {
            "CPU decode"
        }
    }
}

// --- helpers ------------------------------------------------------------

fn icon_button<'a>(glyph: Glyph, size: f32, message: Message) -> Element<'a, Message> {
    button(icons::icon(glyph, size, theme::TEXT))
        .padding(5)
        .style(theme::ghost_button)
        .on_press(message)
        .into()
}

/// The letter a key press represents, and whether Ctrl was held.
///
/// Compositors disagree about Ctrl chords: some deliver Ctrl+L as `Character("l")`
/// with a Ctrl modifier, others as the control character U+000C with no modifier
/// set at all. Handling only the first spelling is why Ctrl+L seeked.
fn chord(key: &iced::keyboard::Key, modifiers: iced::keyboard::Modifiers) -> (Option<String>, bool) {
    let iced::keyboard::Key::Character(text) = key.as_ref() else {
        return (None, modifiers.command());
    };

    let mut chars = text.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return (None, modifiers.command());
    };

    // C0 control characters: Ctrl+A is 0x01 through Ctrl+Z at 0x1A.
    if ('\u{1}'..='\u{1a}').contains(&c) {
        let letter = (b'a' + (c as u8 - 1)) as char;
        return (Some(letter.to_string()), true);
    }

    (
        Some(c.to_lowercase().to_string()),
        modifiers.command(),
    )
}

fn section<'a>(title: &'a str) -> Element<'a, Message> {
    text(title).size(10).color(theme::FAINT).into()
}

/// A small selectable pill — speeds and container formats.
fn chip<'a>(label: String, selected: bool, message: Message) -> Element<'a, Message> {
    button(text(label).size(11))
        .padding([5, 10])
        .style(if selected {
            theme::accent_button
        } else {
            theme::ghost_button
        })
        .on_press(message)
        .into()
}

fn track_row<'a>(track: &Track, id: Option<String>) -> Element<'a, Message> {
    simple_row(
        &track.label,
        &track.detail,
        track.selected,
        Message::SelectTrack(track.kind, id),
    )
}

/// One line of a track list: what it is, what it is made of, and whether it is
/// the live one.
fn simple_row<'a>(
    label: &str,
    detail: &str,
    selected: bool,
    message: Message,
) -> Element<'a, Message> {
    let mark: Element<'_, Message> = if selected {
        text("•").size(14).color(theme::ACCENT).into()
    } else {
        Space::new().width(8).into()
    };

    let mut lines = column![text(label.to_owned())
        .size(12)
        .color(if selected { theme::TEXT } else { theme::MUTED })]
    .spacing(2);

    if !detail.is_empty() {
        lines = lines.push(text(detail.to_owned()).size(10).color(theme::FAINT));
    }

    button(
        row![mark, lines]
            .spacing(9)
            .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .padding([7, 10])
    .style(theme::ghost_button)
    .on_press(message)
    .into()
}

fn fact<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    row![
        text(label).size(11).color(theme::MUTED),
        Space::new().width(Length::Fill),
        text(value).size(11).color(theme::TEXT),
    ]
    .align_y(Alignment::Center)
    .into()
}

fn marker<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    column![
        text(label).size(9).color(theme::FAINT),
        text(value).size(12).color(theme::TEXT),
    ]
    .spacing(3)
    .into()
}

fn hint<'a>(key: &'a str, action: &'a str) -> Element<'a, Message> {
    row![
        text(key).size(10).color(theme::TEXT),
        text(action).size(10).color(theme::FAINT),
    ]
    .spacing(7)
    .align_y(Alignment::Center)
    .into()
}

/// `5.1` reads better than `6 channels` for anyone judging an audio track.
fn channel_layout(channels: u32) -> String {
    match channels {
        1 => "mono".into(),
        2 => "stereo".into(),
        6 => "5.1".into(),
        8 => "7.1".into(),
        n => format!("{n}ch"),
    }
}

/// The last path segment, percent-decoded enough to be readable.
fn short_name(uri: &str) -> String {
    let tail = uri.rsplit('/').next().unwrap_or(uri);
    tail.replace("%20", " ")
}

async fn pick_file() -> Option<std::path::PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Open video")
        .add_filter(
            "Video",
            &[
                "mkv", "mp4", "m4v", "webm", "avi", "mov", "ts", "m2ts", "flv", "ogv", "wmv",
                "mpg", "mpeg",
            ],
        )
        .add_filter("All files", &["*"])
        .pick_file()
        .await
        .map(|handle| handle.path().to_path_buf())
}

/// Owns the GStreamer engine and forwards its events into the iced runtime.
fn engine_events() -> impl iced::futures::Stream<Item = engine::Event> {
    use iced::futures::{channel::mpsc, SinkExt, StreamExt};

    iced::stream::channel(128, async move |mut output| {
        let (tx, mut rx) = mpsc::unbounded();

        let sink: engine::gst::EventSink = Arc::new(move |event| {
            let _ = tx.unbounded_send(event);
        });

        // Decoding happens in its own confined process. If that cannot be
        // started there is nothing to fall back to that would be honest: an
        // in-process decoder would work, but silently without the isolation the
        // user was told they had.
        match engine::remote::RemoteEngine::spawn(sink) {
            Ok(engine) => {
                let engine: Arc<dyn PlaybackEngine> = engine;
                let _ = output.send(engine::Event::Ready(engine)).await;
            }
            Err(err) => {
                let _ = output
                    .send(engine::Event::Error(format!(
                        "could not start the decode process: {err:#}"
                    )))
                    .await;
                return;
            }
        }

        // Forward events, collapsing frame notices.
        //
        // The engine posts one per decoded frame into an unbounded queue, and
        // this feeds a bounded one. If the UI falls even slightly behind, those
        // notices pile up without limit and every redraw shows a frame the audio
        // has already passed — the picture drifts further behind the sound the
        // longer it plays. Only the newest frame is worth drawing, so anything
        // already waiting is dropped in favour of it.
        let mut batch: Vec<engine::Event> = Vec::new();

        while let Some(first) = rx.next().await {
            batch.clear();
            batch.push(first);
            while let Ok(next) = rx.try_recv() {
                batch.push(next);
            }

            let newest_frame = batch
                .iter()
                .rposition(|event| matches!(event, engine::Event::Frame(..)));

            for (index, event) in batch.drain(..).enumerate() {
                let superseded = matches!(event, engine::Event::Frame(..))
                    && Some(index) != newest_frame;
                if superseded {
                    continue;
                }
                if output.send(event).await.is_err() {
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod chord_tests {
    use super::chord;
    use iced::keyboard::{Key, Modifiers};

    fn character(text: &str) -> Key {
        Key::Character(text.into())
    }

    #[test]
    fn reads_a_ctrl_chord_reported_as_a_modifier() {
        let (letter, ctrl) = chord(&character("l"), Modifiers::CTRL);
        assert_eq!(letter.as_deref(), Some("l"));
        assert!(ctrl);
    }

    /// Some compositors send Ctrl+L as U+000C with no modifier set. Missing this
    /// spelling is what made Ctrl+L seek forward instead of opening a URL.
    #[test]
    fn reads_a_ctrl_chord_reported_as_a_control_character() {
        let (letter, ctrl) = chord(&character("\u{c}"), Modifiers::empty());
        assert_eq!(letter.as_deref(), Some("l"));
        assert!(ctrl);

        let (letter, ctrl) = chord(&character("\u{f}"), Modifiers::empty());
        assert_eq!(letter.as_deref(), Some("o"));
        assert!(ctrl);
    }

    #[test]
    fn leaves_plain_keys_unmodified() {
        let (letter, ctrl) = chord(&character("l"), Modifiers::empty());
        assert_eq!(letter.as_deref(), Some("l"));
        assert!(!ctrl);
    }

    #[test]
    fn folds_shifted_letters_to_lowercase() {
        let (letter, _) = chord(&character("O"), Modifiers::SHIFT);
        assert_eq!(letter.as_deref(), Some("o"));
    }

    #[test]
    fn ignores_keys_that_are_not_single_characters() {
        assert_eq!(chord(&Key::Named(iced::keyboard::key::Named::Space), Modifiers::empty()).0, None);
        assert_eq!(chord(&character("ab"), Modifiers::empty()).0, None);
    }
}
