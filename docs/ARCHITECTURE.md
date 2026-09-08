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

**The decoder is never told where anything is.** The player opens the file and
passes the descriptor; the decoder plays `fd://N`. That is what makes the
restriction meaningful — a decoder that never needs to open a path can be denied
the ability to open one.

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

**The audio path is short and stays in float.** `audioconvert` -> `audioresample`
(quality 10, not the default 4) -> a `F32LE` caps filter -> `pipewiresink`, falling
back to `pulsesink` then `autoaudiosink`. Nothing quantises until the sink does
the single conversion to the device format, with TPDF dither. Going straight to
PipeWire rather than through the Pulse compatibility socket removes one layer
that can silently resample. When the file's rate already matches the device — 48
kHz here — no resampling happens at all.

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

`MYVID_DIAG=1 myvid <file>` reports, once a second, the frame count, resolution,
pixel format and plane strides, plus the video and audio decoders in use and
whether video decode is hardware-accelerated. It is the only way to tell "no
frames" apart from "frames, but nothing on screen".

## Icon

`assets/icon-source.png` is the artwork; `install.sh` derives nothing at install
time and simply copies the pre-rendered `assets/myvid-<size>.png` files into the
hicolor theme. The window itself carries no embedded icon: Wayland matches a
window to a desktop entry by application id, which `window::Settings` sets to
`myvid`, so the icon costs zero bytes in the binary.

## Design

Wireframes for both directions live in `docs/design/` as `.dc.html` artboards.
`Main.dc.html` is the Modern direction currently implemented.
