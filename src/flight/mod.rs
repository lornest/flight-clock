pub mod adsb;
pub mod hexdb;
pub mod poller;
pub mod tracker;
pub mod types;

use std::sync::OnceLock;
use std::time::Duration;

/// Shared HTTP client. Timeouts stop a stalled request from hanging the
/// poller (and leaving the radar stuck on "Updating...") forever.
pub fn http() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()
    })
}
