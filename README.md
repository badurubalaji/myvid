<p align="center">
  <img src="assets/myvid-256.png" width="160" alt="myvid logo">
</p>

<h1 align="center">myvid</h1>

<p align="center">
  A video player: GStreamer decodes, wgpu presents, iced draws the chrome.<br>
  Hardware decoding, network streams, and every file decoded in its own sandbox.
</p>

<p align="center">
  <a href="https://github.com/badurubalaji/myvid/releases/latest"><b>Download</b></a> ·
  <a href="#install">Install</a> ·
  <a href="#use">Use</a> ·
  <a href="THIRD-PARTY-LICENSES.md">Licenses</a>
</p>

## Install

For Ubuntu 24.04 or newer, Debian 13, Linux Mint 22, Pop!_OS 24.04 and other
64-bit (amd64) distributions based on them.

**One line** — downloads the latest release, verifies its checksum, and installs
it with apt:

```sh
curl -fsSL https://raw.githubusercontent.com/badurubalaji/myvid/main/get.sh | bash
```

**Or by hand** — download
[`myvid_amd64.deb`](https://github.com/badurubalaji/myvid/releases/latest/download/myvid_amd64.deb)
and install it:

```sh
sudo apt install ./myvid_amd64.deb
```

apt pulls in GStreamer, the codecs, ffmpeg and the Vulkan loader. Then launch
**myvid** from your applications menu, or run `myvid film.mkv`.

To remove it:

```sh
sudo apt remove myvid
```

### Requirements

- 64-bit x86 Linux with glibc 2.39 or newer
- A GPU with a Vulkan driver (Intel, AMD, or NVIDIA with its proprietary driver)
- Wayland or X11
- Linux 5.13 or newer for the decoder sandbox; older kernels still play, unconfined

On Intel GPUs, `sudo apt install intel-media-va-driver-non-free` (Ubuntu
*multiverse*) enables hardware decoding for more codecs.

## Build from source

Needs a Rust toolchain (1.92 or newer, from [rustup](https://rustup.rs)) on a
Debian or Ubuntu system:

```sh
git clone https://github.com/badurubalaji/myvid.git
cd myvid
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

### Making a release

```sh
cargo build --release --locked
packaging/licenses.sh                  # refresh THIRD-PARTY-LICENSES.md if Cargo.lock changed
packaging/build-deb.sh                 # → dist/myvid_<version>_amd64.deb, myvid_amd64.deb, SHA256SUMS
gh release create v<version> dist/myvid_amd64.deb dist/myvid_*_amd64.deb dist/SHA256SUMS
```

Bump `version` in `Cargo.toml` and add a `<release>` entry to
`packaging/linux/io.github.badurubalaji.myvid.metainfo.xml` first. Keep the
version-less `myvid_amd64.deb` asset: the one-line installer and the download
link above point at `releases/latest/download/myvid_amd64.deb`.

## Use

```sh
myvid /path/to/film.mkv
myvid https://example.com/stream.m3u8
myvid                       # opens with a drop target and a file picker
```

Drag a file onto the window at any time, whether or not something is already
playing.

For a stream, press `Ctrl`+`L` or use **Open URL** on the opening screen, then
paste and press Enter. HTTP, HTTPS, HLS, DASH, RTSP, RTMP, UDP and SRT all work.

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
| `Ctrl`+`L` | open a stream URL |

The controls fade after 2.4 s of a still pointer while playing, and come back on
any movement, keypress or state change.

## Tracks

`T` opens a panel listing every audio and subtitle stream with what it actually
is — codec, channels, sample rate, bitrate — not just its language. The best
audio track is selected automatically on load, by channel count and then
bitrate, rather than whichever the container happened to list first. Playback
continues while the panel is open, so you can hear the track you just chose.

## Sound

The same panel has two switches, both off by default:

- **Clear dialogue** lifts the centre channel of 5.1 and 7.1 tracks, where films
  put the voices, by 6 dB against everything else. Stereo tracks are unchanged.
- **Night mode** compresses the dynamic range, so quiet scenes come up and loud
  ones come down.

Volume goes to 150%. Everything above 100%, and anything the switches push past
full scale, goes through a limiter, so it gets louder without clipping. With
both switches off and volume at or below 100%, the samples are not touched.

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

## Isolation

Each file is decoded in its own process, confined with Landlock to that one
file. A film that compromises a demuxer gets a process that can read the film it
was opened for and nothing else — not your documents, your keys, your browser
profile, nor even the other files in the same folder — and can write nowhere
outside the GPU and the audio socket.

```
myvid decoder: landlock: enforced
[diag] decoder can read $HOME: no
```

If the kernel is too old for Landlock the decoder still runs in its own process
and says so rather than pretending. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for what stays reachable and why.

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

## License

myvid is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

It builds on a great deal of other people's work. The Rust crates compiled into
the binary, and the system libraries it uses at run time — GStreamer, GLib,
FFmpeg, Mesa and the Vulkan loader — are listed with their licenses and full
license texts in [THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md). The same
file ships in the package at `/usr/share/doc/myvid/`.
