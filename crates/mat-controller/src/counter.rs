//! Message counters and replay-protection window (spec §4.5).

/// Outgoing message counter, randomly initialized per spec 4.5.1.
pub struct TxCounter(u32);

impl TxCounter {
    pub fn new_random() -> Self {
        let mut b = [0u8; 4];
        getrandom::getrandom(&mut b).expect("os rng");
        Self((u32::from_le_bytes(b) & 0x0FFF_FFFF) + 1)
    }

    /// Returns the current counter value and advances.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u32 {
        let v = self.0;
        self.0 = self.0.wrapping_add(1);
        v
    }
}

/// Sliding replay-protection window (width 32) over received counters.
pub struct RxWindow {
    max: u32,
    /// bit i set = counter (max - 1 - i) already seen
    bitmap: u32,
    empty: bool,
}

impl RxWindow {
    pub fn new() -> Self {
        Self {
            max: 0,
            bitmap: 0,
            empty: true,
        }
    }

    /// Returns true (and commits) if the counter is fresh; false on
    /// duplicates and on counters older than the window.
    pub fn check_and_commit(&mut self, counter: u32) -> bool {
        if self.empty {
            self.empty = false;
            self.max = counter;
            self.bitmap = 0;
            return true;
        }
        if counter > self.max {
            let delta = counter - self.max;
            self.bitmap = if delta >= 32 {
                0
            } else {
                (self.bitmap << delta) | (1 << (delta - 1))
            };
            self.max = counter;
            return true;
        }
        if counter == self.max {
            return false;
        }
        let offset = self.max - counter; // >= 1
        if offset > 32 {
            return false;
        }
        let bit = 1u32 << (offset - 1);
        if self.bitmap & bit != 0 {
            return false;
        }
        self.bitmap |= bit;
        true
    }
}

impl Default for RxWindow {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_counter_starts_in_spec_range_and_increments() {
        let mut c = TxCounter::new_random();
        let a = c.next();
        let b = c.next();
        assert!((1..=(1u32 << 28)).contains(&a));
        assert_eq!(b, a + 1);
    }

    #[test]
    fn rx_window_accepts_fresh_rejects_duplicates() {
        let mut w = RxWindow::new();
        assert!(w.check_and_commit(100));
        assert!(!w.check_and_commit(100));
        assert!(w.check_and_commit(101));
        assert!(!w.check_and_commit(101));
        assert!(!w.check_and_commit(100));
    }

    #[test]
    fn rx_window_accepts_out_of_order_within_window() {
        let mut w = RxWindow::new();
        assert!(w.check_and_commit(100));
        assert!(w.check_and_commit(105));
        assert!(w.check_and_commit(103)); // 窓内・未見
        assert!(!w.check_and_commit(103)); // 二度目は重複
        assert!(!w.check_and_commit(100)); // commit 済み
        assert!(w.check_and_commit(104));
    }

    #[test]
    fn rx_window_rejects_too_old() {
        let mut w = RxWindow::new();
        assert!(w.check_and_commit(1000));
        assert!(!w.check_and_commit(1000 - 33)); // 窓幅 32 の外
        assert!(w.check_and_commit(1000 - 32)); // ちょうど窓の端は受理
    }

    #[test]
    fn rx_window_survives_large_jump() {
        let mut w = RxWindow::new();
        assert!(w.check_and_commit(10));
        assert!(w.check_and_commit(10_000));
        assert!(!w.check_and_commit(10_000));
        assert!(!w.check_and_commit(10)); // 窓外に落ちた
    }
}
