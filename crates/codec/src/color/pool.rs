//! Threads kept for conversion: starting one costs 50–170 µs on a
//! Neoverse N1, a tenth of converting a whole 1080p frame on four cores,
//! and a frame with a large change needs up to three.

use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};

/// One call of [`run`]'s `f`, for one index.
struct Task {
    f: *const (dyn Fn(usize) + Sync),
    index: usize,
    latch: Arc<Latch>,
}

// SAFETY: `f` is `Sync`, and `run` does not return (or unwind) before
// every task has finished with it.
unsafe impl Send for Task {}

#[derive(Default)]
struct Latch {
    state: Mutex<(usize, bool)>,
    done: Condvar,
}

impl Latch {
    fn finish(&self, panicked: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 -= 1;
        state.1 |= panicked;
        self.done.notify_all();
    }
}

struct Pool {
    tasks: Mutex<mpsc::Sender<Task>>,
    queue: Arc<Mutex<mpsc::Receiver<Task>>>,
    workers: Mutex<usize>,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        Pool {
            tasks: Mutex::new(tx),
            queue: Arc::new(Mutex::new(rx)),
            workers: Mutex::new(0),
        }
    })
}

/// Runs `f(0)`, …, `f(n - 1)`, `f(0)` on this thread and the rest on
/// kept threads, and returns when all have. A panic in any is resumed here.
pub fn run(n: usize, f: &(dyn Fn(usize) + Sync)) {
    if n <= 1 {
        if n == 1 {
            f(0);
        }
        return;
    }
    let pool = pool();
    {
        let mut workers = pool.workers.lock().unwrap_or_else(|e| e.into_inner());
        while *workers < n - 1 {
            let queue = pool.queue.clone();
            let spawned = std::thread::Builder::new()
                .name("latch-color".into())
                .spawn(move || loop {
                    let task = match queue.lock() {
                        Ok(queue) => queue.recv(),
                        Err(_) => return,
                    };
                    let Ok(task) = task else { return };
                    // SAFETY: `run` waits for this task before `f` goes.
                    let f = unsafe { &*task.f };
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(task.index)));
                    task.latch.finish(result.is_err());
                });
            if spawned.is_err() {
                break;
            }
            *workers += 1;
        }
    }
    let latch = Arc::new(Latch::default());
    // Erases `f`'s lifetime: every task is waited for below, even when
    // `f(0)` panics or a task cannot be sent (then it is run here).
    // SAFETY: only the lifetime changes; the pointer is not used after
    // the wait.
    let erased: *const (dyn Fn(usize) + Sync) = unsafe {
        std::mem::transmute::<&(dyn Fn(usize) + Sync), &'static (dyn Fn(usize) + Sync)>(f)
    };
    let mut local = Vec::new();
    {
        let tasks = pool.tasks.lock().unwrap_or_else(|e| e.into_inner());
        for index in 1..n {
            latch.state.lock().unwrap_or_else(|e| e.into_inner()).0 += 1;
            let task = Task {
                f: erased,
                index,
                latch: latch.clone(),
            };
            if let Err(mpsc::SendError(task)) = tasks.send(task) {
                latch.finish(false);
                local.push(task.index);
            }
        }
    }
    struct Wait<'a>(&'a Latch);
    impl Drop for Wait<'_> {
        fn drop(&mut self) {
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.0 > 0 {
                state = self.0.done.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        }
    }
    let wait = Wait(&latch);
    f(0);
    for index in local {
        f(index);
    }
    drop(wait);
    let panicked = latch.state.lock().unwrap_or_else(|e| e.into_inner()).1;
    if panicked {
        panic!("a conversion thread panicked");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn every_index_runs_once_and_borrows_are_safe() {
        for n in [0, 1, 2, 4, 16, 3] {
            let counts: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(0)).collect();
            run(n, &|i| {
                counts[i].fetch_add(1, Ordering::Relaxed);
            });
            assert!(counts.iter().all(|c| c.load(Ordering::Relaxed) == 1), "{n}");
        }
    }

    #[test]
    fn a_panic_on_a_kept_thread_reaches_the_caller_after_all_finish() {
        let finished = AtomicUsize::new(0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(4, &|i| {
                if i == 2 {
                    panic!("band 2");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                finished.fetch_add(1, Ordering::Relaxed);
            })
        }));
        assert!(result.is_err());
        assert_eq!(finished.load(Ordering::Relaxed), 3);
        // The pool still works.
        let n = AtomicUsize::new(0);
        run(4, &|_| {
            n.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(n.load(Ordering::Relaxed), 4);
    }
}
