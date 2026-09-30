//! Bounded fan-out over priority-ordered work items.
//!
//! The collectors use this for per-repository and per-remote jobs: items are
//! ordered by value (newest activity first) and must start in that order,
//! while every result should reach the caller the moment it finishes rather
//! than when the slowest job ends. `rayon`'s `par_iter` gives neither - its
//! work-stealing scheduler ignores input order and `collect` returns at the
//! end - so the pool is a few scoped threads pulling a shared index.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};

/// The worker cap the collectors run at: wide enough for a machine's
/// repositories and remote round-trips, narrow enough to keep subprocess
/// pressure bounded. Chosen empirically - parallel `ls-remote` saturated the
/// wall-clock win well below this.
pub const WORKERS: usize = 16;

/// Run `work` over `items` on at most `workers` scoped threads, calling
/// `each` with `(index, result)` in completion order on the calling thread.
///
/// Workers pull the next index off a shared counter, so input order is the
/// order jobs are *taken* in: item 0 starts before item 1 and so on. Results
/// stream back as each job finishes - a slow high-priority job never delays
/// delivery of the ones behind it.
///
/// A panic inside `work` propagates out of `fan_out`: the panic's worker
/// stops, the rest of the pool drains the remaining jobs, and the panic is
/// resumed once `std::thread::scope` joins. A panic inside `each` unwinds
/// immediately; the workers then exit early since the result channel is
/// gone - no result blocks on send because the channel is unbounded.
pub fn fan_out<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    work: impl Fn(&T) -> R + Sync,
    each: impl FnMut(usize, R),
) {
    if items.is_empty() {
        return;
    }
    let workers = workers.clamp(1, items.len());
    let next = AtomicUsize::new(0);
    let (tx, rx) = channel::<(usize, R)>();
    std::thread::scope(|s| {
        for _ in 0..workers {
            let tx = tx.clone();
            let work = &work;
            let next = &next;
            s.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= items.len() {
                        return;
                    }
                    // A send only fails when the receiver is gone - `each`
                    // panicked - so further results can only be dropped.
                    if tx.send((i, work(&items[i]))).is_err() {
                        return;
                    }
                }
            });
        }
        drop(tx);
        drain(rx, each);
    });
}

/// The consumer half: deliver results until every worker's sender is gone.
fn drain<R>(rx: Receiver<(usize, R)>, mut each: impl FnMut(usize, R)) {
    for (i, result) in rx {
        each(i, result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    #[test]
    fn every_item_runs_once_and_streams_back() {
        let items: Vec<u64> = (0..24).collect();
        let started = Mutex::new(Vec::new());
        let finished = Mutex::new(Vec::new());
        fan_out(
            &items,
            WORKERS,
            |item| {
                started.lock().unwrap().push(*item);
                // Item 0 is slow: a serial or LIFO pool would deliver it
                // last, a streaming pool delivers the quick tail first.
                std::thread::sleep(if *item == 0 {
                    Duration::from_millis(150)
                } else {
                    Duration::from_millis(10)
                });
                *item * 2
            },
            |index, result| {
                finished.lock().unwrap().push((index, result));
            },
        );
        let started = started.into_inner().unwrap();
        // Every item ran exactly once.
        let mut sorted = started.clone();
        sorted.sort();
        assert_eq!(sorted, items);
        // Priority order is start order: no item ahead of 0 - which is the
        // *last taken* index only when the pool is wide enough - starts
        // before the first `WORKERS` items were all taken. Item 23 can only
        // be taken after 0..23 were taken, so every starter index < 23 must
        // precede it.
        let last = started.iter().position(|i| *i == 23).unwrap();
        for i in 0..16 {
            assert!(
                started[..last].contains(&i),
                "item {i} was taken after a lower-priority item: {started:?}"
            );
        }
        // Results arrived per completion, not in input order: the slow item
        // 0 finished last even though it started first.
        let finished = finished.into_inner().unwrap();
        assert_eq!(finished.len(), items.len());
        assert_eq!(finished.last().unwrap().0, 0, "{finished:?}");
        for (i, result) in &finished {
            assert_eq!(*result, *i as u64 * 2);
        }
    }

    #[test]
    fn a_single_worker_is_strictly_sequential() {
        let order = Mutex::new(Vec::new());
        fan_out(
            &[1usize, 2, 3, 4],
            1,
            |item| {
                order.lock().unwrap().push(*item);
                *item
            },
            |_, _| {},
        );
        assert_eq!(*order.lock().unwrap(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn empty_input_spawns_nothing() {
        let items: Vec<u8> = Vec::new();
        let ran = AtomicBool::new(false);
        fan_out(
            &items,
            WORKERS,
            |_| ran.store(true, Ordering::Relaxed), // coverage: off - empty input invokes no work
            |_, _| {},                              // coverage: off - and streams no results
        );
        assert!(!ran.load(Ordering::Relaxed));
    }

    #[test]
    fn results_stream_before_slow_jobs_end() {
        let slow_done = AtomicBool::new(false);
        let fast_seen_while_slow_ran = AtomicBool::new(false);
        fan_out(
            &[0usize, 1],
            2,
            |item| {
                if *item == 0 {
                    std::thread::sleep(Duration::from_millis(150));
                    slow_done.store(true, Ordering::Relaxed);
                }
                *item
            },
            |index, _| {
                if index == 1 && !slow_done.load(Ordering::Relaxed) {
                    fast_seen_while_slow_ran.store(true, Ordering::Relaxed);
                }
            },
        );
        assert!(fast_seen_while_slow_ran.load(Ordering::Relaxed));
    }

    #[test]
    fn a_worker_panic_propagates() {
        let started = Instant::now();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fan_out(
                &[0usize, 1, 2],
                2,
                |item| {
                    // The surviving worker keeps going: it must not be
                    // abandoned mid-pool, so give it real work to finish.
                    std::thread::sleep(Duration::from_millis(20));
                    assert_ne!(*item, 1, "boom");
                    *item
                },
                |_, _| {},
            );
        }));
        assert!(panic.is_err());
        // The pool joined rather than detaching: this assertion runs only
        // after every worker finished or panicked.
        assert!(started.elapsed() >= Duration::from_millis(20));
    }
}
