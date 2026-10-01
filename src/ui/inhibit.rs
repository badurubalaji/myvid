//! Keeping the desktop awake while a video plays.
//!
//! Watching a film means not touching the keyboard or mouse for two hours, and
//! the session reads that as idle: the screen blanks and the machine suspends
//! mid-scene. Wayland's idle-inhibit protocol would be the obvious tool, but
//! GNOME does not implement it, so this asks the session over D-Bus instead:
//!
//! 1. `org.gnome.SessionManager` — GNOME, blocks both idle and suspend.
//! 2. `org.freedesktop.ScreenSaver` — KDE, Xfce, Cinnamon, MATE and others.
//! 3. The inhibit portal, for anything else.
//!
//! The portal comes last even though it is the "standard" route: it names the
//! caller by the app ID it reads from the process's systemd scope, which is
//! empty when myvid is started from a terminal, and gnome-session rejects an
//! empty app ID while the portal still reports success. Asking the session
//! manager directly lets us name ourselves.
//!
//! D-Bus is asynchronous and the UI's update loop is not, so the inhibition
//! lives on a thread of its own that follows a single "should be awake" flag.

use ashpd::desktop::inhibit::{InhibitFlags, InhibitProxy};
use ashpd::desktop::Request;
use tokio::sync::watch;
use zbus::Connection;

const APP_ID: &str = "myvid";
const REASON: &str = "Playing a video";

pub struct SleepInhibitor {
    wanted: watch::Sender<bool>,
}

impl SleepInhibitor {
    pub fn spawn() -> Self {
        let (wanted, receiver) = watch::channel(false);
        let started = std::thread::Builder::new()
            .name("sleep-inhibitor".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(follow(receiver)),
                    Err(err) => eprintln!("myvid: sleep inhibitor: no runtime: {err}"),
                }
            });
        if let Err(err) = started {
            eprintln!("myvid: sleep inhibitor: thread did not start: {err}");
        }
        Self { wanted }
    }

    /// Hold the inhibition while `awake` is true, release it otherwise. Cheap to
    /// call on every event: only a change reaches D-Bus.
    pub fn set(&self, awake: bool) {
        self.wanted.send_if_modified(|current| {
            let changed = *current != awake;
            *current = awake;
            changed
        });
    }
}

/// An inhibition currently in force, and what it takes to lift it.
enum Hold {
    Gnome(u32),
    ScreenSaver(u32),
    Portal(Request<()>),
}

async fn follow(mut wanted: watch::Receiver<bool>) {
    // One connection for the life of the app. Both session managers also drop
    // an inhibition when the connection that took it closes, so quitting — or
    // crashing — never leaves the machine unable to sleep.
    let bus = match Connection::session().await {
        Ok(bus) => Some(bus),
        Err(err) => {
            eprintln!("myvid: no session bus, the screen may blank during playback: {err}");
            None
        }
    };
    let mut held: Option<Hold> = None;
    // Report a failure once, not on every pause and play.
    let mut warned = false;

    loop {
        let awake = *wanted.borrow_and_update();

        if awake && held.is_none() {
            match acquire(bus.as_ref()).await {
                Ok(hold) => held = Some(hold),
                Err(err) if !warned => {
                    eprintln!("myvid: cannot keep the screen awake: {err}");
                    warned = true;
                }
                Err(_) => {}
            }
        } else if !awake {
            if let Some(hold) = held.take() {
                release(bus.as_ref(), hold).await;
            }
        }

        // The sender lives in the UI; it going away means the app is closing.
        if wanted.changed().await.is_err() {
            return;
        }
    }
}

async fn acquire(bus: Option<&Connection>) -> Result<Hold, String> {
    let mut failures = Vec::new();

    if let Some(bus) = bus {
        // Inhibit(app_id, toplevel_xid, reason, flags); 4 = suspend, 8 = idle.
        let reply = bus
            .call_method(
                Some("org.gnome.SessionManager"),
                "/org/gnome/SessionManager",
                Some("org.gnome.SessionManager"),
                "Inhibit",
                &(APP_ID, 0u32, REASON, 4u32 | 8u32),
            )
            .await;
        match cookie(reply) {
            Ok(cookie) => return Ok(Hold::Gnome(cookie)),
            Err(err) => failures.push(format!("gnome-session: {err}")),
        }

        let reply = bus
            .call_method(
                Some("org.freedesktop.ScreenSaver"),
                "/org/freedesktop/ScreenSaver",
                Some("org.freedesktop.ScreenSaver"),
                "Inhibit",
                &(APP_ID, REASON),
            )
            .await;
        match cookie(reply) {
            Ok(cookie) => return Ok(Hold::ScreenSaver(cookie)),
            Err(err) => failures.push(format!("screensaver: {err}")),
        }
    }

    let portal = async {
        InhibitProxy::new()
            .await?
            .inhibit(None, InhibitFlags::Idle | InhibitFlags::Suspend, REASON)
            .await
    };
    match portal.await {
        Ok(request) => Ok(Hold::Portal(request)),
        Err(err) => {
            failures.push(format!("portal: {err}"));
            Err(failures.join("; "))
        }
    }
}

async fn release(bus: Option<&Connection>, hold: Hold) {
    let result = match (hold, bus) {
        (Hold::Gnome(cookie), Some(bus)) => bus
            .call_method(
                Some("org.gnome.SessionManager"),
                "/org/gnome/SessionManager",
                Some("org.gnome.SessionManager"),
                "Uninhibit",
                &(cookie,),
            )
            .await
            .map(drop)
            .map_err(|err| err.to_string()),
        (Hold::ScreenSaver(cookie), Some(bus)) => bus
            .call_method(
                Some("org.freedesktop.ScreenSaver"),
                "/org/freedesktop/ScreenSaver",
                Some("org.freedesktop.ScreenSaver"),
                "UnInhibit",
                &(cookie,),
            )
            .await
            .map(drop)
            .map_err(|err| err.to_string()),
        (Hold::Portal(request), _) => request.close().await.map_err(|err| err.to_string()),
        // A cookie cannot exist without the bus it came from.
        (_, None) => Ok(()),
    };
    if let Err(err) = result {
        eprintln!("myvid: could not lift the sleep inhibition: {err}");
    }
}

/// The `u32` cookie an `Inhibit` call replies with.
fn cookie(reply: zbus::Result<zbus::Message>) -> zbus::Result<u32> {
    reply?.body().deserialize()
}
