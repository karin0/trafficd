use anyhow::Result;
use xray_core::app::stats::command::stats_service_client::StatsServiceClient;
use xray_core::app::stats::command::{QueryStatsRequest, Stat as RawStat};
use xray_core::tonic::transport::{Channel, Endpoint};

pub struct Source(StatsServiceClient<Channel>);

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
        let req = QueryStatsRequest {
            pattern: String::new(),
            reset,
        };
        let r = self.0.clone().query_stats(req).await?;
        Ok(Stats(r.into_inner().stat))
    }
}

impl Stats {
    pub fn iter(&self) -> impl Iterator<Item = Stat<'_>> {
        self.0.iter().filter(|s| s.value != 0).map(|s| {
            /*
            {
                "name": "user>>>root@a.cc>>>traffic>>>downlink",
                "value": 4489573739
            },
            */
            let mut parts = s.name.split(">>>");
            assert_eq!(parts.next(), Some("user"));
            let email = parts.next().unwrap();
            assert_eq!(parts.next(), Some("traffic"));
            let link: &str = parts.next().unwrap();
            let is_tx = link == "uplink";
            assert!(is_tx || link == "downlink");
            assert!(parts.next().is_none());
            assert!(s.value > 0);

            Stat {
                email,
                is_tx,
                value: s.value.try_into().unwrap(),
            }
        })
    }
}
