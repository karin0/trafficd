use std::collections::BTreeMap;

use anyhow::Result;
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    secret: String,
    url: String,
}

pub struct Source {
    client: Client,
    url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Traffic {
    pub tx: u64,
    pub rx: u64,
}

// https://v2.hysteria.network/zh/docs/advanced/Traffic-Stats-API/
impl Source {
    pub fn new(config: Config) -> Result<Self> {
        let mut secret = HeaderValue::from_str(&config.secret)?;
        secret.set_sensitive(true);
        Ok(Self {
            client: Client::builder()
                .default_headers(HeaderMap::from_iter([(AUTHORIZATION, secret)]))
                .build()?,
            url: config.url,
        })
    }

    pub async fn traffic(&self, reset: bool) -> Result<BTreeMap<String, Traffic>> {
        let url = if reset {
            format!("{}?clear=1", self.url)
        } else {
            self.url.clone()
        };
        Ok(self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
}
