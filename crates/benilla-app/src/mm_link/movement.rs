//! Majora's Mask drives the character (`BENILLA_MM_MOVE=1`, with `BENILLA_MM_LINK=1`): while Link
//! runs on the MM treadmill, his per-update displacement (LinkState v3, `mm-bridge/LINKSTATE.md`)
//! becomes this character's movement, at Link's own speed, converted with the active form's
//! MM-units-per-yard that the packet carries.
//!
//! How it drives: by **input, not position**. Each frame [`MmMove::drive`] gives the controller a
//! facing, "forward or stop" and a speed, which replace the keys' axes and the server's run speed
//! (one hook in `player/controller.rs`). Everything after that is Benilla's own: the mover's
//! collision and ground snap, the movement flags, the `MSG_MOVE_*` stream and the run animation, so
//! the server sees ordinary running. Driving the position directly would bypass the mover's
//! collision and stream moves with no direction bits; the keys' own bits would run at WoW's fixed
//! 7 yd/s, not Link's speed.
//!
//! Axes: the MM frame (`mm-bridge/GROUNDSTATE.md`) is exactly Bevy's (both are WoW mapped by
//! `(−y, z, −x)`), so a displacement converts by the scale alone, and an MM yaw θ (0 faces +Z) is
//! the Bevy facing θ + π (Bevy facing 0 faces −Z).
//!
//! Facing while moving is the direction Link actually moves (his displacement), so strafes,
//! sidehops and backflips carry the character the same way (it turns to face that way, since it
//! only runs forward). Standing, it is Link's facing (`Actor.shape.rot.y`).

use std::f32::consts::PI;

/// Game updates per second in Majora's Mask (20 Hz).
pub(super) const MM_UPDATES_PER_SEC: f32 = 20.0;
/// Below this (yd/s) the character stands.
const STOP_SPEED: f32 = 0.05;
/// No movement sample for this long (s): the character stops and keys take over again.
const STALE_SECS: f32 = 0.25;
/// Samples averaged for the speed (game updates).
const SMOOTH: usize = 4;
/// One update's step above this many MM units is not running (a warp or a re-anchor).
const MAX_STEP_UNITS: f32 = 200.0;

/// The v3 fields the mapping reads.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct MoveSample {
    pub(super) gameplay_frames: u32,
    pub(super) treadmill: bool,
    pub(super) facing: i16,
    pub(super) transformation: u8,
    pub(super) units_per_yard: f32,
    pub(super) disp_x: f32,
    pub(super) disp_z: f32,
    pub(super) epoch: u32,
}

/// Decode the movement fields of a LinkState packet of version 3 or later; `None` for anything
/// older, not gameplay with a valid Player, or with bad values.
pub(super) fn decode_movement(b: &[u8]) -> Option<MoveSample> {
    const V3_SIZE: usize = 192;
    if b.len() < V3_SIZE || &b[0..4] != b"MMLS" {
        return None;
    }
    let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let f32_at = |o: usize| f32::from_bits(u32_at(o));
    if u16_at(4) < 3 || u16_at(6) as usize != b.len() {
        return None;
    }
    let flags = u32_at(24);
    // Gameplay active (bit 1) and Player valid (bit 3).
    if flags & 0x0A != 0x0A {
        return None;
    }
    let s = MoveSample {
        gameplay_frames: u32_at(52),
        treadmill: flags & 0x10 != 0,
        facing: u16_at(58) as i16,
        transformation: b[120],
        units_per_yard: f32_at(176),
        disp_x: f32_at(180),
        disp_z: f32_at(184),
        epoch: u32_at(188),
    };
    let finite = s.units_per_yard.is_finite() && s.disp_x.is_finite() && s.disp_z.is_finite();
    (finite && s.units_per_yard > 1.0).then_some(s)
}

/// One update's MM displacement (MM units) in yards on Bevy's ground plane `(x, z)`.
pub(super) fn step_yards(disp_x: f32, disp_z: f32, units_per_yard: f32) -> (f32, f32) {
    (disp_x / units_per_yard, disp_z / units_per_yard)
}

/// An MM yaw (65536 = a turn, 0 faces +Z) as Benilla's facing (radians, 0 faces −Z), in (−π, π].
pub(super) fn mm_yaw_to_facing(binang: i16) -> f32 {
    wrap(binang as f32 * (PI / 32768.0) + PI)
}

/// The Bevy facing that moves along ground direction `(x, z)` (its forward is `(−sin, −cos)`).
pub(super) fn facing_of_direction(x: f32, z: f32) -> f32 {
    wrap((-x).atan2(-z))
}

fn wrap(a: f32) -> f32 {
    let mut a = a % (2.0 * PI);
    if a <= -PI {
        a += 2.0 * PI;
    } else if a > PI {
        a -= 2.0 * PI;
    }
    a
}

/// What the controller is told this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Drive {
    /// Run forward (else stand).
    pub(crate) moving: bool,
    /// The facing to hold (Benilla yaw, radians).
    pub(crate) face_yaw: f32,
    /// The ground speed to run at (yd/s), replacing the server's run speed.
    pub(crate) speed: f32,
}

/// The mapping's state; a resource only when `BENILLA_MM_MOVE=1`.
#[derive(bevy::prelude::Resource, Default)]
pub(crate) struct MmMove {
    last: Option<MoveSample>,
    last_at: f32,
    speeds: [f32; SMOOTH],
    speed_slots: usize,
    dir: (f32, f32),
    current: Option<Drive>,
    pub(super) driving: bool,
    pub(super) idle_reason: &'static str,
}

impl MmMove {
    /// Take one decoded packet received at `now` (seconds).
    pub(super) fn on_sample(&mut self, s: MoveSample, now: f32) {
        // A repeat of the same game update carries no new movement.
        if self.last.is_some_and(|l| l.gameplay_frames == s.gameplay_frames) {
            return;
        }
        let continuous = self.last.is_some_and(|l| {
            l.epoch == s.epoch && s.gameplay_frames.wrapping_sub(l.gameplay_frames) <= 10
        });
        let step = (s.disp_x * s.disp_x + s.disp_z * s.disp_z).sqrt();
        if continuous && step <= MAX_STEP_UNITS {
            let (x, z) = step_yards(s.disp_x, s.disp_z, s.units_per_yard);
            let speed = (x * x + z * z).sqrt() * MM_UPDATES_PER_SEC;
            self.speeds[self.speed_slots % SMOOTH] = speed;
            self.speed_slots += 1;
            if speed > STOP_SPEED {
                self.dir = (x, z);
            }
        }
        self.last = Some(s);
        self.last_at = now;
    }

    fn speed(&self) -> f32 {
        let n = self.speed_slots.min(SMOOTH);
        if n == 0 {
            return 0.0;
        }
        self.speeds[..n].iter().sum::<f32>() / n as f32
    }

    /// This frame's drive, or `None` when the keys should have the character: no fresh sample,
    /// or Link isn't on the treadmill.
    pub(super) fn evaluate(&mut self, now: f32) -> Option<Drive> {
        let Some(s) = self.last else {
            self.idle_reason = "no LinkState v3 yet";
            return None;
        };
        if now - self.last_at > STALE_SECS {
            self.idle_reason = "no fresh LinkState (MM closed, paused or loading)";
            self.speed_slots = 0;
            return None;
        }
        if !s.treadmill {
            self.idle_reason = "Link is not on the treadmill";
            self.speed_slots = 0;
            return None;
        }
        let speed = self.speed();
        let moving = speed > STOP_SPEED;
        let face_yaw = if moving {
            facing_of_direction(self.dir.0, self.dir.1)
        } else {
            mm_yaw_to_facing(s.facing)
        };
        Some(Drive {
            moving,
            face_yaw,
            speed,
        })
    }

    /// The drive the controller reads, computed by the link each frame before it runs.
    pub(crate) fn drive(&self) -> Option<Drive> {
        self.current
    }

    /// Store this frame's [`Self::evaluate`] for the controller.
    pub(super) fn set_current(&mut self, d: Option<Drive>) {
        self.current = d;
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn packet(frames: u32, treadmill: bool, facing: i16, upy: f32, dx: f32, dz: f32, epoch: u32) -> Vec<u8> {
        let mut b = vec![0u8; 192];
        b[0..4].copy_from_slice(b"MMLS");
        b[4..6].copy_from_slice(&3u16.to_le_bytes());
        b[6..8].copy_from_slice(&192u16.to_le_bytes());
        let flags: u32 = 0x0F | if treadmill { 0x10 } else { 0 };
        b[24..28].copy_from_slice(&flags.to_le_bytes());
        b[52..56].copy_from_slice(&frames.to_le_bytes());
        b[58..60].copy_from_slice(&facing.to_le_bytes());
        b[176..180].copy_from_slice(&upy.to_le_bytes());
        b[180..184].copy_from_slice(&dx.to_le_bytes());
        b[184..188].copy_from_slice(&dz.to_le_bytes());
        b[188..192].copy_from_slice(&epoch.to_le_bytes());
        b
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn decodes_v3_and_rejects_older() {
        let s = decode_movement(&packet(7, true, 0x4000, 30.9, 8.25, 0.0, 2)).unwrap();
        assert_eq!((s.gameplay_frames, s.treadmill, s.facing, s.epoch), (7, true, 0x4000, 2));
        assert!(close(s.units_per_yard, 30.9) && close(s.disp_x, 8.25));
        let mut v2 = packet(7, true, 0, 30.9, 0.0, 0.0, 0);
        v2.truncate(176);
        v2[4..6].copy_from_slice(&2u16.to_le_bytes());
        v2[6..8].copy_from_slice(&176u16.to_le_bytes());
        assert!(decode_movement(&v2).is_none());
        let mut nan = packet(7, true, 0, 30.9, f32::NAN, 0.0, 0);
        assert!(decode_movement(&nan).is_none());
        nan = packet(7, true, 0, 0.0, 1.0, 0.0, 0);
        assert!(decode_movement(&nan).is_none(), "a zero scale is rejected");
    }

    /// MM runs of 8.25 units/update (Human) at each form's scale come out at the measured speeds.
    #[test]
    fn per_form_scale_gives_the_measured_speeds() {
        for (upy, step, want) in [
            (30.9_f32, 8.25_f32, 5.34_f32), // Human run, 76 % of 7 yd/s
            (30.9, 12.25, 7.93),            // Bunny Hood
            (41.7, 14.75, 7.07),            // Fierce Deity
            (24.9, 9.0, 7.23),              // Zora run
            (26.6, 23.4, 17.59),            // Goron roll (troll scale)
            (31.8, 9.0, 5.66),              // Deku run
        ] {
            let (x, z) = step_yards(step, 0.0, upy);
            let speed = (x * x + z * z).sqrt() * MM_UPDATES_PER_SEC;
            assert!((speed - want).abs() < 0.02, "upy {upy} step {step}: {speed} vs {want}");
        }
    }

    /// The axis map: MM −Z is WoW north, +X east (GROUNDSTATE.md); Bevy agrees, so Benilla's
    /// `bevy_to_wow` of a converted step is north / east, and the facing runs along it.
    #[test]
    fn mm_directions_land_on_the_right_compass_points() {
        // Bevy → WoW, as benilla_assets::coords::bevy_to_wow: [-z, -x, y].
        let wow = |x: f32, z: f32| [-z, -x];
        let north = step_yards(0.0, -10.0, 30.9);
        assert!(wow(north.0, north.1)[0] > 0.0 && close(wow(north.0, north.1)[1], 0.0));
        let east = step_yards(10.0, 0.0, 30.9);
        assert!(wow(east.0, east.1)[1] < 0.0, "east is WoW −Y");
        // The facing toward a direction runs along it: forward (−sin, −cos) matches.
        for (x, z) in [(0.0, -1.0), (1.0, 0.0), (0.0, 1.0), (-1.0, 0.0), (0.6, -0.8)] {
            let f = facing_of_direction(x, z);
            assert!(close(-f.sin(), x) && close(-f.cos(), z), "({x}, {z}) -> {f}");
        }
        // MM yaw 0 faces MM +Z (south); 0x4000 faces +X (east): the same facings.
        assert!(close(mm_yaw_to_facing(0), facing_of_direction(0.0, 1.0)));
        assert!(close(mm_yaw_to_facing(0x4000), facing_of_direction(1.0, 0.0)));
        assert!(close(mm_yaw_to_facing(-0x8000), facing_of_direction(0.0, -1.0)));
    }

    #[test]
    fn drives_only_on_the_treadmill_and_while_fresh() {
        let mut m = MmMove::default();
        assert!(m.evaluate(0.0).is_none());
        for i in 0..6 {
            m.on_sample(decode_movement(&packet(i, true, 0, 30.9, 0.0, -8.25, 1)).unwrap(), i as f32 * 0.05);
        }
        let d = m.evaluate(0.3).unwrap();
        assert!(d.moving && (d.speed - 5.34).abs() < 0.02);
        assert!(close(d.face_yaw, facing_of_direction(0.0, -1.0)), "runs north");
        assert!(m.evaluate(0.3 + STALE_SECS + 0.01).is_none(), "stale");
        let mut off = MmMove::default();
        off.on_sample(decode_movement(&packet(1, false, 0, 30.9, 0.0, -8.25, 1)).unwrap(), 0.0);
        assert!(off.evaluate(0.0).is_none(), "treadmill off: keys drive");
    }

    #[test]
    fn a_reanchor_or_a_gap_is_not_movement_and_a_form_change_rescales() {
        let mut m = MmMove::default();
        m.on_sample(decode_movement(&packet(1, true, 0, 30.9, 0.0, 0.0, 1)).unwrap(), 0.0);
        // A new epoch: its step is the jump to a new anchor, ignored.
        m.on_sample(decode_movement(&packet(2, true, 0, 30.9, 0.0, -150.0, 2)).unwrap(), 0.05);
        assert_eq!(m.speed_slots, 0);
        // Then Goron at its own scale: 9 units/update is 6.77 yd/s.
        for i in 3..8 {
            m.on_sample(decode_movement(&packet(i, true, 0, 26.6, 9.0, 0.0, 2)).unwrap(), i as f32 * 0.05);
        }
        let d = m.evaluate(0.4).unwrap();
        assert!((d.speed - 6.77).abs() < 0.02, "{}", d.speed);
        // Standing still: stop, facing Link's facing (yaw 0 = south).
        let mut s = MmMove::default();
        for i in 0..5 {
            s.on_sample(decode_movement(&packet(i, true, 0, 30.9, 0.0, 0.0, 1)).unwrap(), 0.0);
        }
        let d = s.evaluate(0.0).unwrap();
        assert!(!d.moving && close(d.face_yaw, mm_yaw_to_facing(0)));
    }
}
