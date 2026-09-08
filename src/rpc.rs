//! What to do when the endpoint says "not now".
//!
//! A refusal for capacity is not an answer. This chain's endpoint hands them
//! out in bursts - `fullnode request limit exceeded` on one request and then
//! nothing for a minute - and each one that reaches a caller costs more than
//! the request would have: a tick window goes stale and a signal is skipped, a
//! sale is priced off the router instead of the model, a pool fails to resolve
//! at startup. They also clear fast. So they are asked again, twice, quickly,
//! and everything else is passed straight back.

use anyhow::Result;
use std::future::Future;
use std::time::Duration;

/// Attempts in total, and the waits between them.
///
/// Short on purpose. The refusals this covers clear in well under a second,
/// and a wait long enough to matter is a wait a trading loop should spend
/// somewhere else. Two extra asks is also the most that is polite to an
/// endpoint that just said it is over its limit.
const TRIES: u32 = 3;
const WAITS: [Duration; 2] = [Duration::from_millis(120), Duration::from_millis(400)];

/// Is this the endpoint declining to serve right now, rather than answering?
///
/// A list of what these endpoints actually say, not a catch-all. Retrying a
/// revert, a bad parameter or a wrong address is a wasted request every time,
/// and asking three times for a call that cannot work is how a request limit
/// gets worse instead of better.
pub fn transient(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    [
        "request limit",
        "rate limit",
        "too many requests",
        "-32005",
        "fullnode unavailable",
        "temporarily unavailable",
        "service unavailable",
        "bad gateway",
        "gateway timeout",
        "timed out",
        "timeout",
        "connection reset",
        "connection closed",
        "error sending request",
        "channel closed",
    ]
    .iter()
    .any(|m| e.contains(m))
}

/// Ask, and ask again if the refusal was only about capacity.
///
/// `what` names the request in the log line, so a burst of refusals can be read
/// back to whichever caller was making them.
pub async fn retrying<T, F, Fut>(what: &str, mut ask: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 1u32;
    loop {
        match ask().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                // The whole chain: the transport's words are usually under a
                // context line, and it is the transport's words that say why.
                if attempt >= TRIES || !transient(&format!("{e:#}")) {
                    return Err(e);
                }
                let wait = WAITS[(attempt - 1) as usize];
                tracing::debug!(
                    what, attempt, of = TRIES, wait_ms = wait.as_millis() as u64,
                    err = %format!("{e:#}"), "the endpoint declined; asking again"
                );
                tokio::time::sleep(wait).await;
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The strings below are verbatim from the endpoints this runs against.
    /// Reading a revert as a capacity refusal would send the same doomed call
    /// three times, on every signal, into a limit that is already biting.
    #[test]
    fn only_capacity_refusals_are_worth_asking_again() {
        assert!(transient(
            "(code: -32000, message: fullnode request limit exceeded)"
        ));
        assert!(transient("(code: -32005, message: rate limit exceeded)"));
        assert!(transient(
            "(code: -32000, message: fullnode unavailable, data: None)"
        ));
        assert!(transient("error sending request for url (https://...)"));
        assert!(transient("operation timed out"));

        assert!(!transient("execution reverted: V4TooLittleReceived"));
        assert!(!transient(
            "(code: 3, message: execution reverted, data: Some(\"0xbe8b8507\"))"
        ));
        assert!(!transient("Block range is too large"));
        assert!(!transient("invalid api key"));
        assert!(!transient(""));
    }

    #[tokio::test]
    async fn a_capacity_refusal_is_asked_again_and_a_revert_is_not() {
        let asks = AtomicU32::new(0);
        let out: Result<u8> = retrying("test", || async {
            match asks.fetch_add(1, Ordering::SeqCst) {
                0 => Err(anyhow::anyhow!("fullnode request limit exceeded")),
                _ => Ok(7u8),
            }
        })
        .await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(asks.load(Ordering::SeqCst), 2);

        let asks = AtomicU32::new(0);
        let out: Result<u8> = retrying("test", || async {
            asks.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("execution reverted"))
        })
        .await;
        assert!(out.is_err());
        assert_eq!(asks.load(Ordering::SeqCst), 1);
    }

    /// A limit that does not clear must not turn one request into an unbounded
    /// stream of them.
    #[tokio::test]
    async fn asking_again_is_bounded() {
        let asks = AtomicU32::new(0);
        let out: Result<u8> = retrying("test", || async {
            asks.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("fullnode request limit exceeded"))
        })
        .await;
        assert!(out.is_err());
        assert_eq!(asks.load(Ordering::SeqCst), TRIES);
    }

    /// The reason lives under a context line at every caller, so a wrapper that
    /// only reads the top of the chain would retry nothing at all.
    #[tokio::test]
    async fn the_reason_is_found_under_a_context_line() {
        use anyhow::Context;
        let asks = AtomicU32::new(0);
        let out: Result<u8> = retrying("test", || async {
            match asks.fetch_add(1, Ordering::SeqCst) {
                0 => Err(anyhow::anyhow!("fullnode request limit exceeded")).context("eth_call"),
                _ => Ok(1u8),
            }
        })
        .await;
        assert_eq!(out.unwrap(), 1);
        assert_eq!(asks.load(Ordering::SeqCst), 2);
    }
}
