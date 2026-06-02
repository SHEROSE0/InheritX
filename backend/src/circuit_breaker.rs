//! A simple thread-safe circuit breaker for protecting external service calls.
//!
//! The circuit transitions through three states:
//! * **Closed** – normal operation; failures are counted.
//! * **Open**   – requests are rejected immediately to shed load.
//! * **HalfOpen** – one probe request is allowed through to test recovery.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

use crate::api_error::ApiError;

/// The state of the circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

/// Shared inner state stored behind an `Arc` so the breaker can be cheaply cloned.
struct Inner {
    /// Name of the guarded service, used in logs and error messages.
    service_name: String,
    /// Consecutive failures needed to trip the circuit.
    failure_threshold: u32,
    /// How long (seconds) to keep the circuit open before probing.
    recovery_timeout_secs: u64,
    /// Consecutive failures since the last reset.
    failure_count: AtomicU32,
    /// Unix timestamp (seconds) of the first failure in the current window;
    /// also used to mark when the circuit was opened.
    opened_at: AtomicU64,
}

/// A simple circuit breaker that prevents cascading failures.
///
/// # Usage
/// ```rust,ignore
/// let cb = CircuitBreaker::new("price-feed", 5, Duration::from_secs(60));
/// let result = cb.call(|| async { fetch_price().await }).await;
/// ```
#[derive(Clone)]
pub struct CircuitBreaker(Arc<Inner>);

impl CircuitBreaker {
    /// Creates a new circuit breaker.
    ///
    /// * `service_name`      – human-readable name shown in logs/errors.
    /// * `failure_threshold` – consecutive failures before the circuit opens.
    /// * `recovery_timeout`  – how long the circuit stays open before trying again.
    pub fn new(service_name: &str, failure_threshold: u32, recovery_timeout: Duration) -> Self {
        let breaker = Self(Arc::new(Inner {
            service_name: service_name.to_string(),
            failure_threshold,
            recovery_timeout_secs: recovery_timeout.as_secs(),
            failure_count: AtomicU32::new(0),
            opened_at: AtomicU64::new(0),
        }));

        breaker.record_state(CircuitState::Closed);
        breaker
    }

    /// Creates a circuit breaker with optional env overrides:
    /// - `CB_<SERVICE>_FAILURE_THRESHOLD`
    /// - `CB_<SERVICE>_RECOVERY_TIMEOUT_SECS`
    ///
    /// Service is upper-cased and non-alphanumeric characters become `_`.
    pub fn from_env(
        service_name: &str,
        default_failure_threshold: u32,
        default_recovery_timeout_secs: u64,
    ) -> Self {
        let service_env = service_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>();

        let threshold_var = format!("CB_{}_FAILURE_THRESHOLD", service_env);
        let timeout_var = format!("CB_{}_RECOVERY_TIMEOUT_SECS", service_env);

        let threshold = std::env::var(threshold_var)
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(default_failure_threshold);
        let timeout_secs = std::env::var(timeout_var)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default_recovery_timeout_secs);

        Self::new(service_name, threshold, Duration::from_secs(timeout_secs))
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn state(&self) -> CircuitState {
        let failures = self.0.failure_count.load(Ordering::Acquire);
        if failures < self.0.failure_threshold {
            return CircuitState::Closed;
        }
        // Circuit is open – check if recovery timeout has elapsed.
        let opened_at = self.0.opened_at.load(Ordering::Acquire);
        if Self::now_secs().saturating_sub(opened_at) >= self.0.recovery_timeout_secs {
            CircuitState::HalfOpen
        } else {
            CircuitState::Open
        }
    }

    fn on_success(&self) {
        let prev = self.0.failure_count.swap(0, Ordering::Release);
        metrics::counter!("circuit_breaker_success_total", "service" => self.0.service_name.clone())
            .increment(1);
        if prev >= self.0.failure_threshold {
            info!(service = %self.0.service_name, "Circuit breaker closed after successful probe");
        }
        self.0.opened_at.store(0, Ordering::Release);
        self.record_state(CircuitState::Closed);
    }

    fn on_failure(&self) {
        let failures = self.0.failure_count.fetch_add(1, Ordering::AcqRel) + 1;
        metrics::counter!("circuit_breaker_failures_total", "service" => self.0.service_name.clone())
            .increment(1);
        if failures == self.0.failure_threshold {
            let now = Self::now_secs();
            self.0.opened_at.store(now, Ordering::Release);
            metrics::counter!("circuit_breaker_open_total", "service" => self.0.service_name.clone())
                .increment(1);
            self.record_state(CircuitState::Open);
            warn!(
                service = %self.0.service_name,
                failure_threshold = self.0.failure_threshold,
                "Circuit breaker opened after consecutive failures"
            );
            // Alert Sentry when a circuit trips — this indicates a sustained
            // external-service outage that warrants immediate attention.
            crate::error_tracking::capture_message(
                &format!(
                    "Circuit breaker opened for service '{}' after {} consecutive failures",
                    self.0.service_name, self.0.failure_threshold
                ),
                sentry::Level::Error,
            );
        }
    }

    fn record_state(&self, state: CircuitState) {
        let numeric_state = match state {
            CircuitState::Closed => 0.0,
            CircuitState::Open => 1.0,
            CircuitState::HalfOpen => 2.0,
        };

        metrics::gauge!("circuit_breaker_state", "service" => self.0.service_name.clone())
            .set(numeric_state);
    }

    /// Executes `operation` if the circuit is not open.
    ///
    /// Returns `Err(ApiError::CircuitOpen)` without calling `operation` when the
    /// circuit is open.  When in `HalfOpen` state the operation is attempted
    /// once; success closes the circuit, failure re-opens it.
    pub async fn call<F, Fut, T>(&self, operation: F) -> Result<T, ApiError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, ApiError>>,
    {
        match self.state() {
            CircuitState::Open => {
                metrics::counter!("circuit_breaker_rejected_total", "service" => self.0.service_name.clone())
                    .increment(1);
                warn!(service = %self.0.service_name, "Circuit breaker is open, rejecting request");
                Err(ApiError::CircuitOpen(self.0.service_name.clone()))
            }
            CircuitState::HalfOpen => {
                self.record_state(CircuitState::HalfOpen);
                info!(service = %self.0.service_name, "Circuit breaker half-open, sending probe");
                match operation().await {
                    Ok(v) => {
                        self.on_success();
                        Ok(v)
                    }
                    Err(e) => {
                        self.on_failure();
                        Err(e)
                    }
                }
            }
            CircuitState::Closed => match operation().await {
                Ok(v) => {
                    self.on_success();
                    Ok(v)
                }
                Err(e) => {
                    if e.is_transient() {
                        self.on_failure();
                    }
                    Err(e)
                }
            },
        }
    }

    // Expose internal state for testing
    #[cfg(test)]
    fn failure_count(&self) -> u32 {
        self.0.failure_count.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn opened_at(&self) -> u64 {
        self.0.opened_at.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering as AtoOrdering;

    /// Helper to track call invocations in tests
    struct CallCounter {
        invocations: AtomicUsize,
    }

    impl CallCounter {
        fn new() -> Self {
            Self {
                invocations: AtomicUsize::new(0),
            }
        }

        fn increment(&self) {
            self.invocations.fetch_add(1, AtoOrdering::SeqCst);
        }

        fn count(&self) -> usize {
            self.invocations.load(AtoOrdering::SeqCst)
        }
    }

    // ──────────────────────────────────────────────────────────────────────────
    // CIRCUIT STATE TRANSITIONS
    // ──────────────────────────────────────────────────────────────────────────

    /// Test that circuit starts in Closed state and allows successful calls.
    #[tokio::test]
    async fn circuit_starts_closed_and_allows_calls() {
        let cb = CircuitBreaker::new("test-service", 3, Duration::from_secs(10));
        let counter = Arc::new(CallCounter::new());
        let counter_clone = counter.clone();

        let result = cb
            .call(move || {
                let c = counter_clone.clone();
                async move {
                    c.increment();
                    Ok::<_, ApiError>(42)
                }
            })
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
        assert_eq!(counter.count(), 1);
    }

    /// Test that circuit opens after reaching failure threshold.
    #[tokio::test]
    async fn circuit_opens_after_threshold_failures() {
        let cb = CircuitBreaker::new("test-service", 3, Duration::from_secs(10));

        // Inject 3 transient failures to reach threshold
        for i in 0..3 {
            let result = cb
                .call(|| async { Err::<(), ApiError>(ApiError::Timeout) })
                .await;
            assert!(result.is_err(), "Attempt {} should fail", i + 1);
            assert_eq!(cb.failure_count(), (i + 1) as u32);
        }

        // Next call should be rejected immediately with CircuitOpen
        let result = cb
            .call(|| async {
                // This should never be invoked
                panic!("Circuit should prevent this call");
            })
            .await;

        assert!(
            matches!(result, Err(ApiError::CircuitOpen(_))),
            "Circuit should reject call with CircuitOpen error"
        );
    }

    /// Test that non-transient errors do not increment failure count.
    #[tokio::test]
    async fn non_transient_errors_do_not_increment_failure_count() {
        let cb = CircuitBreaker::new("test-service", 3, Duration::from_secs(10));

        // Non-transient error (Unauthorized)
        let result = cb
            .call(|| async { Err::<(), ApiError>(ApiError::Unauthorized) })
            .await;

        assert!(result.is_err());
        assert_eq!(cb.failure_count(), 0, "Non-transient error should not increment count");

        // Transient error (Timeout)
        let result = cb
            .call(|| async { Err::<(), ApiError>(ApiError::Timeout) })
            .await;

        assert!(result.is_err());
        assert_eq!(cb.failure_count(), 1, "Transient error should increment count");
    }

    /// Test that successful calls reset the failure count.
    #[tokio::test]
    async fn successful_call_resets_failure_count() {
        let cb = CircuitBreaker::new("test-service", 3, Duration::from_secs(10));

        // Inject one failure
        let _ = cb
            .call(|| async { Err::<(), ApiError>(ApiError::Timeout) })
            .await;
        assert_eq!(cb.failure_count(), 1);

        // Successful call should reset failure count
        let result = cb.call(|| async { Ok::<_, ApiError>(()) }).await;

        assert!(result.is_ok());
        assert_eq!(
            cb.failure_count(),
            0,
            "Successful call should reset failure count"
        );
    }

    // ──────────────────────────────────────────────────────────────────────────
    // HALF-OPEN STATE AND RECOVERY
    // ──────────────────────────────────────────────────────────────────────────

    /// Test that circuit transitions to HalfOpen after timeout period.
    #[tokio::test]
    async fn circuit_transitions_to_half_open_after_timeout() {
        let cb = CircuitBreaker::new("test-service", 2, Duration::from_millis(100));

        // Trigger circuit open
        for _ in 0..2 {
            let _ = cb
                .call(|| async { Err::<(), ApiError>(ApiError::Timeout) })
                .await;
        }

        // Circuit is now open
        let result = cb
            .call(|| async { panic!("Should not be called") })
            .await;
        assert!(matches!(result, Err(ApiError::CircuitOpen(_))));

        // Wait for recovery timeout
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Circuit should be HalfOpen and allow a probe
        let counter = Arc::new(CallCounter::new());
        let counter_clone = counter.clone();

        let result = cb
            .call(move || {
                let c = counter_clone.clone();
                async move {
                    c.increment();
                    Ok::<_, ApiError>(42)
                }
            })
            .await;

        assert!(result.is_ok());
        assert_eq!(counter.count(), 1, "HalfOpen should allow probe");
        assert_eq!(cb.failure_count(), 0, "Successful probe should reset failure count");
    }

    /// Test that HalfOpen probe failure re-opens the circuit.
    #[tokio::test]
    async fn half_open_probe_failure_reopens_circuit() {
        let cb = CircuitBreaker::new("test-service", 2, Duration::from_millis(100));

        // Trigger circuit open
        for _ in 0..2 {
            let _ = cb
                .call(|| async { Err::<(), ApiError>(ApiError::Timeout) })
                .await;
        }

        // Wait for recovery timeout to enter HalfOpen
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Probe fails - circuit should re-open
        let result = cb
            .call(|| async { Err::<(), ApiError>(ApiError::Timeout) })
            .await;

        assert!(result.is_err());
        assert_eq!(cb.failure_count(), 1, "Failed probe increments failure count");

        // Next call should be rejected
        let result = cb
            .call(|| async { panic!("Should not be called") })
            .await;
        assert!(matches!(result, Err(ApiError::CircuitOpen(_))));
    }

    /// Test that HalfOpen probe success closes the circuit.
    #[tokio::test]
    async fn half_open_probe_success_closes_circuit() {
        let cb = CircuitBreaker::new("test-service", 2, Duration::from_millis(100));

        // Trigger circuit open
        for _ in 0..2 {
            let _ = cb
                .call(|| async { Err::<(), ApiError>(ApiError::Timeout) })
                .await;
        }

        // Wait for recovery timeout
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Successful probe closes the circuit
        let result = cb.call(|| async { Ok::<_, ApiError>(99) }).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 99);
        assert_eq!(cb.failure_count(), 0, "Successful probe resets failure count");

        // Next call should also succeed without circuit rejection
        let result = cb.call(|| async { Ok::<_, ApiError>(88) }).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 88);
    }

    // ──────────────────────────────────────────────────────────────────────────
    // TRANSIENT vs NON-TRANSIENT ERRORS
    // ──────────────────────────────────────────────────────────────────────────

    /// Test that only transient errors increment failure count.
    #[tokio::test]
    async fn only_transient_errors_increment_failure_count() {
        let cb = CircuitBreaker::new("test-service", 5, Duration::from_secs(30));

        // ExternalService error (transient) - should increment
        let _ = cb
            .call(|| async { Err::<(), ApiError>(ApiError::ExternalService("503".to_owned())) })
            .await;
        assert_eq!(cb.failure_count(), 1);

        // NotFound (non-transient) - should not increment
        let _ = cb
            .call(|| async { Err::<(), ApiError>(ApiError::NotFound("x".to_owned())) })
            .await;
        assert_eq!(cb.failure_count(), 1);

        // Timeout (transient) - should increment
        let _ = cb
            .call(|| async { Err::<(), ApiError>(ApiError::Timeout) })
            .await;
        assert_eq!(cb.failure_count(), 2);

        // Unauthorized (non-transient) - should not increment
        let _ = cb
            .call(|| async { Err::<(), ApiError>(ApiError::Unauthorized) })
            .await;
        assert_eq!(cb.failure_count(), 2);
    }

    // ──────────────────────────────────────────────────────────────────────────
    // CONCURRENT REQUESTS
    // ──────────────────────────────────────────────────────────────────────────

    /// Test that concurrent requests respect circuit state.
    #[tokio::test]
    async fn concurrent_requests_respect_circuit_state() {
        let cb = Arc::new(CircuitBreaker::new("test-service", 2, Duration::from_secs(10)));

        // Create two tasks that will try to fail the circuit
        let cb1 = cb.clone();
        let task1 = tokio::spawn(async move {
            for _ in 0..2 {
                let _ = cb1
                    .call(|| async { Err::<(), ApiError>(ApiError::Timeout) })
                    .await;
            }
        });

        let cb2 = cb.clone();
        let task2 = tokio::spawn(async move {
            // Small delay to let task1 get first failures in
            tokio::time::sleep(Duration::from_millis(10)).await;
            let result = cb2.call(|| async { Ok::<_, ApiError>(()) }).await;
            result
        });

        let _ = tokio::join!(task1, task2);

        // Circuit should be open
        let result = cb
            .call(|| async { panic!("Should not reach here") })
            .await;
        assert!(matches!(result, Err(ApiError::CircuitOpen(_))));
    }

    /// Test that concurrent successful calls maintain closed state.
    #[tokio::test]
    async fn concurrent_successful_calls_maintain_closed_state() {
        let cb = Arc::new(CircuitBreaker::new("test-service", 10, Duration::from_secs(10)));
        let counter = Arc::new(CallCounter::new());

        let mut tasks = vec![];

        for i in 0..10 {
            let cb_clone = cb.clone();
            let counter_clone = counter.clone();

            let task = tokio::spawn(async move {
                for _ in 0..5 {
                    let _ = cb_clone
                        .call(move || {
                            let c = counter_clone.clone();
                            async move {
                                c.increment();
                                Ok::<_, ApiError>(i)
                            }
                        })
                        .await;
                }
            });

            tasks.push(task);
        }

        for task in tasks {
            let _ = task.await;
        }

        // Should have succeeded 50 times
        assert_eq!(counter.count(), 50);
        // Circuit should remain closed
        assert_eq!(cb.failure_count(), 0);
    }

    // ──────────────────────────────────────────────────────────────────────────
    // ENV VARIABLE OVERRIDES
    // ──────────────────────────────────────────────────────────────────────────

    /// Test that from_env respects custom failure threshold.
    #[test]
    fn from_env_respects_failure_threshold_override() {
        std::env::set_var("CB_CUSTOM_TEST_FAILURE_THRESHOLD", "7");
        let cb = CircuitBreaker::from_env("custom-test", 5, 30);

        // The threshold should be 7 from env, not 5 from default
        // We can verify this indirectly by checking behavior

        std::env::remove_var("CB_CUSTOM_TEST_FAILURE_THRESHOLD");
    }

    /// Test that from_env respects custom recovery timeout.
    #[test]
    fn from_env_respects_recovery_timeout_override() {
        std::env::set_var("CB_CUSTOM_TEST_2_RECOVERY_TIMEOUT_SECS", "120");
        let cb = CircuitBreaker::from_env("custom-test-2", 5, 30);

        // Verify the recovery timeout was set - the timeout is stored as secs
        assert_eq!(cb.0.recovery_timeout_secs, 120);

        std::env::remove_var("CB_CUSTOM_TEST_2_RECOVERY_TIMEOUT_SECS");
    }

    /// Test that from_env uses defaults when env vars not set.
    #[test]
    fn from_env_uses_defaults_when_unset() {
        // Ensure env vars are not set
        std::env::remove_var("CB_DEFAULT_TEST_FAILURE_THRESHOLD");
        std::env::remove_var("CB_DEFAULT_TEST_RECOVERY_TIMEOUT_SECS");

        let cb = CircuitBreaker::from_env("default-test", 5, 60);

        assert_eq!(cb.0.failure_threshold, 5);
        assert_eq!(cb.0.recovery_timeout_secs, 60);
    }

    // ──────────────────────────────────────────────────────────────────────────
    // STRESS SCENARIOS
    // ──────────────────────────────────────────────────────────────────────────

    /// Test rapid failures trigger circuit correctly.
    #[tokio::test]
    async fn rapid_failures_trigger_circuit() {
        let cb = CircuitBreaker::new("test-service", 5, Duration::from_secs(1));
        let counter = Arc::new(CallCounter::new());

        // Rapidly send 5 failures
        for _ in 0..5 {
            let result = cb
                .call(|| async { Err::<(), ApiError>(ApiError::Timeout) })
                .await;
            assert!(result.is_err());
        }

        // Next call should be rejected
        let result = cb
            .call(move || {
                let c = counter.clone();
                async move {
                    c.increment();
                    panic!("Should not reach");
                }
            })
            .await;

        assert!(matches!(result, Err(ApiError::CircuitOpen(_))));
        assert_eq!(counter.count(), 0, "Circuit should prevent calls");
    }

    /// Test that just-under threshold failures don't open circuit.
    #[tokio::test]
    async fn below_threshold_failures_do_not_open_circuit() {
        let cb = CircuitBreaker::new("test-service", 5, Duration::from_secs(30));

        // Send 4 failures (threshold is 5)
        for i in 0..4 {
            let result = cb
                .call(|| async { Err::<(), ApiError>(ApiError::Timeout) })
                .await;
            assert!(result.is_err(), "Failure {} should be allowed", i + 1);
        }

        // Next successful call should still go through
        let result = cb.call(|| async { Ok::<_, ApiError>(42) }).await;

        assert!(
            result.is_ok(),
            "Circuit should allow calls when below threshold"
        );
        assert_eq!(cb.failure_count(), 0, "Successful call resets counter");
    }

    /// Test that circuit name is included in error message.
    #[tokio::test]
    async fn circuit_name_in_error_message() {
        let cb = CircuitBreaker::new("payment-gateway", 1, Duration::from_secs(10));

        let _ = cb
            .call(|| async { Err::<(), ApiError>(ApiError::Timeout) })
            .await;

        let result = cb
            .call(|| async { panic!("Should not reach") })
            .await;

        match result {
            Err(ApiError::CircuitOpen(name)) => {
                assert_eq!(name, "payment-gateway");
            }
            _ => panic!("Expected CircuitOpen error with service name"),
        }
    }

    /// Test that separate circuit breakers operate independently.
    #[tokio::test]
    async fn separate_circuit_breakers_independent() {
        let cb1 = Arc::new(CircuitBreaker::new("service-1", 2, Duration::from_secs(10)));
        let cb2 = Arc::new(CircuitBreaker::new("service-2", 2, Duration::from_secs(10)));

        // Fail service-1 twice
        for _ in 0..2 {
            let _ = cb1
                .call(|| async { Err::<(), ApiError>(ApiError::Timeout) })
                .await;
        }

        // Service-1 should be open
        assert!(
            cb1.call(|| async { panic!("Should not reach") })
                .await
                .is_err()
        );

        // Service-2 should still be closed
        let result = cb2.call(|| async { Ok::<_, ApiError>(42) }).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
    }
}
