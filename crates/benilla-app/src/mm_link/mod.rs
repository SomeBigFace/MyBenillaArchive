//! The Majora's Mask link: an opt-in receiver for LinkState packets from the MM emulator Core.
//! Not part of the 1.12 client; it lives on the `mm-link` branch and does nothing unless
//! `BENILLA_MM_LINK=1`.
//!
//! The packet contract is `mm-bridge/LINKSTATE.md` (v0: 48 bytes, little-endian, UDP on
//! 127.0.0.1). This first crossover reacts to one verified field: when Link's health drops
//! during gameplay, the player jumps through the same one-shot a Space press fires, so the
//! server sees an ordinary jump. The rule itself is in [`detector`].
//!
//! Benilla never depends on the emulator: no handshake, no blocking. Without packets it logs
//! "MM link: no data" and carries on; when they resume it starts fresh.

mod detector;

use std::net::UdpSocket;
use std::time::Instant;

use benilla_world::schedule::WorldStage;
use bevy::prelude::*;

use crate::bindings::{BindingsState, Input};
use crate::char_select::ClientState;
use detector::{decode, DropDetector};

/// The port the link listens on unless `MM_LINK_PORT` names another (LINKSTATE.md's port
/// convention: 47647 is the PowerShell viewer, 47648 is Benilla).
const DEFAULT_PORT: u16 = 47648;
/// Datagrams drained per frame at most; at ~20 a second there are rarely more than one or two.
const MAX_PER_FRAME: usize = 256;

/// The socket and the detector, present only when the link is enabled and its port bound.
#[derive(Resource)]
struct MmLink {
    socket: UdpSocket,
    detector: DropDetector,
}

pub(crate) struct MmLinkPlugin;

impl Plugin for MmLinkPlugin {
    fn build(&self, app: &mut App) {
        if std::env::var("BENILLA_MM_LINK").as_deref() != Ok("1") {
            return;
        }
        let port = std::env::var("MM_LINK_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);
        let socket = match UdpSocket::bind(("127.0.0.1", port)) {
            Ok(s) => s,
            Err(e) => {
                warn!("MM link: can't listen on 127.0.0.1:{port} ({e}); the link is off");
                return;
            }
        };
        if let Err(e) = socket.set_nonblocking(true) {
            warn!("MM link: can't make the socket non-blocking ({e}); the link is off");
            return;
        }
        info!("MM link: listening on 127.0.0.1:{port} for LinkState v0 (no data yet)");
        app.insert_resource(MmLink {
            socket,
            detector: DropDetector::default(),
        })
        .add_systems(
            Update,
            receive
                .after(crate::ui_script::UiInput)
                .before(WorldStage::Input),
        );
    }
}

/// Drain the socket every frame, in or out of the world, so the detector always holds the
/// current state; press Jump only in the world, so damage taken at a login screen is forgotten.
fn receive(
    mut link: ResMut<MmLink>,
    mut binds: ResMut<BindingsState>,
    state: Option<Res<State<ClientState>>>,
) {
    let now = Instant::now();
    let in_world = state.is_some_and(|s| *s.get() == ClientState::InWorld);
    let link = &mut *link;
    let mut jump = false;
    let mut buf = [0u8; 64];
    for _ in 0..MAX_PER_FRAME {
        // WouldBlock is the normal "nothing waiting"; any other error (such as Windows'
        // connection-reset report for UDP) ends this frame's drain the same way.
        let Ok(n) = link.socket.recv(&mut buf) else {
            break;
        };
        let Some(packet) = decode(&buf[..n]) else {
            continue;
        };
        if !link.detector.receiving() {
            info!("MM link: receiving");
        }
        if link.detector.on_packet(packet, now) {
            if in_world {
                info!("MM link: Link's health fell to {}; jumping", packet.health);
            } else {
                info!("MM link: Link's health fell to {} (not in the world; no jump)", packet.health);
            }
            jump = true;
        }
    }
    if link.detector.check_stale(now) {
        info!("MM link: no data");
    }
    if jump && in_world {
        binds.fire_external(Input::Jump);
    }
}
