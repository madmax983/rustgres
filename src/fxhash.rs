//! A small, non-cryptographic hasher for the catalog's string-keyed maps
//! (`Database::tables`/`indexes`/`stats`/`views`/`sequences`/`roles`).
//!
//! These maps are keyed by short, trusted identifiers (table/role/index
//! names chosen by whoever is connected to this server) and are probed
//! many times per query (once per `find_table`/`find_role`/... call), so
//! the default `SipHash` — designed to resist an attacker who controls
//! the keys and can measure hash-flooding DoS — pays a fixed per-call
//! cost (multiple mix rounds) that this workload has no reason to pay.
//! This is the public-domain FxHash algorithm (originally from Firefox,
//! also used by `rustc-hash`): a rotate-xor-multiply mix, reimplemented
//! here instead of taking a dependency on it.
use std::hash::Hasher;

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

#[derive(Default)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add_to_hash(&mut self, w: u64) {
        self.hash = (self.hash.rotate_left(5) ^ w).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            self.add_to_hash(u64::from_ne_bytes(bytes[..8].try_into().unwrap()));
            bytes = &bytes[8..];
        }
        if bytes.len() >= 4 {
            self.add_to_hash(u32::from_ne_bytes(bytes[..4].try_into().unwrap()) as u64);
            bytes = &bytes[4..];
        }
        if bytes.len() >= 2 {
            self.add_to_hash(u16::from_ne_bytes(bytes[..2].try_into().unwrap()) as u64);
            bytes = &bytes[2..];
        }
        if let [b] = bytes {
            self.add_to_hash(*b as u64);
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add_to_hash(i as u64);
    }
    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add_to_hash(i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add_to_hash(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add_to_hash(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add_to_hash(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

pub type FxBuildHasher = std::hash::BuildHasherDefault<FxHasher>;
