//! A thread that owns a value and serialises access to it.
//!
//! `rusqlite::Connection` is not `Sync` and SQLite calls block, so connections live on dedicated
//! threads and the async runtime talks to them through this type. It also gives one place to
//! count and time calls.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};

use tokio::sync::oneshot;

use crate::error::MemError;

type Job<S> = Box<dyn FnOnce(&mut S) + Send>;

pub struct Actor<S> {
    tx: Option<mpsc::Sender<Job<S>>>,
    thread: Option<JoinHandle<()>>,
    calls: AtomicU64,
}

impl<S: Send + 'static> Actor<S> {
    /// Starts a named thread that owns `state`.
    ///
    /// # Panics
    /// Panics if the operating system cannot spawn a thread.
    pub fn spawn(name: &str, mut state: S) -> Self {
        let (tx, rx) = mpsc::channel::<Job<S>>();
        let thread = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    job(&mut state);
                }
            })
            .expect("spawn memory actor thread");
        Self {
            tx: Some(tx),
            thread: Some(thread),
            calls: AtomicU64::new(0),
        }
    }

    fn enqueue<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut S) -> R + Send + 'static,
    ) -> Result<oneshot::Receiver<Result<R, MemError>>, MemError> {
        let (reply, receiver) = oneshot::channel();
        let job: Job<S> = Box::new(move |state| {
            // A panicking request must not take the thread, and every later caller, with it.
            let outcome = catch_unwind(AssertUnwindSafe(|| f(state)));
            let _ = reply.send(outcome.map_err(|_| MemError::ActorPanicked));
        });
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.tx
            .as_ref()
            .ok_or(MemError::ActorGone)?
            .send(job)
            .map_err(|_| MemError::ActorGone)?;
        Ok(receiver)
    }

    /// Runs `f` on the actor's thread and awaits its result.
    ///
    /// # Errors
    /// [`MemError::ActorGone`] if the thread has exited, [`MemError::ActorPanicked`] if `f` panicked.
    pub async fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut S) -> R + Send + 'static,
    ) -> Result<R, MemError> {
        self.enqueue(f)?
            .await
            .map_err(|_| MemError::ActorGone)
            .and_then(|outcome| outcome)
    }

    /// As [`Actor::call`], blocking the calling thread. Not for use inside an async context.
    ///
    /// # Errors
    /// See [`Actor::call`].
    pub fn call_blocking<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut S) -> R + Send + 'static,
    ) -> Result<R, MemError> {
        self.enqueue(f)?
            .blocking_recv()
            .map_err(|_| MemError::ActorGone)
            .and_then(|outcome| outcome)
    }

    /// Requests handed to this actor so far. Gates assert on this rather than on timing.
    #[must_use]
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}

impl<S> Drop for Actor<S> {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
