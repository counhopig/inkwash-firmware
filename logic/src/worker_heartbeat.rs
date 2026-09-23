pub fn should_feed_after_command(watchdog_subscribed: bool) -> bool {
    watchdog_subscribed
}

#[cfg(test)]
mod tests {
    use super::should_feed_after_command;

    #[test]
    fn subscribed_worker_feeds_after_every_command() {
        let command_stream = ["AlarmStatus", "Snapshot", "ReadTime", "AlarmStatus"];
        let feed_points = command_stream
            .iter()
            .map(|_| should_feed_after_command(true))
            .collect::<Vec<_>>();

        assert_eq!(feed_points, vec![true, true, true, true]);
    }

    #[test]
    fn unsubscribed_worker_does_not_request_feed() {
        assert!(!should_feed_after_command(false));
    }
}
