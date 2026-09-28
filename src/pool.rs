//! One pool of workers for a whole target.
//!
//! Both languages and every edition group draw from the same queue, so `-j N`
//! means N units of work in flight rather than N per half. The jobs are built
//! before any worker starts, which is what lets a chunk carry a borrowed
//! configuration path and what makes the ordering -- heaviest first -- the
//! scheduling decision it is.

use std::{
    any::Any,
    collections::VecDeque,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

/// One unit of work. The `Control` handle is how a job that fails stops the
/// ones that have not started yet.
pub type Job<'a, S> = Box<dyn FnOnce(&mut S, &Control) + Send + 'a>;

/// Whatever a panicking worker carried, for the caller to turn into its own
/// error type.
pub type Panic = Box<dyn Any + Send + 'static>;

#[derive(Debug, Default)]
pub struct Control {
    stop: AtomicBool,
}

impl Control {
    /// Stop scheduling. Work already running finishes: a formatter that
    /// abandoned a half-written file would be worse than one that did too
    /// much.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

/// Run `jobs` across at most `workers` threads and return each worker's state.
///
/// The calling thread is one of the workers, so a run never has more threads
/// alive than it was asked for.
pub fn run<S: Send>(
    jobs: Vec<Job<'_, S>>,
    workers: usize,
    new_state: impl Fn() -> S,
) -> (Vec<S>, Vec<Panic>) {
    if jobs.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let workers = workers.max(1).min(jobs.len());
    let queue = Mutex::new(VecDeque::from(jobs));
    let control = Control::default();

    thread::scope(|scope| {
        let handles: Vec<_> = (1..workers)
            .map(|_| {
                let mut state = new_state();
                let queue = &queue;
                let control = &control;
                scope.spawn(move || {
                    work(queue, control, &mut state);
                    state
                })
            })
            .collect();

        let mut own = new_state();
        work(&queue, &control, &mut own);

        let mut states = vec![own];
        let mut panics = Vec::new();
        for handle in handles {
            match handle.join() {
                Ok(state) => states.push(state),
                Err(payload) => panics.push(payload),
            }
        }
        (states, panics)
    })
}

fn work<S>(queue: &Mutex<VecDeque<Job<'_, S>>>, control: &Control, state: &mut S) {
    loop {
        if control.stopped() {
            return;
        }
        let job = queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        let Some(job) = job else { return };
        job(state, control);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn every_job_runs_exactly_once() {
        let seen = AtomicUsize::new(0);
        let jobs: Vec<Job<'_, usize>> = (0..1000)
            .map(|_| {
                let seen = &seen;
                Box::new(move |state: &mut usize, _: &Control| {
                    *state += 1;
                    seen.fetch_add(1, Ordering::Relaxed);
                }) as Job<'_, usize>
            })
            .collect();

        let (states, panics) = run(jobs, 8, || 0usize);
        assert!(panics.is_empty());
        assert_eq!(seen.load(Ordering::Relaxed), 1000);
        assert_eq!(states.iter().sum::<usize>(), 1000);
    }

    /// The cap is the point: a pool asked for four threads must not be five,
    /// however many jobs it is given.
    #[test]
    fn no_more_threads_are_used_than_asked_for() {
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let jobs: Vec<Job<'_, ()>> = (0..64)
            .map(|_| {
                let (live, peak) = (&live, &peak);
                Box::new(move |(): &mut (), _: &Control| {
                    let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::yield_now();
                    live.fetch_sub(1, Ordering::SeqCst);
                }) as Job<'_, ()>
            })
            .collect();

        run(jobs, 4, || ());
        assert!(peak.load(Ordering::SeqCst) <= 4);
    }

    /// Stopping must not disable parallelism, which is what the old
    /// `--fail-fast` did: it forced the serial path to get an early exit.
    #[test]
    fn stopping_leaves_running_work_alone_and_schedules_no_more() {
        let ran = AtomicUsize::new(0);
        let jobs: Vec<Job<'_, ()>> = (0..500)
            .map(|index| {
                let ran = &ran;
                Box::new(move |(): &mut (), control: &Control| {
                    ran.fetch_add(1, Ordering::SeqCst);
                    if index == 0 {
                        control.stop();
                    }
                }) as Job<'_, ()>
            })
            .collect();

        run(jobs, 4, || ());
        assert!(ran.load(Ordering::SeqCst) < 500);
    }

    #[test]
    fn an_empty_run_starts_no_threads() {
        let (states, panics) = run(Vec::<Job<'_, ()>>::new(), 8, || ());
        assert!(states.is_empty() && panics.is_empty());
    }
}
