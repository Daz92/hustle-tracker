//! Wayland idle detection using `ext-idle-notify-v1`.
//!
//! The Wayland event queue is deliberately driven from a dedicated OS thread:
//! `blocking_dispatch` must never block the Tokio runtime.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{Event as IdleNotificationEvent, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};

const IDLE_TIMEOUT_MS: u32 = 10_000;
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

struct State {
    user_active: Arc<AtomicBool>,
}

struct WaylandSession {
    event_queue: EventQueue<State>,
    state: State,
}

/// Start Wayland idle monitoring and return the shared activity flag.
///
/// Initial setup is synchronous so callers can fall back to rdev when the
/// Wayland connection or the idle-notify global is unavailable. Once setup
/// succeeds, all event processing happens on a dedicated OS thread.
pub fn start() -> Result<Arc<AtomicBool>> {
    let user_active = Arc::new(AtomicBool::new(true));

    // wayland-client's globals.bind() panics (rather than returning Err) on a
    // protocol version mismatch, so a plain `?` is not enough to guarantee the
    // rdev fallback. Idle detection must never be able to take the daemon down:
    // losing idle accuracy is acceptable, losing all tracking is not.
    let session = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        connect(&user_active)
    })) {
        Ok(result) => result?,
        Err(_) => {
            return Err(anyhow!(
                "Wayland idle setup panicked (protocol version mismatch?); falling back to rdev"
            ))
        }
    };

    log::info!(
        "Wayland idle backend started (ext-idle-notify-v1, timeout={}ms)",
        IDLE_TIMEOUT_MS
    );

    let thread_user_active = Arc::clone(&user_active);
    thread::Builder::new()
        .name("wayland-idle".to_string())
        .spawn(move || run_event_loop(session, thread_user_active))
        .context("failed to spawn Wayland idle event thread")?;

    Ok(user_active)
}

fn connect(user_active: &Arc<AtomicBool>) -> Result<WaylandSession> {
    user_active.store(true, Ordering::Relaxed);

    let connection = Connection::connect_to_env()
        .context("Wayland connection unavailable (WAYLAND_DISPLAY is not usable)")?;
    let (globals, event_queue) = registry_queue_init::<State>(&connection)
        .context("failed to initialize the Wayland registry")?;
    let queue_handle = event_queue.handle();

    let seat = globals
        .bind::<WlSeat, _, _>(&queue_handle, 1..=9, ())
        .map_err(|error| anyhow!("Wayland compositor does not advertise wl_seat: {error}"))?;
    // Version range must stay 1..=1. wayland-protocols 0.31 only generates
    // ext_idle_notifier_v1 at version 1, and globals.bind() PANICS rather than
    // returning Err when the requested maximum exceeds the generated proxy
    // version ("outdated wayland XML files?"). Requesting 1..=2 killed the daemon
    // on startup.
    let notifier = globals
        .bind::<ExtIdleNotifierV1, _, _>(&queue_handle, 1..=1, ())
        .map_err(|error| {
            anyhow!("Wayland compositor does not advertise ext-idle-notify-v1: {error}")
        })?;

    notifier.get_idle_notification(IDLE_TIMEOUT_MS, &seat, &queue_handle, ());

    Ok(WaylandSession {
        event_queue,
        state: State {
            user_active: Arc::clone(user_active),
        },
    })
}

fn run_event_loop(mut session: WaylandSession, user_active: Arc<AtomicBool>) {
    loop {
        match session.event_queue.blocking_dispatch(&mut session.state) {
            Ok(_) => {}
            Err(error) => {
                log::warn!(
                    "Wayland idle event queue disconnected: {error}; retrying in {}s",
                    RECONNECT_BACKOFF.as_secs()
                );
                thread::sleep(RECONNECT_BACKOFF);

                loop {
                    match connect(&user_active) {
                        Ok(new_session) => {
                            log::info!("Wayland idle backend reconnected");
                            session = new_session;
                            break;
                        }
                        Err(error) => {
                            log::warn!(
                                "Wayland idle backend reconnect failed: {error}; retrying in {}s",
                                RECONNECT_BACKOFF.as_secs()
                            );
                            thread::sleep(RECONNECT_BACKOFF);
                        }
                    }
                }
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: <WlSeat as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtIdleNotifierV1,
        _event: <ExtIdleNotifierV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtIdleNotificationV1,
        event: IdleNotificationEvent,
        _data: &(),
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        match event {
            IdleNotificationEvent::Idled => {
                state.user_active.store(false, Ordering::Relaxed);
                log::info!("Wayland idle transition: Idled");
            }
            IdleNotificationEvent::Resumed => {
                state.user_active.store(true, Ordering::Relaxed);
                log::info!("Wayland idle transition: Resumed");
            }
            _ => {}
        }
    }
}
