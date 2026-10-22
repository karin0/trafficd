use std::str::FromStr;

use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;

pub async fn connect(url: &str) -> sqlx::Result<SqlitePool> {
    SqlitePool::connect_with(SqliteConnectOptions::from_str(url)?).await
}
