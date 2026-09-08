//! The sandbox has to be checked from outside: Landlock cannot be lifted once
//! applied, so a process that confines itself cannot then report on what it
//! could have reached. `myvid --sandbox-selftest <path>` confines and then tries
//! to open one path, which is what these drive.

use std::path::Path;
use std::process::Command;

fn can_read(path: &Path) -> bool {
    let output = Command::new(env!("CARGO_BIN_EXE_myvid"))
        .arg("--sandbox-selftest")
        .arg(path)
        .output()
        .expect("selftest runs");
    output.status.success()
}

fn confinement() -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_myvid"))
        .arg("--sandbox-selftest")
        .arg("/etc/hostname")
        .output()
        .expect("selftest runs");
    String::from_utf8_lossy(&output.stderr).trim().to_owned()
}

#[test]
fn the_sandbox_is_actually_enforced() {
    let status = confinement();
    assert!(
        status.contains("enforced"),
        "expected an enforced sandbox, got {status:?}"
    );
}

#[test]
fn playback_keeps_what_it_needs() {
    for path in ["/usr/lib", "/etc", "/dev/dri", "/dev/null"] {
        let path = Path::new(path);
        if !path.exists() {
            continue;
        }
        assert!(can_read(path), "{} should stay reachable", path.display());
    }
}

/// The point of the exercise. A decoder parsing a hostile file must not be able
/// to read the things worth stealing.
#[test]
fn the_users_files_are_out_of_reach() {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let Some(home) = home else {
        eprintln!("skipped: no HOME");
        return;
    };

    assert!(!can_read(&home), "$HOME should be denied");
    for name in [".bashrc", ".ssh", ".config"] {
        let path = home.join(name);
        if path.exists() {
            assert!(!can_read(&path), "{} should be denied", path.display());
        }
    }
    assert!(!can_read(Path::new("/tmp")), "/tmp should be denied");
}
