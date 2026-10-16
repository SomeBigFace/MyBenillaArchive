//! The Majora's Mask link: an opt-in receiver for LinkState packets from the MM emulator Core.
//! Not part of the 1.12 client; it lives on the `mm-link` branch and does nothing unless
//! `BENILLA_MM_LINK=1`.
//!
//! The packet contract is `mm-bridge/LINKSTATE.md` (append-only; this reads the v0 prefix, UDP on
//! 127.0.0.1). This first crossover reacts to one verified field: when Link's health drops
//! during gameplay, the player jumps through the same one-shot a Space press fires, so the
//! server sees an ordinary jump. The rule itself is in [`detector`].
//!
//! It also sends GroundState (`mm-bridge/GROUNDSTATE.md`): the ground under our character, for
//! the lab floor in Majora's Mask to tilt to (see [`ground`]).
//!
//! With `BENILLA_MM_MOVE=1` too, Link running on the MM treadmill drives this character (see
//! [`movement`]).
//!
//! Benilla never depends on the emulator: no handshake, no blocking. Without packets it logs
//! "MM link: no data" and carries on; when they resume it starts fresh.

mod detector;
mod ground;
mod ground_packet;
mod movement;

pub(crate) use movement::MmMove;

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
/// Receive buffer. LinkState only grows (append-only), and on Windows a datagram larger than
/// the buffer fails the read outright, so leave ample room: any UDP payload fits.
const RECV_BUFFER: usize = 65536;

/// The socket and the detector, present only when the link is enabled and its port bound.
#[derive(Resource)]
struct MmLink {
    socket: UdpSocket,
    detector: DropDetector,
    buf: Vec<u8>,
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
        info!("MM link: listening on 127.0.0.1:{port} for LinkState (no data yet)");
        if std::env::var("BENILLA_MM_MOVE").as_deref() == Ok("1") {
            info!("MM link: Majora's Mask movement drives the character while Link is on the treadmill");
            app.init_resource::<MmMove>()
                .init_resource::<BlockWatch>()
                .add_systems(Update, watch_blocked.after(WorldStage::Input));
        }
        if let Some(sender) = ground::GroundSender::open() {
            app.insert_resource(sender).add_systems(
                Update,
                ground::send_ground
                    .run_if(resource_exists::<ground::GroundSender>)
                    .after(WorldStage::Input),
            );
        }
        app.insert_resource(MmLink {
            socket,
            detector: DropDetector::default(),
            buf: vec![0; RECV_BUFFER],
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
    mut mm_move: Option<ResMut<MmMove>>,
    time: Res<Time>,
) {
    let t = time.elapsed_secs();
    let now = Instant::now();
    let in_world = state.is_some_and(|s| *s.get() == ClientState::InWorld);
    let link = &mut *link;
    let mut jump = false;
    for _ in 0..MAX_PER_FRAME {
        // WouldBlock is the normal "nothing waiting"; any other error (such as Windows'
        // connection-reset report for UDP) ends this frame's drain the same way.
        let Ok(n) = link.socket.recv(&mut link.buf) else {
            break;
        };
        if let (Some(m), Some(s)) = (mm_move.as_deref_mut(), movement::decode_movement(&link.buf[..n])) {
            m.on_sample(s, t);
        }
        let Some(packet) = decode(&link.buf[..n]) else {
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
    if let Some(m) = mm_move.as_deref_mut() {
        let d = if in_world { m.evaluate(t) } else { None };
        if d.is_some() != m.driving {
            m.driving = d.is_some();
            if m.driving {
                info!("MM link: movement: Link drives the character");
            } else {
                info!("MM link: movement: keys drive the character ({})", m.idle_reason);
            }
        }
        m.set_current(d);
    }
}

/// The blocked-by-WoW watch: over each second while Link drives, the distance Link asked for
/// against the distance the character actually covered.
#[derive(Resource, Default)]
struct BlockWatch {
    asked: f32,
    covered: f32,
    window: f32,
    last_pos: Option<Vec3>,
    blocked: bool,
}

/// Log, don't fix: a WoW wall or a too-steep slope stops the character while Link keeps running
/// (expected, `mm-bridge/DESIGN.md`). Logged once each time it starts and stops.
fn watch_blocked(
    mm_move: Res<MmMove>,
    player: Res<crate::player::Player>,
    time: Res<Time>,
    mut watch: ResMut<BlockWatch>,
) {
    let pos = player.pos;
    let moved = watch
        .last_pos
        .map_or(0.0, |p| Vec2::new(pos.x - p.x, pos.z - p.z).length());
    watch.last_pos = Some(pos);
    let Some(d) = mm_move.drive().filter(|d| d.moving) else {
        watch.asked = 0.0;
        watch.covered = 0.0;
        watch.window = 0.0;
        return;
    };
    let dt = time.delta_secs();
    watch.asked += d.speed * dt;
    watch.covered += moved;
    watch.window += dt;
    if watch.window >= 1.0 {
        let blocked = watch.asked > 1.0 && watch.covered < 0.2 * watch.asked;
        if blocked != watch.blocked {
            watch.blocked = blocked;
            if blocked {
                info!(
                    "MM link: movement: Link runs ({:.1} yd/s) but the character is blocked in WoW (wall or steep slope)",
                    d.speed
                );
            } else {
                info!("MM link: movement: the character is moving again");
            }
        }
        watch.asked = 0.0;
        watch.covered = 0.0;
        watch.window = 0.0;
    }
}
