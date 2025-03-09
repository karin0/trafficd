use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use awc::{Client, ClientBuilder, http::header};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    secret: String,
    url: String,
}

#[derive(Default)]
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
    pub fn new(config: Config) -> Self {
        Self {
            client: ClientBuilder::new()
                .add_default_header((header::AUTHORIZATION, config.secret))
                .finish(),
            url: config.url,
        }
    }

    async fn _traffic<U: AsRef<str>>(&self, url: U) -> Result<BTreeMap<String, Traffic>> {
        match self.client.get(url.as_ref()).send().await {
            Ok(mut r) => Ok(r.json().await?),
            Err(e) => Err(anyhow!("SendRequestError: {}", e)),
        }
    }

    pub async fn traffic(&self, reset: bool) -> Result<BTreeMap<String, Traffic>> {
        if reset {
            self._traffic(format!("{}?clear=1", self.url)).await
        } else {
            self._traffic(&*self.url).await
        }
    }
}
