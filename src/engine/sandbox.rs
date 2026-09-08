//! Confining the decode process.
//!
//! Every demuxer and decoder below us is C code parsing untrusted input, which
//! is the oldest remote-code-execution surface there is. Rust around it buys
//! nothing. What does buy something is making the process that runs it unable to
//! reach anything worth stealing: after this is applied, a compromised decoder
//! cannot open your documents, your keys, or your browser profile, and cannot
//! write anywhere outside the few paths playback genuinely needs.
//!
//! Timing matters. GStreamer builds its plugin registry under `$HOME/.cache` and
//! reads a great deal of the filesystem while doing it, so this must be applied
//! *after* `gst::init()` and after the pipeline's elements exist — otherwise the
//! restriction either fails the registry build or has to be so wide it protects
//! nothing.

use std::path::Path;

/// What the sandbox managed to do, for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confinement {
    /// Fully enforced.
    Enforced,
    /// The kernel supports only part of what was asked for.
    Partial(String),
    /// Not available — the process runs unconfined.
    Unavailable(String),
}

impl Confinement {
    pub fn describe(&self) -> String {
        match self {
            Confinement::Enforced => "landlock: enforced".to_owned(),
            Confinement::Partial(why) => format!("landlock: partial ({why})"),
            Confinement::Unavailable(why) => format!("landlock: unavailable ({why})"),
        }
    }

}

/// Readable and executable: shared libraries, GStreamer plugins loaded lazily
/// when a codec first appears, fonts, timezone and locale data.
const READABLE: &[&str] = &[
    "/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc", "/proc", "/sys",
];

/// Readable and writable: the GPU, the audio server socket, and the handful of
/// devices any process needs.
const WRITABLE: &[&str] = &[
    "/dev/dri",
    "/dev/null",
    "/dev/zero",
    "/dev/urandom",
    "/dev/random",
    "/dev/shm",
];

/// Confine this process to what playback needs.
///
/// Deliberately absent from both lists: `$HOME`, `/tmp`, `/media`, `/mnt`,
/// `/run` outside the session's own runtime directory, and every other user's
/// files. The media file itself is not here either — it arrives as an already
/// open file descriptor from the parent, so the decoder never needs the ability
/// to open a path at all.
#[cfg(target_os = "linux")]
pub fn confine() -> Confinement {
    use landlock::{
        ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
        path_beneath_rules,
    };

    let abi = ABI::V5;
    let read_only = AccessFs::from_read(abi);
    let read_write = AccessFs::from_all(abi);

    // The audio server's socket is the one place under the user's account the
    // decoder legitimately needs to write.
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    let mut writable: Vec<&Path> = WRITABLE.iter().map(Path::new).collect();
    if let Some(runtime) = runtime_dir.as_deref() {
        writable.push(runtime);
    }

    // `path_beneath_rules` quietly drops paths that do not exist on this system,
    // which is right: one fewer thing to allow, not an error.
    let build = || -> Result<_, landlock::RulesetError> {
        Ruleset::default()
            .handle_access(read_write)?
            .create()?
            .add_rules(path_beneath_rules(READABLE.iter().map(Path::new), read_only))?
            .add_rules(path_beneath_rules(writable.iter().copied(), read_write))?
            .restrict_self()
    };

    match build() {
        Ok(status) => match status.ruleset {
            RulesetStatus::FullyEnforced => Confinement::Enforced,
            RulesetStatus::PartiallyEnforced => {
                Confinement::Partial("kernel supports only some restrictions".to_owned())
            }
            RulesetStatus::NotEnforced => {
                Confinement::Unavailable("kernel refused the ruleset".to_owned())
            }
        },
        Err(err) => Confinement::Unavailable(err.to_string()),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn confine() -> Confinement {
    Confinement::Unavailable("only implemented for Linux".to_owned())
}

/// Prove the sandbox does what it claims: confine, then try to read a path.
///
/// Landlock cannot be lifted once applied, so this has to run in its own
/// process. `myvid --sandbox-selftest <path>` is that process, and the test
/// suite drives it.
pub fn selftest(path: &Path) -> ! {
    let confinement = confine();
    eprintln!("{}", confinement.describe());

    let readable = std::fs::File::open(path).is_ok();
    println!("{}", if readable { "allowed" } else { "denied" });
    std::process::exit(if readable { 0 } else { 1 })
}
