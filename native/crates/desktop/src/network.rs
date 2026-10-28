//! The Tokio runtime that owns network work, bridged to GPUI.
//!
//! GPUI's executor owns UI work and Tokio owns network work; neither waits
//! synchronously for the other. A GPUI task awaits a [`NetworkTask`], and
//! dropping that task aborts the network work, so closing a view or the
//! window releases its requests.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::runtime::{EnterGuard, Runtime};
use tokio::task::JoinHandle;

/// Worker threads for network I/O; requests are few and mostly waiting.
const WORKER_THREADS: usize = 2;

/// The network runtime of one server connection.
#[derive(Debug)]
pub struct Network {
    runtime: Option<Runtime>,
}

impl Network {
    pub fn new() -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(WORKER_THREADS)
            .thread_name("openchamber-network")
            .enable_all()
            .build()?;
        Ok(Self {
            runtime: Some(runtime),
        })
    }

    /// Runs `future` on the network runtime.
    pub fn run<F>(&self, future: F) -> NetworkTask<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        NetworkTask {
            handle: self.runtime().spawn(future),
        }
    }

    /// Makes the runtime current, for constructors that spawn their own tasks.
    pub fn enter(&self) -> EnterGuard<'_> {
        self.runtime().enter()
    }

    /// Captures the original executor while a durable-send barrier is pending.
    pub(crate) fn handle(&self) -> tokio::runtime::Handle {
        self.runtime().handle().clone()
    }

    fn runtime(&self) -> &Runtime {
        self.runtime
            .as_ref()
            .expect("the runtime lives until the network is dropped")
    }
}

impl Drop for Network {
    // The UI thread never waits for network threads to wind down.
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// Network work awaited from GPUI. Dropping it aborts the work.
#[derive(Debug)]
#[must_use = "dropping a network task cancels it"]
pub struct NetworkTask<T> {
    handle: JoinHandle<T>,
}

impl<T: Send + 'static> NetworkTask<T> {
    pub(crate) fn spawn_on(
        handle: &tokio::runtime::Handle,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Self {
        Self {
            handle: handle.spawn(future),
        }
    }
}

impl<T> Future for NetworkTask<T> {
    /// `None` when the work was cancelled or panicked.
    type Output = Option<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.handle)
            .poll(cx)
            .map(|result| match result {
                Ok(value) => Some(value),
                Err(error) => {
                    if error.is_panic() {
                        tracing::error!("a network task panicked");
                    }
                    None
                }
            })
    }
}

impl<T> Drop for NetworkTask<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Waker;
    use std::time::Duration;

    use super::*;

    /// Polls from outside Tokio, as GPUI's executor does.
    fn wait<T>(mut task: NetworkTask<T>) -> Option<T> {
        let mut context = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(value) = Pin::new(&mut task).poll(&mut context) {
                return value;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn results_cross_executors_and_dropping_cancels() {
        let network = Network::new().expect("the runtime starts");
        assert_eq!(wait(network.run(async { 7 })), Some(7));

        let finished = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&finished);
        let task = network.run(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            flag.store(true, Ordering::SeqCst);
        });
        drop(task);
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !finished.load(Ordering::SeqCst),
            "a dropped task never completes"
        );
    }
}
