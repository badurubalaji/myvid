//! The conversation between the player and its decode process.
//!
//! One `SOCK_SEQPACKET` socket pair carries everything. Sequenced packets
//! preserve message boundaries, so each `sendmsg` is one message and needs no
//! length framing, and any descriptor attached to it — the media file, or the
//! shared frame buffer — arrives with the message it belongs to.

use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use anyhow::{Context, Result};
use bincode::{Decode, Encode};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketFlags, SocketType,
};

use super::{AudioEffects, MediaInfo, State, Track, TrackKind};

/// Generous for a track listing; nothing here carries pixels.
const MAX_MESSAGE: usize = 256 * 1024;

/// Player to decoder.
#[derive(Encode, Decode, Debug, Clone)]
pub enum Request {
    /// Confine this decoder to one file, then play it.
    ///
    /// The path arrives before the sandbox closes, and is the only thing outside
    /// the system directories the process will ever be able to open — so a
    /// decoder is confined to the single film it was started for. `fdsrc`, which
    /// needs no path at all, mishandles seeks near the end of a file; `filesrc`
    /// does not, and this keeps the confinement without paying that price.
    PlayPath(String),
    /// Play a network source, which the decoder must open itself.
    PlayUri(String),
    Resume,
    Pause,
    Seek(u64),
    Volume(f64),
    AudioEffects(AudioEffects),
    Rate(f64),
    SelectTrack {
        kind: TrackKind,
        id: Option<String>,
    },
    Shutdown,
}

/// Decoder to player.
#[derive(Encode, Decode, Debug, Clone)]
pub enum Notice {
    /// Sent once at startup, reporting how well the decoder is confined.
    Ready { confinement: String },
    State(State),
    Duration(u64),
    Position(u64),
    Buffering(u8),
    Loaded(MediaInfo),
    Tracks(Vec<Track>),
    /// A new shared frame buffer, attached as a descriptor. Sent whenever the
    /// stream's resolution changes.
    Surface(Layout),
    /// Slot `slot` now holds frame `generation`, handed over at `sent_ns`
    /// (nanoseconds since the epoch, so both processes read the same clock).
    Frame {
        slot: u32,
        generation: u64,
        sent_ns: u64,
    },
    Subtitle {
        text: Option<String>,
        start: u64,
        end: u64,
    },
    Eos,
    Failed(String),
}

/// How pixels are arranged in the shared buffer.
#[derive(Encode, Decode, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub y_stride: u32,
    pub uv_stride: u32,
    /// Bytes per slot, including both planes.
    pub slot_bytes: u64,
    /// How many frames the buffer holds before wrapping.
    pub slots: u32,
}

impl Layout {
    pub fn new(width: u32, height: u32, y_stride: u32, uv_stride: u32, slots: u32) -> Self {
        let luma = y_stride as u64 * height as u64;
        let chroma = uv_stride as u64 * height.div_ceil(2) as u64;
        Self {
            width,
            height,
            y_stride,
            uv_stride,
            slot_bytes: luma + chroma,
            slots,
        }
    }

    pub fn luma_bytes(&self) -> usize {
        self.y_stride as usize * self.height as usize
    }

    pub fn chroma_bytes(&self) -> usize {
        self.uv_stride as usize * self.height.div_ceil(2) as usize
    }

    pub fn total_bytes(&self) -> usize {
        self.slot_bytes as usize * self.slots as usize
    }
}

/// One end of the socket pair.
pub struct Channel {
    socket: OwnedFd,
}

impl Channel {
    pub fn pair() -> Result<(Channel, Channel)> {
        let (a, b) = rustix::net::socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::empty(),
            None,
        )
        .context("creating the decoder socket")?;

        Ok((Channel { socket: a }, Channel { socket: b }))
    }

    /// A channel with nobody on the other end.
    ///
    /// Somewhere to point before the first decoder exists. Sends fail and reads
    /// return nothing, which is exactly what "no decoder yet" should look like.
    pub fn orphan() -> Result<Channel> {
        let (ours, theirs) = Channel::pair()?;
        drop(theirs);
        Ok(ours)
    }

    /// # Safety
    /// The descriptor must be a connected `SOCK_SEQPACKET` socket.
    pub fn from_fd(socket: OwnedFd) -> Self {
        Channel { socket }
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }

    /// Send, waiting for room if the socket is full.
    pub fn send<T: Encode>(&self, message: &T, attach: Option<BorrowedFd<'_>>) -> Result<()> {
        self.send_inner(message, attach, false).map(|_| ())
    }

    /// Send only if it can go immediately; `false` means the socket was full.
    ///
    /// For a frame or position notice that is the right answer: the next one
    /// supersedes it, and blocking here stalls the decode loop — including its
    /// ability to notice the player has gone away.
    pub fn try_send<T: Encode>(&self, message: &T) -> Result<bool> {
        self.send_inner(message, None, true)
    }

    fn send_inner<T: Encode>(
        &self,
        message: &T,
        attach: Option<BorrowedFd<'_>>,
        non_blocking: bool,
    ) -> Result<bool> {
        let bytes = bincode::encode_to_vec(message, bincode::config::standard())
            .context("encoding a message")?;

        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        let attached = attach.map(|fd| [fd]);
        if let Some(fds) = attached.as_ref() {
            control.push(SendAncillaryMessage::ScmRights(fds));
        }

        let flags = if non_blocking {
            SendFlags::DONTWAIT
        } else {
            SendFlags::empty()
        };

        loop {
            match rustix::net::sendmsg(&self.socket, &[IoSlice::new(&bytes)], &mut control, flags) {
                Ok(_) => return Ok(true),
                // A signal is not a failure; the message simply has not gone yet.
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::AGAIN) | Err(rustix::io::Errno::WOULDBLOCK)
                    if non_blocking =>
                {
                    return Ok(false)
                }
                Err(err) => return Err(anyhow::Error::new(err).context("sending a message")),
            }
        }
    }

    /// Returns `None` when the other end has gone away.
    pub fn recv<T: Decode<()>>(&self) -> Result<Option<(T, Option<OwnedFd>)>> {
        let mut buffer = vec![0u8; MAX_MESSAGE];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = RecvAncillaryBuffer::new(&mut space);

        let received = loop {
            match rustix::net::recvmsg(
                &self.socket,
                &mut [IoSliceMut::new(&mut buffer)],
                &mut control,
                RecvFlags::empty(),
            ) {
                Ok(received) => break received,
                // Interrupted by a signal. Treating this as "the peer is gone"
                // tears down a healthy connection and deadlocks the other end.
                Err(rustix::io::Errno::INTR) => continue,
                Err(err) => return Err(anyhow::Error::new(err).context("receiving a message")),
            }
        };

        if received.bytes == 0 {
            return Ok(None);
        }

        let mut attached = None;
        for message in control.drain() {
            if let RecvAncillaryMessage::ScmRights(mut fds) = message {
                attached = fds.next();
            }
        }

        let (value, _) = bincode::decode_from_slice(&buffer[..received.bytes], bincode::config::standard())
            .context("decoding a message")?;

        Ok(Some((value, attached)))
    }
}
