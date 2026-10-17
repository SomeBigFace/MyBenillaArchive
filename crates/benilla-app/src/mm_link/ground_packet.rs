//! The GroundState packet (contract: `mm-bridge/GROUNDSTATE.md`), Benilla → Majora's Mask: the
//! ground under our character, in WoW yards and WoW axes (+X north, +Y west, +Z up). The receiver
//! converts. Standard library only, so it is testable on its own.

/// `"MMGS"`.
pub(super) const MAGIC: [u8; 4] = *b"MMGS";
pub(super) const VERSION: u16 = 0;
pub(super) const SIZE: usize = 48;

/// Flag bit 0: the character is in the world and the fields describe it.
pub(super) const FLAG_VALID: u32 = 1 << 0;
/// Flag bit 1: ground was found under the feet; height and normal are meaningful.
pub(super) const FLAG_GROUND: u32 = 1 << 1;
/// Flag bit 2: the character is swimming (informational).
pub(super) const FLAG_SWIMMING: u32 = 1 << 2;

/// One GroundState sample.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct GroundSample {
    pub(super) flags: u32,
    /// Height of the ground under the feet (WoW z, yards).
    pub(super) ground_z: f32,
    /// Unit normal of that ground, WoW axes.
    pub(super) normal: [f32; 3],
    /// The character's feet (WoW x, y, z, yards).
    pub(super) feet: [f32; 3],
}

/// Encode `sample` as a v0 GroundState datagram with `sequence`.
pub(super) fn encode(sample: &GroundSample, sequence: u32) -> [u8; SIZE] {
    let mut b = [0u8; SIZE];
    b[0..4].copy_from_slice(&MAGIC);
    b[4..6].copy_from_slice(&VERSION.to_le_bytes());
    b[6..8].copy_from_slice(&(SIZE as u16).to_le_bytes());
    b[8..12].copy_from_slice(&sequence.to_le_bytes());
    b[12..16].copy_from_slice(&sample.flags.to_le_bytes());
    b[16..20].copy_from_slice(&sample.ground_z.to_le_bytes());
    for (i, v) in sample.normal.iter().enumerate() {
        b[20 + i * 4..24 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    for (i, v) in sample.feet.iter().enumerate() {
        b[32 + i * 4..36 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    b
}

/// GroundState v1 appends a height grid (`mm-bridge/GROUNDSTATE.md`, "v1 grid block").
pub(super) const VERSION_GRID: u16 = 1;
/// Samples per side; the grid is `GRID_N × GRID_N`.
pub(super) const GRID_N: usize = 13;
/// v1 size: the v0 prefix, a 20-byte grid header, the heights, then the hole bitmap, padded.
pub(super) const SIZE_GRID: usize = 768;
const GRID_HEADER: usize = 48;
const GRID_HEIGHTS: usize = 68;
const GRID_HOLES: usize = GRID_HEIGHTS + GRID_N * GRID_N * 4;

/// A height grid around the character, in WoW yards and axes: sample `(i, j)` stands at
/// north `origin[0] + i·spacing`, west `origin[1] + j·spacing`, and is `heights[i·N + j]` high,
/// unless `holes[i·N + j]` (nothing found within the ray's reach: a drop).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct GroundGrid {
    pub(super) spacing: f32,
    pub(super) origin: [f32; 2],
    /// +1 each time the grid is resampled.
    pub(super) sequence: u32,
    pub(super) heights: Vec<f32>,
    pub(super) holes: Vec<bool>,
}

/// Encode `sample` with `grid` as a v1 GroundState datagram with `sequence`.
pub(super) fn encode_grid(sample: &GroundSample, grid: &GroundGrid, sequence: u32) -> Vec<u8> {
    let mut b = vec![0u8; SIZE_GRID];
    b[..SIZE].copy_from_slice(&encode(sample, sequence));
    b[4..6].copy_from_slice(&VERSION_GRID.to_le_bytes());
    b[6..8].copy_from_slice(&(SIZE_GRID as u16).to_le_bytes());
    b[GRID_HEADER..GRID_HEADER + 2].copy_from_slice(&(GRID_N as u16).to_le_bytes());
    b[GRID_HEADER + 2..GRID_HEADER + 4].copy_from_slice(&1u16.to_le_bytes()); // grid valid
    b[52..56].copy_from_slice(&grid.spacing.to_le_bytes());
    b[56..60].copy_from_slice(&grid.origin[0].to_le_bytes());
    b[60..64].copy_from_slice(&grid.origin[1].to_le_bytes());
    b[64..68].copy_from_slice(&grid.sequence.to_le_bytes());
    for k in 0..GRID_N * GRID_N {
        let h = if grid.holes[k] { 0.0 } else { grid.heights[k] };
        b[GRID_HEIGHTS + k * 4..GRID_HEIGHTS + k * 4 + 4].copy_from_slice(&h.to_le_bytes());
        if grid.holes[k] {
            b[GRID_HOLES + k / 8] |= 1 << (k % 8);
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grid_layout_matches_the_contract() {
        let s = GroundSample {
            flags: FLAG_VALID | FLAG_GROUND,
            ground_z: 10.0,
            normal: [0.0, 0.0, 1.0],
            feet: [100.5, 200.25, 10.0],
        };
        let n = GRID_N * GRID_N;
        let mut grid = GroundGrid {
            spacing: 1.0,
            origin: [94.0, 194.0],
            sequence: 9,
            heights: (0..n).map(|k| k as f32 * 0.5).collect(),
            holes: vec![false; n],
        };
        grid.holes[3] = true;
        grid.holes[168] = true;
        let b = encode_grid(&s, &grid, 4);
        assert_eq!(b.len(), 768);
        assert_eq!(u16::from_le_bytes([b[4], b[5]]), 1);
        assert_eq!(u16::from_le_bytes([b[6], b[7]]), 768);
        assert_eq!(f32_at(&b, 16), 10.0, "the v0 prefix is unchanged");
        assert_eq!(u16::from_le_bytes([b[48], b[49]]), 13);
        assert_eq!(u16::from_le_bytes([b[50], b[51]]), 1);
        assert_eq!([f32_at(&b, 52), f32_at(&b, 56), f32_at(&b, 60)], [1.0, 94.0, 194.0]);
        assert_eq!(u32::from_le_bytes(b[64..68].try_into().unwrap()), 9);
        assert_eq!(f32_at(&b, 68 + 4 * 2), 1.0);
        assert_eq!(f32_at(&b, 68 + 4 * 3), 0.0, "a hole's height is 0");
        assert_eq!(f32_at(&b, 68 + 4 * 167), 83.5);
        let holes = 68 + 169 * 4;
        assert_eq!(b[holes], 1 << 3);
        assert_eq!(b[holes + 21], 1 << 0, "sample 168 is bit 0 of byte 21");
        assert_eq!(&b[holes + 22..], &[0, 0], "padding");
    }

    fn f32_at(b: &[u8], at: usize) -> f32 {
        f32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn the_layout_matches_the_contract() {
        let s = GroundSample {
            flags: FLAG_VALID | FLAG_GROUND,
            ground_z: 42.5,
            normal: [0.6, 0.0, 0.8],
            feet: [-8913.0, -130.25, 82.0],
        };
        let b = encode(&s, 7);
        assert_eq!(&b[0..4], b"MMGS");
        assert_eq!(u16::from_le_bytes([b[4], b[5]]), 0);
        assert_eq!(u16::from_le_bytes([b[6], b[7]]), 48);
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(b[12..16].try_into().unwrap()), 3);
        assert_eq!(f32_at(&b, 16), 42.5);
        assert_eq!([f32_at(&b, 20), f32_at(&b, 24), f32_at(&b, 28)], [0.6, 0.0, 0.8]);
        assert_eq!([f32_at(&b, 32), f32_at(&b, 36), f32_at(&b, 40)], [-8913.0, -130.25, 82.0]);
        assert_eq!(&b[44..48], &[0, 0, 0, 0]);
    }
}
