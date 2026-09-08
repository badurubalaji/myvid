# myvid

A video player: GStreamer decodes, wgpu presents, iced draws the chrome.

## Install

```sh
sudo ./install.sh          # system packages, build, install, reclaim build space
```

Root is needed only for `apt`. The build always runs as the invoking user, so
nothing in `~/.cargo` or `./target` ends up owned by root. Individual steps:

```sh
sudo ./install.sh deps     # system packages only
     ./install.sh build    # build only, no root
     ./install.sh install  # install to ~/.local, no root
     ./install.sh reclaim  # delete build artifacts, keep the installed binary
sudo ./install.sh uninstall
```

The binary lands in `~/.local/bin/myvid`, with a desktop entry and icons under
`~/.local/share`. `target/` is deleted after a successful install — a full build
tree is several GB and nothing needs it afterwards.

## Use

```sh
myvid /path/to/film.mkv
myvid https://example.com/stream.m3u8
myvid                       # opens with a drop target and a file picker
```

Drag a file onto the window at any time, whether or not something is already
playing.

| Key | |
|---|---|
| `Space` / `K` | play / pause |
| `←` `→` | seek ±5 s |
| `J` `L` | seek ±10 s |
| `↑` `↓` | volume ±5% |
| `[` `]` | speed down / up |
| `M` | mute |
| `F` | fullscreen (`Esc` leaves) |
| `T` | tracks and speed panel |
| `I` `O` | mark clip in / out |
| `E` | export the marked clip |
| `Ctrl`+`O` | open a file |

The controls fade after 2.4 s of a still pointer while playing, and come back on
any movement, keypress or state change.

## Tracks

`T` opens a panel listing every audio and subtitle stream with what it actually
is — codec, channels, sample rate, bitrate — not just its language. The best
audio track is selected automatically on load, by channel count and then
bitrate, rather than whichever the container happened to list first. Playback
continues while the panel is open, so you can hear the track you just chose.

## Clips

Mark a range with `I` and `O`, pick MKV or MP4, and `E` writes it out. The copy
is lossless: no re-encoding, so it takes about as long as reading the bytes.

Because nothing is re-encoded, the cut can only start on a keyframe, so the clip
begins at the keyframe at or before your in point — for a typical web encode
that is within a few seconds. A frame-exact cut would mean re-encoding, which is
a different feature and is not built.

Clip export shells out to `ffmpeg` (installed by `install.sh`). The reason is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md); the short version is that a
flushing seek cannot pass through a GStreamer muxer.

## Formats

Containers: MKV, MP4, WebM, AVI, MOV, TS, M2TS, FLV, OGV, WMV.
Video: H.264, HEVC, AV1, VP9, VP8, MPEG-2, MPEG-4, ProRes, Theora, VC-1.
Audio: whatever `gstreamer1.0-libav` and the plugin set provide.

Hardware decode is used where the GPU offers it (VA-API on Intel and AMD). The
control bar says `GPU decode` or `CPU decode` so you can tell which you got.

Verified on an Intel Iris Xe (Tiger Lake):

| File | Decoder | |
|---|---|---|
| MKV · H.264 | `vah264dec` | hardware |
| MKV · HEVC | `vah265dec` | hardware |
| MKV · AV1 | `vaav1dec` | hardware |
| MP4 · H.264 | `vah264dec` | hardware |
| MOV · H.264 | `vah264dec` | hardware |
| TS · H.264 | `vah264dec` | hardware |
| WebM · VP9 | `vavp9dec` | hardware |
| WebM · VP8 | `vavp8dec` | hardware |
| MOV · ProRes | `avdec_prores` | software |
| AVI · MPEG-4 | `avdec_mpeg4` | software |

ProRes and MPEG-4 have no fixed-function block on this GPU, so falling back to
software there is correct rather than a failure.

Subtitles: SubRip and ASS/SSA render as text. PGS and VobSub are bitmap formats
and are not drawn yet.

## Diagnostics

```sh
MYVID_DIAG=1 myvid film.mkv
```

Reports frame throughput, resolution, pixel format and plane strides once a
second, plus the video and audio decoders actually chosen:

```
[diag] video: vah264dec (hardware) · audio: avdec_eac3
[diag] 1133 frames · 1920x1080 · NV12 · strides [1920, 1920]
```

## Layout

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how the three layers fit
together and which trade-offs were made deliberately. Interface wireframes for
both design directions are in `docs/design/`.
