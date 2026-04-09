use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BASE_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const STABLE_CONNECTION_WINDOW: Duration = Duration::from_secs(120);
const MAX_RECONNECT_JITTER: Duration = Duration::from_millis(750);

#[derive(Debug)]
pub struct ReconnectPolicy {
    current_delay: Duration,
    base_delay: Duration,
    max_delay: Duration,
    stable_connection_window: Duration,
    max_jitter: Duration,
    connected_at: Option<Instant>,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            current_delay: Duration::ZERO,
            base_delay: BASE_RECONNECT_DELAY,
            max_delay: MAX_RECONNECT_DELAY,
            stable_connection_window: STABLE_CONNECTION_WINDOW,
            max_jitter: MAX_RECONNECT_JITTER,
            connected_at: None,
        }
    }
}

impl ReconnectPolicy {
    pub fn reconnect_wait(&self) -> Option<Duration> {
        if self.current_delay.is_zero() {
            None
        } else {
            Some(self.current_delay + self.jitter())
        }
    }

    pub fn note_connect_error(&mut self) -> Duration {
        self.connected_at = None;
        self.increase_delay();
        self.current_delay
    }

    pub fn note_connected(&mut self) {
        self.connected_at = Some(Instant::now());
    }

    pub fn note_spirc_disconnect(&mut self) -> Duration {
        let uptime = self
            .connected_at
            .take()
            .map(|started_at| started_at.elapsed())
            .unwrap_or(Duration::ZERO);
        self.apply_disconnect(uptime);
        uptime
    }

    pub fn reset(&mut self) {
        self.current_delay = Duration::ZERO;
        self.connected_at = None;
    }

    fn apply_disconnect(&mut self, uptime: Duration) {
        if uptime >= self.stable_connection_window {
            self.current_delay = self.base_delay;
        } else {
            self.increase_delay();
        }
    }

    fn increase_delay(&mut self) {
        self.current_delay = if self.current_delay.is_zero() {
            self.base_delay
        } else {
            std::cmp::min(self.current_delay * 2, self.max_delay)
        };
    }

    fn jitter(&self) -> Duration {
        if self.max_jitter.is_zero() {
            return Duration::ZERO;
        }

        let max_nanos = std::cmp::min(self.max_jitter.as_nanos(), u128::from(u64::MAX)) as u64;
        if max_nanos == 0 {
            return Duration::ZERO;
        }

        let now_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64;
        Duration::from_nanos(now_nanos % max_nanos.saturating_add(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl ReconnectPolicy {
        fn for_test(
            base_delay: Duration,
            max_delay: Duration,
            stable_connection_window: Duration,
            max_jitter: Duration,
        ) -> Self {
            Self {
                current_delay: Duration::ZERO,
                base_delay,
                max_delay,
                stable_connection_window,
                max_jitter,
                connected_at: None,
            }
        }

        fn note_spirc_disconnect_with_uptime(&mut self, uptime: Duration) {
            self.apply_disconnect(uptime);
        }
    }

    #[test]
    fn doubles_reconnect_delay_until_maximum() {
        let mut policy = ReconnectPolicy::for_test(
            Duration::from_secs(1),
            Duration::from_secs(8),
            Duration::from_secs(30),
            Duration::ZERO,
        );

        assert_eq!(policy.note_connect_error(), Duration::from_secs(1));
        assert_eq!(policy.note_connect_error(), Duration::from_secs(2));
        assert_eq!(policy.note_connect_error(), Duration::from_secs(4));
        assert_eq!(policy.note_connect_error(), Duration::from_secs(8));
        assert_eq!(policy.note_connect_error(), Duration::from_secs(8));
    }

    #[test]
    fn stable_connection_resets_backoff_to_base_delay() {
        let mut policy = ReconnectPolicy::for_test(
            Duration::from_secs(1),
            Duration::from_secs(32),
            Duration::from_secs(30),
            Duration::ZERO,
        );

        assert_eq!(policy.note_connect_error(), Duration::from_secs(1));
        assert_eq!(policy.note_connect_error(), Duration::from_secs(2));
        assert_eq!(policy.note_connect_error(), Duration::from_secs(4));

        policy.note_spirc_disconnect_with_uptime(Duration::from_secs(31));
        assert_eq!(policy.reconnect_wait(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn short_uptime_keeps_increasing_backoff() {
        let mut policy = ReconnectPolicy::for_test(
            Duration::from_secs(1),
            Duration::from_secs(16),
            Duration::from_secs(30),
            Duration::ZERO,
        );

        assert_eq!(policy.note_connect_error(), Duration::from_secs(1));
        assert_eq!(policy.note_connect_error(), Duration::from_secs(2));

        policy.note_spirc_disconnect_with_uptime(Duration::from_secs(5));
        assert_eq!(policy.reconnect_wait(), Some(Duration::from_secs(4)));
    }

    #[test]
    fn reset_clears_delay() {
        let mut policy = ReconnectPolicy::for_test(
            Duration::from_secs(1),
            Duration::from_secs(16),
            Duration::from_secs(30),
            Duration::ZERO,
        );

        policy.note_connect_error();
        policy.reset();

        assert_eq!(policy.reconnect_wait(), None);
    }
}
