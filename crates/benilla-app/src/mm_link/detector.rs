//! LinkState v0 decoding and the health-drop → jump rule. Plain Rust with no Bevy, so the rule
//! can be compiled and tested on its own (`rustc --edition 2021 --test detector.rs`).
//! The packet contract is `mm-bridge/LINKSTATE.md`.

use std::time::{Duration, Instant};

/// Silence longer than this is "no data", and the detector starts over (LINKSTATE.md).
pub(crate) const STALE_AFTER: Duration = Duration::from_secs(1);
/// At most one jump per this long, so continuous damage doesn't make a pogo stick.
pub(crate) const JUMP_COOLDOWN: Duration = Duration::from_secs(1);

const PACKET_SIZE: usize = 48;
const FLAG_GAMEPLAY_ACTIVE: u32 = 0x2;

/// The v0 fields the link reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LinkState {
    pub sequence: u32,
    pub gameplay_active: bool,
    pub health: i16,
    pub file_num: i32,
}

/// Decode a LinkState v0 datagram; `None` for anything that isn't exactly one.
pub(crate) fn decode(bytes: &[u8]) -> Option<LinkState> {
    if bytes.len() != PACKET_SIZE || &bytes[0..4] != b"MMLS" {
        return None;
    }
    let u16_at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    if u16_at(4) != 0 || u16_at(6) as usize != PACKET_SIZE {
        return None;
    }
    Some(LinkState {
        sequence: u32_at(8),
        gameplay_active: u32_at(24) & FLAG_GAMEPLAY_ACTIVE != 0,
        health: u16_at(32) as i16,
        file_num: u32_at(44) as i32,
    })
}

/// The jump rule, kept free of sockets and clocks.
#[derive(Default)]
pub(crate) struct DropDetector {
    previous: Option<LinkState>,
    last_packet: Option<Instant>,
    last_jump: Option<Instant>,
}

impl DropDetector {
    /// Feed one packet; true when it should cause a jump. A jump needs the previous and current
    /// packets both in gameplay, the same save file and emulator session, and lower health.
    /// Capacity changes, file loads and emulator restarts never jump.
    pub(crate) fn on_packet(&mut self, state: LinkState, now: Instant) -> bool {
        self.last_packet = Some(now);
        let Some(previous) = self.previous.replace(state) else {
            return false;
        };
        let same_session = state.sequence > previous.sequence && state.file_num == previous.file_num;
        let dropped = previous.gameplay_active
            && state.gameplay_active
            && same_session
            && state.health < previous.health;
        let cooled = self
            .last_jump
            .is_none_or(|t| now.duration_since(t) >= JUMP_COOLDOWN);
        if dropped && cooled {
            self.last_jump = Some(now);
            return true;
        }
        false
    }

    /// Note the passage of time; true on the call where the data goes stale. Going stale
    /// forgets the previous packet, so a resumed stream never compares against old health.
    pub(crate) fn check_stale(&mut self, now: Instant) -> bool {
        match self.last_packet {
            Some(t) if now.duration_since(t) > STALE_AFTER => {
                self.last_packet = None;
                self.previous = None;
                true
            }
            _ => false,
        }
    }

    /// Whether packets are currently arriving (one within [`STALE_AFTER`]).
    pub(crate) fn receiving(&self) -> bool {
        self.last_packet.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A v0 packet built byte by byte from the offsets in LINKSTATE.md.
    fn packet(sequence: u32, active: bool, health: i16, capacity: i16, file_num: i32) -> Vec<u8> {
        let mut b = vec![0u8; 48];
        b[0..4].copy_from_slice(b"MMLS");
        b[4..6].copy_from_slice(&0u16.to_le_bytes()); // version
        b[6..8].copy_from_slice(&48u16.to_le_bytes()); // size
        b[8..12].copy_from_slice(&sequence.to_le_bytes());
        b[12..16].copy_from_slice(&sequence.to_le_bytes()); // emulator frame
        b[16..20].copy_from_slice(&7u32.to_le_bytes()); // VI count
        b[20..24].copy_from_slice(&0xABCDu32.to_le_bytes()); // update stamp
        let flags: u32 = 1 | if active { 2 } else { 0 } | 4;
        b[24..28].copy_from_slice(&flags.to_le_bytes());
        b[28..32].copy_from_slice(&0x1Fu32.to_le_bytes()); // field-present bits
        b[32..34].copy_from_slice(&health.to_le_bytes());
        b[34..36].copy_from_slice(&capacity.to_le_bytes());
        b[36] = 96; // magic
        b[37] = 2; // magic level
        b[38] = 4; // player form: Human
        b[40..44].copy_from_slice(&(if active { 0i32 } else { 1 }).to_le_bytes()); // game mode
        b[44..48].copy_from_slice(&file_num.to_le_bytes());
        b
    }

    fn state(sequence: u32, active: bool, health: i16) -> LinkState {
        decode(&packet(sequence, active, health, 320, 0)).unwrap()
    }

    #[test]
    fn decodes_the_documented_layout() {
        let s = decode(&packet(42, true, 312, 320, 0)).unwrap();
        assert_eq!(s, LinkState { sequence: 42, gameplay_active: true, health: 312, file_num: 0 });
        let title = decode(&packet(1, false, 0, 0, 255)).unwrap();
        assert!(!title.gameplay_active);
        assert_eq!(title.file_num, 255);
        assert_eq!(decode(&packet(9, true, -1, 320, 0)).unwrap().health, -1);
    }

    #[test]
    fn rejects_wrong_magic_version_or_size() {
        let good = packet(1, true, 48, 48, 0);
        let mut magic = good.clone();
        magic[0] = b'X';
        let mut version = good.clone();
        version[4] = 1;
        let mut size = good.clone();
        size[6] = 40;
        assert!(decode(&good).is_some());
        assert!(decode(&magic).is_none());
        assert!(decode(&version).is_none());
        assert!(decode(&size).is_none());
        assert!(decode(&good[..47]).is_none());
        assert!(decode(&[good.clone(), vec![0]].concat()).is_none());
    }

    #[test]
    fn a_health_drop_in_gameplay_jumps_once_per_cooldown() {
        let mut d = DropDetector::default();
        let t0 = Instant::now();
        assert!(!d.on_packet(state(1, true, 320), t0));
        assert!(d.on_packet(state(2, true, 312), t0 + Duration::from_millis(50)));
        // Continuous damage inside the cooldown doesn't jump again.
        assert!(!d.on_packet(state(3, true, 304), t0 + Duration::from_millis(500)));
        assert!(d.on_packet(state(4, true, 296), t0 + Duration::from_millis(1100)));
        // Unchanged or rising health never jumps.
        assert!(!d.on_packet(state(5, true, 296), t0 + Duration::from_millis(2500)));
        assert!(!d.on_packet(state(6, true, 312), t0 + Duration::from_millis(2600)));
    }

    #[test]
    fn health_zero_in_gameplay_is_a_drop() {
        let mut d = DropDetector::default();
        let t0 = Instant::now();
        d.on_packet(state(1, true, 4), t0);
        assert!(d.on_packet(state(2, true, 0), t0));
    }

    #[test]
    fn transitions_and_capacity_changes_never_jump() {
        let t0 = Instant::now();
        let mut d = DropDetector::default();
        // Title screen to file load: an inactive side never counts.
        d.on_packet(state(1, false, 48), t0);
        assert!(!d.on_packet(state(2, false, 0), t0));
        assert!(!d.on_packet(state(3, true, 320), t0));
        // Leaving gameplay with lower health doesn't count either.
        assert!(!d.on_packet(state(4, false, 0), t0 + Duration::from_secs(5)));
        // A capacity change alone isn't a drop.
        let mut d = DropDetector::default();
        d.on_packet(decode(&packet(1, true, 320, 320, 0)).unwrap(), t0);
        assert!(!d.on_packet(decode(&packet(2, true, 320, 336, 0)).unwrap(), t0));
        // A different save file is a new session, even with lower health.
        assert!(!d.on_packet(decode(&packet(3, true, 48, 48, 1)).unwrap(), t0));
    }

    #[test]
    fn an_emulator_restart_is_a_new_session() {
        let mut d = DropDetector::default();
        let t0 = Instant::now();
        d.on_packet(state(500, true, 320), t0);
        assert!(!d.on_packet(state(0, true, 48), t0 + Duration::from_millis(10)));
    }

    #[test]
    fn silence_goes_stale_and_resets_the_detector() {
        let mut d = DropDetector::default();
        let t0 = Instant::now();
        d.on_packet(state(1, true, 320), t0);
        assert!(!d.check_stale(t0 + Duration::from_millis(900)));
        assert!(d.receiving());
        assert!(d.check_stale(t0 + Duration::from_millis(1100)));
        assert!(!d.receiving());
        assert!(!d.check_stale(t0 + Duration::from_millis(1200)), "stale is reported once");
        // The first packet after the gap only primes the detector, even with lower health.
        assert!(!d.on_packet(state(60, true, 200), t0 + Duration::from_secs(3)));
        assert!(d.on_packet(state(61, true, 190), t0 + Duration::from_secs(3)));
    }
}
