use log::trace;

use sqlx::{
    ConnectOptions,
    sqlite::{SqliteConnectOptions, SqliteConnection},
};
use std::str::FromStr;

pub async fn connect(url: &str, ro: bool) -> sqlx::Result<SqliteConnection> {
    trace!("connecting to {}, ro = {}", url, ro);
    SqliteConnectOptions::from_str(url)?
        .read_only(ro)
        .connect()
        .await
}
