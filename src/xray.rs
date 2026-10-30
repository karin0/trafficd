use anyhow::Result;
use xray_core::app::stats::command::stats_service_client::StatsServiceClient;
use xray_core::app::stats::command::{QueryStatsRequest, Stat as RawStat};
use xray_core::tonic::transport::{Channel, Endpoint};

pub struct Source(StatsServiceClient<Channel>);

const USER_PREFIX: &str = "user>>>";

#[derive(Debug)]
pub struct Stat<'a> {
    pub email: &'a str,
    pub is_tx: bool,
    pub value: u64,
}

#[derive(Debug)]
pub struct Stats(Vec<RawStat>);

impl Source {
    // The channel connects on the first query and reconnects after a failure.
    // tonic is built without TLS, so the https scheme connects in plaintext.
    pub fn new(server: &str) -> Result<Self> {
        let channel = Endpoint::try_from(format!("https://{server}"))?.connect_lazy();
        Ok(Self(StatsServiceClient::new(channel)))
    }

    pub async fn query(&self, reset: bool) -> Result<Stats> {
        // Xray matches the pattern as a substring, so the counters of an inbound or
        // outbound whose tag ends with "user" also match, and are reset with the rest.
        let req = QueryStatsRequest {
            pattern: USER_PREFIX.to_owned(),
            reset,
        };
        let r = self.0.clone().query_stats(req).await?;
        Ok(Stats(r.into_inner().stat))
    }
}

// Xray names the traffic counters of a user "user>>>{email}>>>traffic>>>uplink" and
// "user>>>{email}>>>traffic>>>downlink", and the email may contain ">>>" itself.
// Returns the email and whether the counter is uplink, or None for a counter
// that is not a user's.
fn parse_user_stat(name: &str) -> Option<(&str, bool)> {
    let rest = name.strip_prefix(USER_PREFIX)?;
    if let Some(email) = rest.strip_suffix(">>>traffic>>>uplink") {
        Some((email, true))
    } else if let Some(email) = rest.strip_suffix(">>>traffic>>>downlink") {
        Some((email, false))
    } else {
        panic!("unknown user stat {name:?}");
    }
}

impl Stats {
    pub fn iter(&self) -> impl Iterator<Item = Stat<'_>> {
        self.0.iter().filter(|s| s.value != 0).filter_map(|s| {
            let (email, is_tx) = parse_user_stat(&s.name)?;
            Some(Stat {
                email,
                is_tx,
                value: s.value.try_into().unwrap(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_user_stat_takes_the_email_between_prefix_and_suffix() {
        assert_eq!(
            parse_user_stat("user>>>root@a.cc>>>traffic>>>uplink"),
            Some(("root@a.cc", true))
        );
        assert_eq!(
            parse_user_stat("user>>>a>>>b@c>>>traffic>>>uplink"),
            Some(("a>>>b@c", true))
        );
        assert_eq!(
            parse_user_stat("user>>>a>>>b@c>>>traffic>>>downlink"),
            Some(("a>>>b@c", false))
        );
    }

    #[test]
    fn parse_user_stat_skips_other_counters() {
        assert_eq!(parse_user_stat("inbound>>>xuser>>>traffic>>>uplink"), None);
        assert_eq!(
            parse_user_stat("outbound>>>user>>>traffic>>>downlink"),
            None
        );
    }

    #[test]
    #[should_panic(expected = "unknown user stat")]
    fn parse_user_stat_fails_on_an_unknown_user_counter() {
        parse_user_stat("user>>>root@a.cc>>>traffic>>>sideways");
    }

    #[test]
    fn iter_yields_the_nonzero_user_counters() {
        let raw = |name: &str, value| RawStat {
            name: name.to_owned(),
            value,
        };
        let stats = Stats(vec![
            raw("user>>>a@x>>>traffic>>>uplink", 3),
            raw("user>>>a@x>>>traffic>>>downlink", 0),
            raw("inbound>>>xuser>>>traffic>>>uplink", 5),
        ]);
        let got: Vec<_> = stats.iter().map(|s| (s.email, s.is_tx, s.value)).collect();
        assert_eq!(got, [("a@x", true, 3)]);
    }
}
