use std::collections::VecDeque;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use anyhow::Result;
use futures::FutureExt;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::warn;

#[derive(Clone, Copy)]
pub(crate) struct RestartPolicy {
    pub delay: Duration,
    pub failures: usize,
    pub window: Duration,
    pub cooldown: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            delay: Duration::from_secs(5),
            failures: 10,
            window: Duration::from_secs(300),
            cooldown: Duration::from_secs(900),
        }
    }
}

struct FailureWindow {
    policy: RestartPolicy,
    failures: VecDeque<Instant>,
}

impl FailureWindow {
    fn record(&mut self, now: Instant) -> Duration {
        while self
            .failures
            .front()
            .is_some_and(|time| now.duration_since(*time) >= self.policy.window)
        {
            self.failures.pop_front();
        }
        self.failures.push_back(now);
        if self.failures.len() >= self.policy.failures {
            self.failures.clear();
            self.policy.cooldown
        } else {
            self.policy.delay
        }
    }
}

/// Restart one independently owned task. The factory is called again for each
/// attempt, including after a panic or unexpected successful return.
pub(crate) async fn supervise<F, Fut>(
    kind: &'static str,
    token: &CancellationToken,
    policy: RestartPolicy,
    mut factory: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut failures = FailureWindow {
        policy,
        failures: VecDeque::new(),
    };
    loop {
        if token.is_cancelled() {
            return;
        }
        let outcome = AssertUnwindSafe(async { factory().await })
            .catch_unwind()
            .await;
        if token.is_cancelled() {
            return;
        }
        match outcome {
            Ok(Ok(())) => warn!(task = kind, "task exited unexpectedly; restarting"),
            Ok(Err(error)) => warn!(task = kind, %error, "task failed; restarting"),
            Err(_) => warn!(task = kind, "task panicked; restarting"),
        }
        let delay = failures.record(Instant::now());
        if delay == policy.cooldown {
            warn!(
                task = kind,
                cooldown_secs = delay.as_secs(),
                "restart circuit breaker cooling down"
            );
        }
        tokio::select! {
            _ = token.cancelled() => return,
            _ = tokio::time::sleep(delay) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn policy() -> RestartPolicy {
        RestartPolicy {
            delay: Duration::from_millis(1),
            failures: 10,
            window: Duration::from_secs(1),
            cooldown: Duration::from_millis(20),
        }
    }

    #[test]
    fn circuit_window_counts_errors_and_expires_old_failures() {
        let mut window = FailureWindow {
            policy: RestartPolicy {
                failures: 2,
                ..policy()
            },
            failures: VecDeque::new(),
        };
        let start = Instant::now();
        assert_eq!(window.record(start), window.policy.delay);
        assert_eq!(
            window.record(start + Duration::from_secs(2)),
            window.policy.delay
        );
        assert_eq!(
            window.record(start + Duration::from_secs(2)),
            window.policy.cooldown
        );
        assert!(window.failures.is_empty());
    }

    #[tokio::test]
    async fn one_task_recovers_from_error_panic_and_clean_exit_without_restarting_its_peer() {
        let token = CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let healthy = Arc::new(AtomicUsize::new(0));
        let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
        let failing_token = token.clone();
        let failing_calls = calls.clone();
        let failing = tokio::spawn(async move {
            supervise("watcher", &failing_token, policy(), || async {
                let attempt = failing_calls.fetch_add(1, Ordering::SeqCst);
                started.send(attempt).unwrap();
                match attempt {
                    0 => anyhow::bail!("injected error"),
                    1 => panic!("injected panic"),
                    2 => Ok(()),
                    _ => {
                        failing_token.cancelled().await;
                        Ok(())
                    }
                }
            })
            .await;
        });
        let healthy_token = token.clone();
        let healthy_calls = healthy.clone();
        let peer = tokio::spawn(async move {
            supervise("peer watcher", &healthy_token, policy(), || async {
                healthy_calls.fetch_add(1, Ordering::SeqCst);
                healthy_token.cancelled().await;
                Ok(())
            })
            .await;
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            for expected in 0..4 {
                assert_eq!(starts.recv().await.unwrap(), expected);
            }
        })
        .await
        .unwrap();
        assert_eq!(healthy.load(Ordering::SeqCst), 1);
        token.cancel();
        failing.await.unwrap();
        peer.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn cancellation_interrupts_cooldown_and_prevents_another_attempt() {
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = calls.clone();
        let (failed, failure) = tokio::sync::oneshot::channel();
        let mut failed = Some(failed);
        let worker = tokio::spawn(async move {
            supervise(
                "manager",
                &worker_token,
                RestartPolicy {
                    failures: 1,
                    cooldown: Duration::from_secs(60),
                    ..policy()
                },
                || {
                    worker_calls.fetch_add(1, Ordering::SeqCst);
                    failed.take().unwrap().send(()).unwrap();
                    async { anyhow::bail!("injected failure") }
                },
            )
            .await;
        });
        failure.await.unwrap();
        token.cancel();
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
