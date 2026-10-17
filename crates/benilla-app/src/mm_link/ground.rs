//! The GroundState sender: the ground under our character, sent to Majora's Mask so the lab floor
//! there can tilt to match (`mm-bridge/GROUNDSTATE.md`).
//!
//! The ground comes from a downward ray from just above the feet against the body's world
//! ([`WorldCollision::ray_body`], the walking collision), not from the mover's own floor snap: the
//! mover keeps that hit internal, and exposing it would mean changing code outside this module.
//! The snap is a downward cast against the same colliders, so on walkable ground they agree.
//!
//! v1 also carries a height grid ([`GRID_N`]² samples [`GRID_SPACING`] yd apart, snapped to the world
//! grid so the terrain does not shimmer as the character moves), resampled [`GRID_HZ`] times a
//! second with one downward ray per sample from [`GRID_RAY_ABOVE`] yd above the feet; a ray that
//! finds nothing within [`GRID_RAY_REACH`] yd marks a hole (a drop). The first surface down wins, so
//! a roof or a bridge above the feet but under the ray's start is reported as ground.
//!
//! Sent at most [`SEND_HZ`] times a second to `MM_GROUNDSTATE_PORTS` (comma list, up to 4), else
//! 47650 (the recomp mod) and 47651 (the PowerShell viewer). Fire and forget: no reply is read.

use std::net::{SocketAddr, UdpSocket};

use benilla_assets::coords::{bevy_to_wow, wow_to_bevy};
use benilla_world::collision::WorldCollision;
use bevy::prelude::*;

use super::ground_packet::{
    encode, encode_grid, GroundGrid, GroundSample, FLAG_GROUND, FLAG_SWIMMING, FLAG_VALID, GRID_N,
};
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
/// The height grid: samples apart (yd), resamples a second, and each ray's start above the feet and
/// reach (yd).
const GRID_SPACING: f32 = 1.0;
const GRID_HZ: f32 = 10.0;
const GRID_RAY_ABOVE: f32 = 4.0;
const GRID_RAY_REACH: f32 = 12.0;

#[derive(Resource)]
pub(super) struct GroundSender {
    socket: UdpSocket,
    dests: Vec<SocketAddr>,
    sequence: u32,
    next_send: f32,
    grid: Option<GroundGrid>,
    next_grid: f32,
    grid_sequence: u32,
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
            grid: None,
            next_grid: 0.0,
            grid_sequence: 0,
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
        if now >= sender.next_grid {
            sender.next_grid = now + 1.0 / GRID_HZ;
            sender.grid_sequence = sender.grid_sequence.wrapping_add(1);
            let seq = sender.grid_sequence;
            sender.grid = Some(sample_grid(&world, sample.feet, seq));
        }
    } else {
        sender.grid = None;
    }

    let packet = match &sender.grid {
        Some(grid) => encode_grid(&sample, grid, sender.sequence),
        None => encode(&sample, sender.sequence).to_vec(),
    };
    sender.sequence = sender.sequence.wrapping_add(1);
    for dest in &sender.dests {
        // No listener is normal (the recomp or the viewer is closed); never stall the frame.
        let _ = sender.socket.send_to(&packet, dest);
    }
}

/// The grid's origin (sample `(0, 0)`, WoW north and west) for feet at `feet`: the samples sit on
/// whole multiples of the spacing, with the feet inside the middle cell.
pub(super) fn grid_origin(feet: [f32; 3]) -> [f32; 2] {
    let half = (GRID_N / 2) as f32 * GRID_SPACING;
    let snap = |v: f32| (v / GRID_SPACING).floor() * GRID_SPACING;
    [snap(feet[0]) - half, snap(feet[1]) - half]
}

/// One downward ray per sample against the walking collision.
fn sample_grid(world: &WorldCollision, feet: [f32; 3], sequence: u32) -> GroundGrid {
    let origin = grid_origin(feet);
    let top = feet[2] + GRID_RAY_ABOVE;
    let mut heights = vec![0.0; GRID_N * GRID_N];
    let mut holes = vec![true; GRID_N * GRID_N];
    for i in 0..GRID_N {
        for j in 0..GRID_N {
            let north = origin[0] + i as f32 * GRID_SPACING;
            let west = origin[1] + j as f32 * GRID_SPACING;
            let start = wow_to_bevy([north, west, top]);
            if let Some(hit) = world.ray_body(start, Dir3::NEG_Y, GRID_RAY_REACH) {
                heights[i * GRID_N + j] = top - hit.distance;
                holes[i * GRID_N + j] = false;
            }
        }
    }
    GroundGrid {
        spacing: GRID_SPACING,
        origin,
        sequence,
        heights,
        holes,
    }
}
