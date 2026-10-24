//! 可在运行时调整的并发闸门。
//!
//! `tokio::sync::Semaphore` 只能加 permit，不能收回去，改不了上限；
//! 这里用「活跃计数 + 上限 + 唤醒」自己实现一个：
//!
//! - `acquire()` 等到 `active < limit` 才放行，返回的 guard 在 drop 时归还
//! - `set(n)` 立刻改上限：调大时唤醒等待者，调小时不再放新的进来（已在跑的不打断）

use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::Notify;

pub struct Limit {
    limit: AtomicUsize,
    active: AtomicUsize,
    notify: Notify,
}

impl Limit {
    pub fn new(n: usize) -> Self {
        Self {
            limit: AtomicUsize::new(n.max(1)),
            active: AtomicUsize::new(0),
            notify: Notify::new(),
        }
    }

    pub fn get(&self) -> usize {
        self.limit.load(Ordering::SeqCst)
    }

    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    pub fn set(&self, n: usize) {
        self.limit.store(n.max(1), Ordering::SeqCst);
        // 调大时把等待中的任务叫醒
        self.notify.notify_waiters();
    }

    pub async fn acquire(&self) -> Guard<'_> {
        loop {
            let active = self.active.load(Ordering::SeqCst);
            if active < self.get() {
                if self
                    .active
                    .compare_exchange(active, active + 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    return Guard { limit: self };
                }
                continue;
            }
            // 名额满了：等 set() 或别的任务归还
            self.notify.notified().await;
        }
    }
}

pub struct Guard<'a> {
    limit: &'a Limit,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.limit.active.fetch_sub(1, Ordering::SeqCst);
        self.limit.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn respects_limit_and_shrinks() {
        let l = Limit::new(2);
        let g1 = l.acquire().await;
        let _g2 = l.acquire().await;
        assert_eq!(l.active(), 2);
        // 缩到 1：已占用的不打断，但不再放新的进来
        l.set(1);
        assert_eq!(l.get(), 1);
        drop(g1);
        assert_eq!(l.active(), 1);
        // 调大后能立刻拿到
        l.set(4);
        let _g3 = l.acquire().await;
        assert_eq!(l.active(), 2);
    }
}
