//! Poison 耐性のあるロック取得ヘルパ。
//!
//! `Mutex` / `RwLock` の poison は「他スレッドがガード保持中に panic した」
//! 事実の伝搬でしかない。ここで `.expect()` すると 1 スレッドの panic が
//! 全経路（op 実行 / mDNS 広告 / commissioning サーバ）へ連鎖するため、
//! panic させるより回収する（安定性監査 Tier 3 と同裁定 — matd
//! `SubHealth` の先行例を全クレートへ共通化したもの）。
//!
//! 回収が無条件に正しいのは guard 跨ぎの複合不変条件を持たない状態
//! （SubHealth のテーブル群、mDNS 広告スロット、dnssd opcache など）。
//! 複合状態を 1 つの guard に載せている呼び出し側（commissioning サーバの
//! `Inner`: pending 鍵材料 + fabric store + fail-safe）は、途中まで書けた
//! 状態の後始末を自前の巻き戻し（fail-safe 失効 →
//! `rollback_uncommitted_fabric`）に負っており、このヘルパはそれを
//! 肩代わりしない。

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// `Mutex::lock` の poison 回収版。
pub fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `RwLock::read` の poison 回収版。
pub fn read_locked<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(PoisonError::into_inner)
}

/// `RwLock::write` の poison 回収版。
pub fn write_locked<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// ガード保持中に panic させて poison 状態を作る。
    fn poison_mutex(m: &Arc<Mutex<u32>>) {
        let m2 = Arc::clone(m);
        let _ = std::thread::spawn(move || {
            let _guard = m2.lock().unwrap();
            panic!("poison on purpose");
        })
        .join();
        assert!(m.is_poisoned(), "precondition: mutex must be poisoned");
    }

    fn poison_rwlock(l: &Arc<RwLock<u32>>) {
        let l2 = Arc::clone(l);
        let _ = std::thread::spawn(move || {
            let _guard = l2.write().unwrap();
            panic!("poison on purpose");
        })
        .join();
        assert!(l.is_poisoned(), "precondition: rwlock must be poisoned");
    }

    #[test]
    fn locked_returns_guard_on_clean_mutex() {
        let m = Mutex::new(7u32);
        *locked(&m) += 1;
        assert_eq!(*locked(&m), 8);
    }

    #[test]
    fn locked_recovers_poisoned_mutex() {
        let m = Arc::new(Mutex::new(42u32));
        poison_mutex(&m);
        assert_eq!(*locked(&m), 42);
        *locked(&m) = 43;
        assert_eq!(*locked(&m), 43);
    }

    #[test]
    fn read_locked_recovers_poisoned_rwlock() {
        let l = Arc::new(RwLock::new(42u32));
        poison_rwlock(&l);
        assert_eq!(*read_locked(&l), 42);
    }

    #[test]
    fn write_locked_recovers_poisoned_rwlock() {
        let l = Arc::new(RwLock::new(42u32));
        poison_rwlock(&l);
        *write_locked(&l) = 43;
        assert_eq!(*read_locked(&l), 43);
    }
}
