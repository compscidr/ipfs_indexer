use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{error, info};

/// Run the crawl workers forever, keeping as many as `target()` says (#304): `work` handles at
/// most one page and says whether it took one
pub fn supervise(
    target: impl Fn() -> usize + Send + 'static,
    tick: Duration,
    work: Arc<dyn Fn() -> bool + Send + Sync>,
) {
    run(Arc::new(Mutex::new(BTreeSet::new())), target, tick, work)
}

fn run(
    set: Arc<Mutex<BTreeSet<usize>>>,
    target: impl Fn() -> usize,
    tick: Duration,
    work: Arc<dyn Fn() -> bool + Send + Sync>,
) {
    // the workers read the target through this, set each tick, since `target` stays here
    let wanted = Arc::new(AtomicUsize::new(0));
    let mut last = None;
    loop {
        let want = target();
        wanted.store(want, Ordering::SeqCst);
        match last {
            None => info!("Crawl workers: {}", want),
            Some(was) if was != want => info!("Crawl workers: {} \u{2192} {}", was, want),
            _ => {}
        }
        last = Some(want);
        for id in 0..want {
            // marked before the thread starts, so a slow start isn't started twice
            if !lock(&set).insert(id) {
                continue;
            }
            let running = Running {
                id,
                set: Arc::clone(&set),
            };
            let (work, wanted) = (Arc::clone(&work), Arc::clone(&wanted));
            let started = std::thread::Builder::new()
                .name(format!("crawl-{}", id))
                .spawn(move || {
                    let _running = running;
                    worker(id, &wanted, &*work);
                });
            if let Err(err) = started {
                // the unrun closure was dropped with its guard, which unmarked the id for a retry
                error!("Couldn't start crawl worker {}: {}", id, err);
            }
        }
        std::thread::sleep(tick);
    }
}

/// Take pages while this worker is still wanted; the target is checked only between pages, so a
/// shrink never drops one mid-fetch
fn worker(id: usize, wanted: &AtomicUsize, work: &(dyn Fn() -> bool + Send + Sync)) {
    while id < wanted.load(Ordering::SeqCst) {
        if !work() {
            // idle workers spun at 100% CPU each (#224)
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Unmarks a worker's id when its thread ends, by returning or by panic, so the supervisor
/// starts it again on a later tick
struct Running {
    id: usize,
    set: Arc<Mutex<BTreeSet<usize>>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        if std::thread::panicking() {
            error!("Crawl worker {} panicked; starting it again", self.id);
        }
        lock(&self.set).remove(&self.id);
    }
}

// the set is only touched in short sections that can't panic, but a poisoned lock mustn't stop
// the crawl
fn lock(set: &Mutex<BTreeSet<usize>>) -> std::sync::MutexGuard<'_, BTreeSet<usize>> {
    set.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How many workers are running
#[cfg(test)]
pub fn running(set: &Mutex<BTreeSet<usize>>) -> usize {
    lock(set).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    const TICK: Duration = Duration::from_millis(20);

    fn wait_for(what: &str, within: Duration, done: impl Fn() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < within, "timed out waiting for {}", what);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// supervise on its own thread, with the target in an atomic the test changes
    fn start(
        target: usize,
        work: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> (Arc<AtomicUsize>, Arc<Mutex<BTreeSet<usize>>>) {
        let target = Arc::new(AtomicUsize::new(target));
        let set = Arc::new(Mutex::new(BTreeSet::new()));
        let (t, s) = (Arc::clone(&target), Arc::clone(&set));
        std::thread::spawn(move || run(s, move || t.load(Ordering::SeqCst), TICK, work));
        (target, set)
    }

    #[test]
    fn workers_grow_and_shrink_to_the_target() {
        let work: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(|| {
            std::thread::sleep(Duration::from_millis(50));
            true
        });
        let (target, set) = start(3, work);
        wait_for("3 running", Duration::from_secs(1), || running(&set) == 3);
        target.store(1, Ordering::SeqCst);
        wait_for("1 running", Duration::from_secs(1), || running(&set) == 1);
        target.store(4, Ordering::SeqCst);
        wait_for("4 running", Duration::from_secs(1), || running(&set) == 4);
        assert_eq!(*set.lock().unwrap(), (0..4).collect());
        target.store(0, Ordering::SeqCst);
        wait_for("none running", Duration::from_secs(1), || {
            running(&set) == 0
        });
    }

    #[test]
    fn no_page_is_lost_when_shrinking() {
        let queue = Arc::new(Mutex::new((0..50).collect::<Vec<usize>>()));
        let done = Arc::new(Mutex::new(Vec::new()));
        let work: Arc<dyn Fn() -> bool + Send + Sync> = {
            let (queue, done) = (Arc::clone(&queue), Arc::clone(&done));
            Arc::new(move || {
                let Some(page) = queue.lock().unwrap().pop() else {
                    return false;
                };
                std::thread::sleep(Duration::from_millis(20));
                done.lock().unwrap().push(page);
                true
            })
        };
        let (target, set) = start(5, work);
        wait_for("5 running", Duration::from_secs(1), || running(&set) == 5);
        wait_for("some pages done", Duration::from_secs(1), || {
            done.lock().unwrap().len() >= 5
        });
        target.store(1, Ordering::SeqCst);
        wait_for("1 running", Duration::from_secs(1), || running(&set) == 1);
        assert!(
            !queue.lock().unwrap().is_empty(),
            "shrank after the queue ran out"
        );
        wait_for("queue empty", Duration::from_secs(5), || {
            queue.lock().unwrap().is_empty() && done.lock().unwrap().len() == 50
        });
        let mut done = done.lock().unwrap().clone();
        done.sort();
        assert_eq!(done, (0..50).collect::<Vec<_>>());
        target.store(0, Ordering::SeqCst);
    }

    #[test]
    fn a_panicking_worker_is_replaced() {
        thread_local!(static SEEN: Cell<bool> = const { Cell::new(false) });
        let threads = Arc::new(AtomicUsize::new(0));
        let panicked = Arc::new(AtomicBool::new(false));
        let work: Arc<dyn Fn() -> bool + Send + Sync> = {
            let (threads, panicked) = (Arc::clone(&threads), Arc::clone(&panicked));
            Arc::new(move || {
                if !SEEN.with(|seen| seen.replace(true)) {
                    threads.fetch_add(1, Ordering::SeqCst);
                }
                if !panicked.swap(true, Ordering::SeqCst) {
                    panic!("a page that panics");
                }
                std::thread::sleep(Duration::from_millis(10));
                true
            })
        };
        let (target, set) = start(2, work);
        // two workers plus the one started in place of the one that panicked
        wait_for("a replacement worker", Duration::from_secs(1), || {
            threads.load(Ordering::SeqCst) == 3
        });
        assert!(panicked.load(Ordering::SeqCst));
        wait_for("2 running", Duration::from_secs(1), || running(&set) == 2);
        target.store(0, Ordering::SeqCst);
    }
}
