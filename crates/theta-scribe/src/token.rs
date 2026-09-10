//! Token supply and refresh.
//!
//! Project session tokens are short-lived by design — minutes to hours
//! (`04-threat-model-security.md` §2) — so a long-running edge process will
//! outlive its token and must be able to get another without the caller
//! restarting anything.

use std::sync::Arc;

use tokio::sync::RwLock;

/// Supplies scoped session tokens.
///
/// The Control Plane mints these; anything implementing this trait works in its
/// place, which is what lets Scribe be tested end to end without one.
pub trait TokenSource: Send + Sync + std::fmt::Debug {
    /// The current token. Called on every new connection.
    fn token(&self) -> String;

    /// Called after the server rejects a token as unauthorized.
    ///
    /// Returns `true` if a *different* token is now available. Returning `false`
    /// stops the retry, because retrying with the same rejected token is a loop,
    /// not a recovery.
    fn refresh(&self) -> bool {
        false
    }
}

/// A fixed token. Fine for tests and for short-lived processes; a long-running
/// one wants a source that can actually refresh.
#[derive(Debug, Clone)]
pub struct StaticToken(pub String);

impl TokenSource for StaticToken {
    fn token(&self) -> String {
        self.0.clone()
    }
}

/// A token that can be replaced from outside, e.g. by the CLI re-resolving a
/// context, without tearing down the Scribe.
#[derive(Debug, Clone, Default)]
pub struct SwappableToken {
    inner: Arc<RwLock<String>>,
    generation: Arc<std::sync::atomic::AtomicU64>,
}

impl SwappableToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(token.into())),
            generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Install a new token. Connections pick it up on their next handshake.
    pub async fn set(&self, token: impl Into<String>) {
        *self.inner.write().await = token.into();
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl TokenSource for SwappableToken {
    fn token(&self) -> String {
        // Blocking read on a lock held only for the duration of a clone.
        self.inner.blocking_read().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_static_token_never_claims_it_can_refresh() {
        let source = StaticToken("tok".into());
        assert_eq!(source.token(), "tok");
        // Retrying with the same rejected token would loop forever.
        assert!(!source.refresh());
    }
}
