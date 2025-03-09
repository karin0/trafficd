use anyhow::Result;
use serde::Deserialize;
use xray_core::Client;
use xray_core::app::stats::command::{QueryStatsRequest, Stat as RawStat};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: String,
}

#[derive(Debug)]
pub struct Stat<'a> {
    pub email: &'a str,
    pub is_tx: bool,
    pub value: u64,
}

#[derive(Debug)]
pub struct Stats(Vec<RawStat>);

pub async fn query(server: &str, reset: bool) -> Result<Stats> {
    let req = QueryStatsRequest {
        pattern: String::new(),
        reset,
    };

    let r = Client::from_url(server)
        .await?
        .stats()
        .query_stats(req)
        .await?;

    Ok(Stats(r.into_inner().stat))
}

impl Stats {
    pub fn iter(&self) -> impl Iterator<Item = Stat> {
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
