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
    for path in ["/usr/lib", "/dev/dri", "/dev/null"] {
        let path = Path::new(path);
        if !path.exists() {
            continue;
        }
        assert!(can_read(path), "{} should stay reachable", path.display());
    }
}

/// A decoder is confined to the one file it was started for, and can read no
/// other — not even its neighbour in the same directory.
#[test]
fn only_the_one_file_it_was_given() {
    // Somewhere the policy denies wholesale, so the only thing making a file
    // readable is having been named.
    let dir = std::env::temp_dir().join("myvid-sandbox-test");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let granted = dir.join("granted.mkv");
    let neighbour = dir.join("neighbour.mkv");
    std::fs::write(&granted, b"x").expect("write");
    std::fs::write(&neighbour, b"x").expect("write");

    let reachable = |path: &Path| {
        Command::new(env!("CARGO_BIN_EXE_myvid"))
            .arg("--sandbox-selftest")
            .arg(path)
            .env("MYVID_SELFTEST_ALLOW", &granted)
            .output()
            .expect("selftest runs")
            .status
            .success()
    };

    assert!(reachable(&granted), "the named file should be readable");
    assert!(
        !reachable(&neighbour),
        "a file it was never given should be denied, even next door"
    );

    let _ = std::fs::remove_dir_all(&dir);
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
