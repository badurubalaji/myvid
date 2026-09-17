# myvid — architecture

Three layers, one process (for now).

```
src/
├── engine/       decode
│   ├── mod.rs    PlaybackEngine trait, Event, MediaInfo, uri handling
│   ├── frame.rs  the decode -> render hand-off (one slot, last frame wins)
│   ├── gst.rs    GStreamer backend: playbin3 + appsink
│   ├── sandbox.rs  Landlock policy for the decode process
│   ├── protocol.rs IPC: SOCK_SEQPACKET, bincode, descriptor passing
│   ├── shm.rs    the shared frame ring
│   ├── worker.rs the decode process itself
│   └── remote.rs the player's handle on it
├── render/       presentation
│   ├── mod.rs    iced shader::Program + wgpu Primitive/Pipeline
│   └── nv12.wgsl NV12 -> RGB, BT.709 limited range, letterboxing
└── ui/           chrome
    ├── mod.rs    application state, update, view, subscriptions
    ├── theme.rs  palette and widget styles (the "Modern" direction)
    └── icons.rs  vector icons drawn on a 24x24 grid
```

## The decode process is confined

Every demuxer and decoder below us is C parsing untrusted input, which is the
oldest remote-code-execution surface there is. Rust around it buys nothing. So
decoding runs in its own process, which is then confined with Landlock.

The player and the decoder are the same binary; `--decode-worker <fd>` selects
the second mode. They share one `SOCK_SEQPACKET` socket: sequenced packets keep
message boundaries, so each send is one message needing no length framing, and a
descriptor attached to it arrives with the message it belongs to.

**Each file gets its own decoder, confined to that file.** The player starts a
decode process per file and tells it the path *before* the sandbox closes, so the
ruleset can name it: that process can then read exactly one film and nothing
else, not even its neighbours in the same directory. A new file means a new
process, because Landlock can only ever be narrowed — a decoder confined for one
film can never be widened to another. The cost is a process start per open, paid
at the moment a person chooses something.

An earlier version passed a *descriptor* instead, so the decoder needed no path
at all and could be denied the filesystem outright. That was tighter, and it was
wrong: `fdsrc` mishandles seeks near the end of a file. Measured over five runs
each on the same file, `fd://` failed 5 times and `file://` failed 0, with the
demuxer reporting `got eos and didn't receive a complete header object`. Naming
the descriptor through `/proc/self/fd/N` does not rescue it either — Landlock
resolves that to the real path and denies it, which is the sandbox working
correctly. A player that cannot seek is not worth a tighter sandbox, so the
sandbox got looser by exactly one file.

**Order matters more than policy.** GStreamer builds its plugin registry under
`$HOME/.cache` and reads widely doing it, so the sandbox is applied *after*
`gst::init()` and after the pipeline exists. Confining earlier would either break
the registry or force a policy so wide it protects nothing.

What stays reachable: `/usr`, `/lib`, `/etc`, `/proc`, `/sys` read-only for
lazily-loaded codec plugins; `/dev/dri` for VA-API; the session's
`XDG_RUNTIME_DIR` for the audio socket. What does not: `$HOME`, `/tmp`,
`/media`, `/mnt`, and every other user's files.

Verified two ways. `tests/sandbox.rs` drives `--sandbox-selftest`, which confines
itself and then tries to open one path — Landlock cannot be lifted, so this needs
a process per path. And `MYVID_DIAG=1` makes the live decoder report whether it
can still read `$HOME` after confining:

```
myvid decoder: landlock: enforced
[diag] decoder can read $HOME: no
```

**What it costs.** Frames must now be copied into shared memory, because the
decoder's own buffer is in another address space — so the zero-copy path is gone
across the process boundary, though the copy from decoder memory into the ring is
still the only one. A three-slot ring lets the writer fill the next frame while
the reader holds the last, so no lock is needed and the writer only laps the
reader if the player stalls for several frames, by which point the picture is
stale anyway. Two processes also means two sets of libraries resident.

Clip export deliberately stays in the *player*. It reads a path and writes a new
one, which is precisely the ability the sandbox exists to remove.

**Seeks are clamped inside the media and snap backwards.** A target at or past
the end puts the demuxer beyond the last byte, where it reads nothing and reports
a missing header — killing playback. `KEY_UNIT` alone makes this worse, because
it snaps to the *nearest* keyframe, which near the end means snapping forward off
the end. Targets are clamped two seconds short of the duration and seek with
`SNAP_BEFORE`, so a seek always lands on data.

**The decoder cannot be wedged, and cannot outlive the player.** Two faults found
while chasing the seek bug, both in the process split rather than in GStreamer:

- Frame notices were sent with a blocking write. A player that stopped reading
  filled the socket, the decode loop blocked in `sendmsg`, and it could then no
  longer notice the player had gone. Frame notices now use a non-blocking send
  and are dropped when the socket is full — the next frame supersedes them
  anyway. Control messages still block, because losing those matters.
- A killed player left a decoder running, still holding the audio device and the
  media file. The worker now sets `PR_SET_PDEATHSIG`, so the kernel takes it down
  with its parent, and checks for an already-dead parent in case it lost the
  race.

`EINTR` is also retried on both sides. Treating a signal as "the peer is gone"
tore down a healthy connection and deadlocked the other end.

## Decisions worth knowing

**GStreamer owns A/V sync.** `playbin3` picks demuxers, decoders (hardware where
available) and the audio sink; the audio sink is the pipeline clock. We replace
only the video sink. Writing our own sync loop is how these projects die.

**Frames cross threads through one mutex-guarded slot.** The decode thread
replaces what is there; the render thread reads whatever it finds. No queue, no
back-pressure: if the GPU misses a frame, the next one is already more correct
than the one it missed. A `generation` counter lets the renderer skip an
unchanged frame.

**The slot holds the decoder's own buffer, not a copy of it.** `wgpu`'s
`Queue::write_texture` accepts an arbitrary row stride — the 256-byte alignment
in `COPY_BYTES_PER_ROW_ALIGNMENT` applies to buffer-to-texture copies, not to
this path (`wgpu-core` passes `aligned: false` here, with a comment saying so).
So the appsink hands the mapped `VideoFrame` straight to the slot and the GPU
upload reads out of decoder memory. That removes a full-resolution memcpy per
frame and the ~3 MiB of padded scratch buffers it needed. The `PlanarFrame`
trait is what keeps this available to a non-GStreamer backend.

An earlier version padded every row to 256 bytes on the way out of GStreamer.
That was defensive, not required, and it cost a copy of every frame.

**The video plane is ours.** iced hands the primitive a render pass already
scissored to the widget bounds, so a single full-screen triangle in clip space
lands exactly on the widget. Aspect ratio is handled by shrinking that triangle
on one axis — letterbox bars are simply where the shader does not draw, which
means they cost nothing and always match the app background.

**Colour is handled explicitly.** The shader converts BT.709 limited-range YUV
and, when the render target is an sRGB format, converts to linear so the
hardware's re-encode does not double-apply the transfer. This is the seam where
HDR tone mapping goes later.

**The audio sink is chosen for its clock, then for its quality.** This is where
the two pull against each other, and timing has to win.

The first version picked `pipewiresink`, to talk to PipeWire directly and skip
the Pulse compatibility layer — one fewer place a hidden resample could happen.
That reasoning was written down here as a virtue. It was also wrong:
`pipewiresink` provides no clock, so the pipeline ran on `GstSystemClock` while
the sound card consumed samples at its own crystal rate. Two oscillators tens of
parts per million apart are inaudible for a second and a visible lip-sync error
after ten minutes. That was this player's progressive A/V drift, and it took
measuring the whole chain to find, because everything downstream was blameless:
the decoder's own A/V gap was flat, frames arrived 0.2 ms after being sent with
no backlog, and the renderer displayed 24-25 fps.

Sinks are now tried in order of whether they provide a clock — `pulsesink`
(`GstPulseSinkClock`), `alsasink`, `autoaudiosink`, and `pipewiresink` only as a
last resort, with a warning that sync may wander. On any modern desktop
`pipewire-pulse` answers `pulsesink`, so the audio still reaches PipeWire and the
pipeline is driven by the device actually playing it.

**The conversion chain lives in `audio-filter`, not around the sink.** Wrapping
the sink in a bin hides the sink's clock from the pipeline, which was the other
half of the same bug. Quality is unchanged: `audioconvert` -> `audioresample`
(quality 10, not the default 4) -> an `F32LE` caps filter, so nothing quantises
until the sink makes the single conversion to the device format, with TPDF
dither. When the file's rate already matches the device — 48 kHz here — no
resampling happens at all.

**Sound effects are a pad probe on that chain's output, not more elements.**
`engine::dsp::AudioFx` runs on the float buffers leaving the caps filter:
centre-channel lift (read from the caps' `channel-mask`), a soft-knee compressor
for night mode, the volume above 100%, and a limiter last. It has no lookahead,
so it adds no latency and the audio sink's clock is untouched. When nothing is
enabled the probe returns without mapping the buffer. Up to 100% volume is still
playbin's own `volume`; only the boost goes through the limiter, because
playbin's volume above 1.0 is plain multiplication and clips.

**The best audio track is chosen, not the first one.** playbin3 defaults to the
first stream of each kind. On a `StreamCollection` message we score audio streams
by channel count, then bitrate, and send a `SelectStreams` event. On the test
file that picks E-AC-3 5.1 at 640 kb/s over the AAC stereo track. Verified:
`[diag] video: vah264dec (hardware) - audio: avdec_eac3`.

**Subtitles are delivered to us, not burned into the picture.** playbin's overlay
scales the video up to the display resolution first, to keep the text crisp. On a
2880x1800 panel that turned a 1080p file into 2880x1800 frames — 2.5x the pixels,
every frame, for nothing. Setting `text-sink` to an appsink hands us the cue text
instead; we draw it, and the video stays at its native size. Verified: cues
arrive and the diagnostic still reports 1920x1080 throughout.

**iced 0.14 pins wgpu 27.** The shader widget hands us *iced's* device and
queue, so our `wgpu` dependency must be the same major version or the types will
not unify. Do not bump `wgpu` independently of `iced`.

**The best audio track is chosen, then choosable.** On a `StreamCollection`
message every audio and subtitle stream is described from its caps and tags, the
best audio is selected by channel count then bitrate, and the whole list goes to
the UI. `select_track` rewrites one slot of the selection and re-sends
`SelectStreams`, so switching audio does not deselect the subtitles.

**Clip export shells out to `ffmpeg`, and that is a deliberate retreat.** The
GStreamer version of this — `filesrc ! parsebin ! mux ! filesink` with a seek —
does not work, and the reason is structural rather than a bug worth chasing:

- A lossless cut needs a *flushing* seek, and a flushing seek cannot pass through
  a muxer. `collectpads` refuses to forward it ("forwarding flush start failed")
  and the muxer never recovers.
- Sending the seek to the pipeline is refused outright, because the muxer is what
  the event reaches first. Sending it to a demuxer source pad is accepted — and
  then the flush still wedges the muxer downstream.
- Seeking after the pipeline reaches PLAYING is worse: the muxer has already
  written its header.
- Waiting for PAUSED before seeking deadlocks. A muxing pipeline cannot finish
  prerolling, because the muxer wants a buffer on every pad while upstream is
  still blocked on the sink prerolling. `gst-launch` avoids this only by going
  straight to PLAYING.

Doing it properly means re-timestamping every buffer after the seek, which is
what GStreamer Editing Services exists for — a library, not a function.
`ffmpeg -c copy` does it correctly in one command, keeps every track, and leaves
the playback pipeline untouched. The cost is a runtime dependency on the
`ffmpeg` binary, which `install.sh` installs.

A consequence worth stating in the UI, and stated there: a stream copy can only
cut on a keyframe, so a clip starts at the keyframe at or before the in point.

**Hardware decode is detected by vendor prefix, not by a list of names.** A fixed
list got this wrong: `vavp8dec` was reported as software purely because the list
happened not to mention VP8, and the readout in the control bar is the only way a
user can tell whether their fans are about to spin up. Matching the prefixes
covers the whole VA-API, NVIDIA, D3D11/12, VideoToolbox, V4L2-stateless, QSV and
AMF families, including decoders that do not exist yet. Three tests pin the
behaviour, including that `vaapipostproc` is not a decoder.

## Footprint

Measured on the test file (1080p H.264, E-AC-3 5.1), debug build: **~226 MiB
RSS** steady while playing. Most of that is not ours — the Mesa/Vulkan driver
and the H.264 decoded-picture buffer, whose size the stream dictates, dominate.
What we control:

- One frame held in the slot, borrowed from the decoder rather than copied.
- `appsink` capped at 2 buffers with `drop=true`, so a slow GPU cannot make the
  pipeline accumulate frames.
- `buffer-size` / `buffer-duration` capped at 16 MiB / 5 s, so a network source
  cannot buffer without limit.
- Two GPU textures for the current resolution, reallocated only when it changes.

The release profile is built for size and speed: `opt-level = 3` (the hot paths
are per-frame, so size-optimising them would be the wrong trade), fat LTO,
`codegen-units = 1`, symbols stripped, and `panic = "abort"` — unwinding across
GStreamer's C callbacks is undefined behaviour anyway, so a panic there already
ends the process and the unwinding tables were dead weight. One consequence:
`cargo test --release` cannot build; tests run on the dev profile.

## Deliberately not done yet

- **Zero-copy import.** Frames take one CPU copy out of GStreamer and one upload.
  At 1080p60 that is ~190 MB/s, which is nothing; at 4K60 it is ~750 MB/s, which
  is noticeable. True zero-copy needs dmabuf on Linux, D3D11 shared handles on
  Windows and IOSurface on macOS — three platform implementations behind the
  same interface.
- **Process isolation.** Every demuxer and decoder here is C, and media files are
  untrusted input. The plan is to run the pipeline in a sandboxed child
  (Landlock + seccomp on Linux, job objects on Windows) with frames crossing over
  shared memory. This shapes `PlaybackEngine`, so it is a change to make before
  the trait grows more methods.
- **Buffered range on the scrub bar.** The bar is an `iced` slider today, which
  can show played/remaining but not a third band. Needs a custom widget.
- **Chrome fade animation.** Chrome currently shows and hides; it does not fade.
- **Image-based subtitles.** Text formats (SubRip, ASS/SSA) render; PGS and
  VobSub are bitmaps and need decoding to RGBA before they can be drawn.
- **Subtitle styling.** iced has no text outline, so cues sit on a translucent
  plate rather than the outlined text the wireframes show.
- **Bitstream passthrough.** E-AC-3/DTS passthrough to an AV receiver over
  HDMI/SPDIF is untouched; everything is decoded and mixed locally. Worth doing
  only if you have a receiver to feed.
- **Frame-exact clips.** The cut is keyframe-bound because nothing is
  re-encoded. Exactness would need to re-encode the first group of pictures —
  a "smart cut", and a separate feature.
- **Buffered range on the scrub bar.** The bar is an `iced` slider, which shows
  played and remaining but not a third band. Needs a custom widget.

## Errors

Every pipeline error goes two places: an in-window chip with a **Copy** button,
and stderr. An error a user cannot copy out of the window is an error they
cannot report, which is exactly what happened the first time one appeared.

The subtitle sink deliberately declares no caps. Constraining it to `text/x-raw`
means `playsink` cannot connect a bitmap subtitle track (PGS, VobSub) at all,
and it fails the entire file with a `GstPlaySink` error rather than simply not
showing subtitles. It now accepts anything and skips what cannot be drawn.

## Diagnostics

`MYVID_DIAG=1 myvid <file>` reports, once a second: frame count, resolution,
pixel format and plane strides; the video and audio decoders actually chosen and
whether video decode is hardware-accelerated; which clock drives the pipeline;
the measured gap between sound and picture; how far frame delivery is behind the
decoder and its transit time; and how many frames reached the screen.

That set is not decoration — it exists because A/V drift was chased three times
by reasoning and found once by measuring. Each number rules out one link in the
chain, which is what turned "sometimes out of sync" into "the pipeline is on the
wrong clock".

```
[diag] clock: GstPulseSinkClock
[diag] a/v gap +0.014s (sound 14.47s, picture 14.46s)
[diag] delivery: 255 frames, 0 behind decoder, transit 0.2 ms avg
[diag] displayed 24 frames in the last second
[diag] decoder can read $HOME: no
```

## Icon

`assets/icon-source.png` is the artwork; `install.sh` derives nothing at install
time and simply copies the pre-rendered `assets/myvid-<size>.png` files into the
hicolor theme. The window itself carries no embedded icon: Wayland matches a
window to a desktop entry by application id, which `window::Settings` sets to
`myvid`, so the icon costs zero bytes in the binary.

## Design

Wireframes for both directions live in `docs/design/` as `.dc.html` artboards.
`Main.dc.html` is the Modern direction currently implemented.
