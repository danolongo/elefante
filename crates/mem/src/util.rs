use std::{
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// Maps `items` through `f` on a bounded number of scoped threads, preserving order.
///
/// # Panics
/// Only if a worker thread panics, which would leave a result slot unfilled.
pub(crate) fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let slots: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .min(8)
        .min(items.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else { break };
                    *slots[index].lock().expect("slot lock") = Some(f(item));
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .expect("slot lock")
                .expect("every slot is filled")
        })
        .collect()
}
