//! Groupcast send support (M5): multicast destination address and the
//! persisted global group data counter.
//!
//! The counter shares one space with chip-tool (same source node id), so it
//! never restarts low: it persists ahead of use (SDK PersistedCounter
//! semantics) and boot-jumps past both its own file and chip-tool's `g/gdc`.

use std::io;
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};

/// Persist-ahead window: the file always stores a value no counter below
/// which has been handed out, so a crash can never reuse a sent counter.
pub const COUNTER_EPOCH: u32 = 4096;

/// Matter site-local transient multicast group address (spec §2.5.9.2):
/// `FF35:0040:FD || fabric_id(8B BE) || 00 || group_id(2B BE)`.
pub fn group_multicast_addr(fabric_id: u64, group_id: u16) -> Ipv6Addr {
    let f = fabric_id.to_be_bytes();
    let g = group_id.to_be_bytes();
    Ipv6Addr::from([
        0xff, 0x35, 0x00, 0x40, 0xfd, f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7], 0x00, g[0],
        g[1],
    ])
}

/// Global Group Data Counter with persist-ahead storage (decimal text file).
pub struct PersistedGroupCounter {
    next: u32,
    ceiling: u32,
    path: PathBuf,
}

impl PersistedGroupCounter {
    /// Starts from `max(own persisted ceiling, chip-tool g/gdc) + EPOCH` and
    /// persists the new ceiling before returning. A corrupt counter file is
    /// an error (starting low would get every send dropped by receivers).
    pub fn load(path: &Path, chip_tool_gdc: u32) -> io::Result<Self> {
        let persisted = match std::fs::read_to_string(path) {
            Ok(s) => s.trim().parse::<u32>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "corrupt group counter file")
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        let start = persisted.max(chip_tool_gdc).wrapping_add(COUNTER_EPOCH);
        let mut c = Self {
            next: start,
            ceiling: start,
            path: path.to_path_buf(),
        };
        c.persist(start.wrapping_add(COUNTER_EPOCH))?;
        Ok(c)
    }

    /// Returns the counter to send with and advances. Crossing the persisted
    /// ceiling persists the next window first.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> io::Result<u32> {
        if self.next == self.ceiling {
            self.persist(self.ceiling.wrapping_add(COUNTER_EPOCH))?;
        }
        let v = self.next;
        self.next = self.next.wrapping_add(1);
        Ok(v)
    }

    /// Atomic write (tmp + fsync + rename) so a crash never leaves a
    /// truncated value behind.
    fn persist(&mut self, ceiling: u32) -> io::Result<()> {
        use std::io::Write;
        let tmp = self.path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(format!("{ceiling}\n").as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        self.ceiling = ceiling;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multicast_addr_packs_fabric_and_group() {
        // FF35:0040:FD || fabric_id(8B BE) || 00 || group_id(2B BE)
        assert_eq!(
            group_multicast_addr(0x1122334455667788, 0xaabb),
            std::net::Ipv6Addr::new(0xff35, 0x0040, 0xfd11, 0x2233, 0x4455, 0x6677, 0x8800, 0xaabb)
        );
        assert_eq!(
            group_multicast_addr(1, 10),
            std::net::Ipv6Addr::new(0xff35, 0x0040, 0xfd00, 0, 0, 0, 0x0100, 0x000a)
        );
    }

    fn tmp_counter_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("mat-group-counter-{}-{tag}", std::process::id()))
    }

    #[test]
    fn counter_starts_above_both_sources_plus_epoch() {
        let p = tmp_counter_path("fresh");
        let _ = std::fs::remove_file(&p);
        let mut c = PersistedGroupCounter::load(&p, 1000).unwrap();
        assert_eq!(c.next().unwrap(), 1000 + COUNTER_EPOCH);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn counter_reload_never_reuses_values() {
        let p = tmp_counter_path("reload");
        let _ = std::fs::remove_file(&p);
        let mut c = PersistedGroupCounter::load(&p, 0).unwrap();
        let mut last = 0;
        for _ in 0..10 {
            last = c.next().unwrap();
        }
        drop(c);
        // 再起動相当: chip-tool 側が 0 でも、自前永続値から必ず上へ跳ぶ。
        let mut c2 = PersistedGroupCounter::load(&p, 0).unwrap();
        assert!(c2.next().unwrap() > last);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn counter_gdc_wins_when_larger_than_own_file() {
        let p = tmp_counter_path("gdcwins");
        let _ = std::fs::remove_file(&p);
        drop(PersistedGroupCounter::load(&p, 0).unwrap()); // 小さい自前値を永続化
        let mut c = PersistedGroupCounter::load(&p, 900_000).unwrap();
        assert!(c.next().unwrap() >= 900_000 + COUNTER_EPOCH);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn counter_persists_ahead_across_epoch_boundary() {
        let p = tmp_counter_path("epoch");
        let _ = std::fs::remove_file(&p);
        let mut c = PersistedGroupCounter::load(&p, 0).unwrap();
        let mut prev = None;
        for _ in 0..(COUNTER_EPOCH + 5) {
            let v = c.next().unwrap();
            if let Some(p) = prev {
                assert_eq!(v, p + 1, "strictly sequential across the persist boundary");
            }
            prev = Some(v);
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn counter_corrupt_file_is_an_error() {
        let p = tmp_counter_path("corrupt");
        std::fs::write(&p, "not a number").unwrap();
        assert!(PersistedGroupCounter::load(&p, 0).is_err());
        let _ = std::fs::remove_file(&p);
    }
}
