//! Circuit breaker and backoff (`Phase 4`) — recovery policy from **`15`** schema.

use std::io;
use std::os::unix::io::FromRawFd;
use std::time::Instant;

use sysx_schema::{RecoveryMode, RecoveryPolicy};

/// Drop restart events older than **`window_sec`** (sliding window).
pub fn prune_restart_window(events: &mut Vec<Instant>, window_sec: u64, now: Instant) {
    events.retain(|t| now.saturating_duration_since(*t).as_secs() < window_sec);
}

/// True when **`events.len() >= max_restarts`** (failure budget exhausted).
pub fn circuit_should_open(events: &[Instant], max_restarts: u32) -> bool {
    if max_restarts == 0 {
        return true;
    }
    events.len() as u32 >= max_restarts
}

/// `backoff_ms * 2^attempt` (cap shift to avoid overflow).
pub fn backoff_exponential_ms(base: u64, attempt: u32) -> u64 {
    base.saturating_mul(1u64 << (attempt.min(31) as u32))
}

/// One-shot relative timerfd (**`TFD_NONBLOCK`**) — drain with **`u64`** read on fire.
pub fn recovery_timerfd_arm_ms(ms: u64) -> io::Result<std::fs::File> {
    unsafe {
        let fd = libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut itv: libc::itimerspec = std::mem::zeroed();
        let secs = (ms / 1000) as libc::time_t;
        let nsec = ((ms % 1000) * 1_000_000) as libc::c_long;
        itv.it_value.tv_sec = secs.into();
        itv.it_value.tv_nsec = nsec.into();
        if libc::timerfd_settime(fd, 0, &itv, std::ptr::null_mut()) != 0 {
            let e = io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        Ok(std::fs::File::from_raw_fd(fd))
    }
}

/// Whether EOF / readiness failure should use recovery (vs immediate cascade).
pub fn recovery_applies(policy: Option<&RecoveryPolicy>) -> bool {
    let Some(p) = policy else {
        return false;
    };
    matches!(
        p.policy,
        RecoveryMode::OnFailure | RecoveryMode::Always
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_exponential() {
        assert_eq!(backoff_exponential_ms(100, 0), 100);
        assert_eq!(backoff_exponential_ms(100, 2), 400);
    }

    #[test]
    fn circuit_open_count() {
        let now = Instant::now();
        let v = vec![now, now];
        assert!(circuit_should_open(&v, 2));
        assert!(!circuit_should_open(&v, 3));
    }
}
