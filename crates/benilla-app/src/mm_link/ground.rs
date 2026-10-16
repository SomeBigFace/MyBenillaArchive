//! The GroundState sender: the ground under our character, sent to Majora's Mask so the lab floor
//! there can tilt to match (`mm-bridge/GROUNDSTATE.md`).
//!
//! The ground comes from a downward ray from just above the feet against the body's world
//! ([`WorldCollision::ray_body`], the walking collision), not from the mover's own floor snap: the
//! mover keeps that hit internal, and exposing it would mean changing code outside this module.
//! The snap is a downward cast against the same colliders, so on walkable ground they agree.
//!
//! Sent at most [`SEND_HZ`] times a second to `MM_GROUNDSTATE_PORTS` (comma list, up to 4), else
//! 47650 (the recomp mod) and 47651 (the PowerShell viewer). Fire and forget: no reply is read.

use std::net::{SocketAddr, UdpSocket};

use benilla_assets::coords::bevy_to_wow;
use benilla_world::collision::WorldCollision;
use bevy::prelude::*;

use super::ground_packet::{encode, GroundSample, FLAG_GROUND, FLAG_SWIMMING, FLAG_VALID};
use crate::char_select::ClientState;
use crate::player::Player;

const DEFAULT_PORTS: &str = "47650,47651";
const MAX_PORTS: usize = 4;
/// Samples per second at most; the receiver smooths, so more would only add load.
const SEND_HZ: f32 = 30.0;
/// The ray starts this far above the feet (yd), so ground the feet sink into a little is found.
const RAY_LIFT: f32 = 0.5;
/// How far below the feet the ray looks (yd); a longer drop reports no ground.
const RAY_BELOW: f32 = 3.0;

#[derive(Resource)]
pub(super) struct GroundSender {
    socket: UdpSocket,
    dests: Vec<SocketAddr>,
    sequence: u32,
    next_send: f32,
}

impl GroundSender {
    /// Open the socket; `None` (logged) if it can't be opened, and the link carries on without it.
    pub(super) fn open() -> Option<Self> {
        let list = std::env::var("MM_GROUNDSTATE_PORTS").unwrap_or_else(|_| DEFAULT_PORTS.into());
        let dests: Vec<SocketAddr> = list
            .split(',')
            .filter_map(|p| p.trim().parse::<u16>().ok())
            .take(MAX_PORTS)
            .map(|p| SocketAddr::from(([127, 0, 0, 1], p)))
            .collect();
        let socket = match UdpSocket::bind(("127.0.0.1", 0)) {
            Ok(s) => s,
            Err(e) => {
                warn!("MM link: can't open the GroundState socket ({e}); not sending ground");
                return None;
            }
        };
        if let Err(e) = socket.set_nonblocking(true) {
            warn!("MM link: can't make the GroundState socket non-blocking ({e}); not sending ground");
            return None;
        }
        info!("MM link: sending GroundState to {dests:?}");
        Some(Self {
            socket,
            dests,
            sequence: 0,
            next_send: 0.0,
        })
    }
}

/// Sample the ground under the character and send it.
pub(super) fn send_ground(
    mut sender: ResMut<GroundSender>,
    player: Res<Player>,
    world: WorldCollision,
    state: Option<Res<State<ClientState>>>,
    time: Res<Time>,
) {
    let now = time.elapsed_secs();
    if now < sender.next_send {
        return;
    }
    sender.next_send = now + 1.0 / SEND_HZ;

    let in_world = state.is_some_and(|s| *s.get() == ClientState::InWorld);
    let mut sample = GroundSample::default();
    if in_world && player.active && !player.detached && !player.settling {
        sample.flags |= FLAG_VALID;
        if player.swimming {
            sample.flags |= FLAG_SWIMMING;
        }
        sample.feet = bevy_to_wow(player.pos);
        let origin = player.pos + Vec3::Y * RAY_LIFT;
        if let Some(hit) = world.ray_body(origin, Dir3::NEG_Y, RAY_LIFT + RAY_BELOW) {
            let n = Vec3::new(hit.normal.x, hit.normal.y, hit.normal.z).normalize_or_zero();
            if n.y > 0.0 {
                sample.flags |= FLAG_GROUND;
                sample.ground_z = origin.y - hit.distance;
                sample.normal = bevy_to_wow(n);
            }
        }
    }

    let packet = encode(&sample, sender.sequence);
    sender.sequence = sender.sequence.wrapping_add(1);
    for dest in &sender.dests {
        // No listener is normal (the recomp or the viewer is closed); never stall the frame.
        let _ = sender.socket.send_to(&packet, dest);
    }
}
