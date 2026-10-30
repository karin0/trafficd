mod hysteria;
mod lark;
mod users;
mod xray;

use actix_web::web::{self, Data, Json};
use actix_web::{App, HttpResponse, HttpServer, Responder, error, guard, middleware};
use anyhow::{Context, Result};
use clap::Parser;
use log::{debug, error, info};
use prost::Message;
use serde::{Deserialize, Serialize};
use sqlx::{SqlitePool, query_as, query_scalar};
use time::macros::format_description;
use time::{Date, Month, OffsetDateTime, UtcOffset};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{LocalSet, spawn_local};

use std::env::{self, VarError};
use std::ffi::{OsStr, OsString};
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::lark::{Sources, worker};
use crate::users::{User, load_users};

struct Record {
    tid: i64,
    time: i64,
    tx: i64,
    rx: i64,
}

mod proto {
    include!(concat!(env!("OUT_DIR"), "/traffic.proto.rs"));
}

struct AppProps {
    pool: SqlitePool,
    users: &'static [User],
    tz: UtcOffset,
    chan: mpsc::Sender<oneshot::Sender<bool>>,
}

// The current month without a year, the whole year without a month
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    year: Option<i32>,
    month: Option<u8>,
}

fn resolve_period(query: &Query, tz: UtcOffset) -> error::Result<proto::Period> {
    let (year, month) = match (query.year, query.month) {
        (None, None) => {
            let today = OffsetDateTime::now_utc().to_offset(tz).date();
            (today.year(), Some(today.month()))
        }
        (Some(year), month) => (
            year,
            month
                .map(Month::try_from)
                .transpose()
                .map_err(error::ErrorBadRequest)?,
        ),
        (None, Some(_)) => return Err(error::ErrorBadRequest("month without year")),
    };
    let first = |year, month| {
        Date::from_calendar_date(year, month, 1)
            .map(|d| d.midnight().assume_offset(tz).unix_timestamp())
            .map_err(error::ErrorBadRequest)
    };
    let (since, until) = match month {
        None => (
            first(year, Month::January)?,
            first(year + 1, Month::January)?,
        ),
        Some(Month::December) => (
            first(year, Month::December)?,
            first(year + 1, Month::January)?,
        ),
        Some(month) => (first(year, month)?, first(year, month.next())?),
    };
    Ok(proto::Period {
        year,
        month: month.map_or(0, u8::from).into(),
        since,
        until,
    })
}

fn mapper(e: sqlx::Error) -> error::Error {
    error!("sql error: {e:?}");
    error::ErrorInternalServerError(e)
}

async fn kick(chan: &mpsc::Sender<oneshot::Sender<bool>>) -> error::Result<()> {
    let (tx, rx) = oneshot::channel();
    chan.send(tx)
        .await
        .map_err(error::ErrorInternalServerError)?;
    let lark_ok = rx.await.map_err(error::ErrorInternalServerError)?;
    if !lark_ok {
        error!("lark worker failed on the request");
    }
    Ok(())
}

async fn index(req: Json<Query>, data: Data<AppProps>) -> error::Result<impl Responder> {
    let period = resolve_period(&req, data.tz)?;
    info!("request: {period:?}");
    kick(&data.chan).await?;

    let rows = query_as!(
        Record,
        "SELECT * FROM Record WHERE time >= ? AND time < ? ORDER BY time",
        period.since,
        period.until,
    )
    .fetch_all(&data.pool)
    .await
    .map_err(mapper)?
    .into_iter()
    .map(|row| proto::Row {
        tid: row.tid.cast_unsigned(),
        time: row.time.cast_unsigned(),
        tx: row.tx.cast_unsigned(),
        rx: row.rx.cast_unsigned(),
    })
    .collect::<Vec<_>>();
    info!("collected {} rows", rows.len());

    let stats = proto::TrafficStats {
        rows,
        users: data
            .users
            .iter()
            .map(|u| proto::User {
                uid: u.uid,
                name: u.name.clone(),
            })
            .collect(),
        period: Some(period),
    };
    let resp = stats.encode_to_vec();
    debug!("encoded into {} bytes", resp.len());
    Ok(HttpResponse::Ok().body(resp))
}

// The traffic of a user in the current month, which ends at until
#[derive(Serialize)]
struct MonthSum {
    sum: i64,
    until: i64,
}

#[actix_web::get("/{uid}")]
async fn query_sum(req: web::Path<u32>, data: Data<AppProps>) -> error::Result<impl Responder> {
    let uid = req.into_inner();
    let period = resolve_period(
        &Query {
            year: None,
            month: None,
        },
        data.tz,
    )?;
    kick(&data.chan).await?;

    let sum = query_scalar!(
        r#"SELECT coalesce(sum(tx + rx), 0) AS "sum!: i64" FROM Record
        WHERE time >= ? AND time < ? AND tid >> 8 = ?"#,
        period.since,
        period.until,
        uid,
    )
    .fetch_one(&data.pool)
    .await
    .map_err(mapper)?;
    info!("uid {uid} has sum {sum}");

    Ok(HttpResponse::Ok().json(MonthSum {
        sum,
        until: period.until,
    }))
}

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Cli {
    // sqlx database urls have to be UTF-8 (&str)
    db_path: String,

    json_path: PathBuf,

    #[clap(short = 'A', long)]
    no_auth: bool,

    #[clap(short, long, default_value = "127.0.0.1:10090")]
    bind: OsString,

    #[clap(short = 'R', long)]
    no_reset: bool,

    #[clap(short, long, default_value = "300")]
    interval: u16,
}

fn parse_socket_addr(s: &OsStr) -> Option<SocketAddr> {
    s.to_str()?.parse().ok()
}

fn env_var(key: &str) -> Result<String> {
    env::var(key).with_context(|| key.to_owned())
}

fn parse_tz(tz: &str) -> Result<UtcOffset, time::error::Parse> {
    UtcOffset::parse(
        tz,
        format_description!("[offset_hour sign:mandatory]:[offset_minute]"),
    )
}

fn load_tz() -> Result<UtcOffset> {
    Ok(match env::var("TRAFFICD_TZ") {
        Ok(tz) => parse_tz(&tz)?,
        Err(VarError::NotPresent) => UtcOffset::current_local_offset()?,
        Err(e) => return Err(e.into()),
    })
}

async fn entry() -> Result<()> {
    let args = Cli::parse();
    info!("running with {args:?}");

    let users: &'static [User] = load_users(&args.json_path)?.leak();
    info!("found {} users", users.len());

    let token: Option<&'static str> = if args.no_auth {
        None
    } else {
        Some(env_var("TRAFFICD_TOKEN")?.leak())
    };
    let tz = load_tz().context("TRAFFICD_TZ")?;
    info!("using timezone {tz}");
    let hysteria = hysteria::Source::new(
        env_var("TRAFFICD_HYSTERIA_URL")?,
        &env_var("TRAFFICD_HYSTERIA_SECRET")?,
    )
    .context("TRAFFICD_HYSTERIA_SECRET")?;
    let xray =
        xray::Source::new(&env_var("TRAFFICD_XRAY_SERVER")?).context("TRAFFICD_XRAY_SERVER")?;
    let sources = Sources::new(users, xray, hysteria);

    let pool = SqlitePool::connect(&format!("sqlite://{}", args.db_path)).await?;
    let (tx, rx) = mpsc::channel(3);
    let _task = spawn_local(worker(
        sources,
        pool.clone(),
        rx,
        args.interval,
        !args.no_reset,
    ));

    let data = Data::new(AppProps {
        pool,
        users,
        tz,
        chan: tx,
    });

    let server = HttpServer::new(move || {
        let mut scope = web::scope("/traffic");
        if let Some(token) = token {
            scope = scope.guard(guard::Header("Authorization", token));
        }

        App::new()
            .wrap(middleware::Logger::default())
            .app_data(data.clone())
            .service(scope.route("", web::post().to(index)).service(query_sum))
    })
    .workers(1);

    let server = if let Some(addr) = parse_socket_addr(&args.bind) {
        server.bind(addr)
    } else {
        server.bind_uds(args.bind)
    }?;
    server.run().await?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    if env::var("RUST_LOG").is_err() {
        unsafe {
            env::set_var("RUST_LOG", "info,sqlx=error");
        }
    }

    if env::var("JOURNAL_STREAM").is_ok() {
        pretty_env_logger::init();
    } else {
        pretty_env_logger::init_timed();
    }

    LocalSet::new().run_until(entry()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tz_accepts_signed_hours_and_minutes() {
        assert_eq!(
            parse_tz("+08:00").unwrap(),
            UtcOffset::from_hms(8, 0, 0).unwrap()
        );
        assert_eq!(
            parse_tz("-09:30").unwrap(),
            UtcOffset::from_hms(-9, -30, 0).unwrap()
        );
    }

    #[test]
    fn parse_tz_rejects_other_forms() {
        for tz in ["8", "08:00", "+8:00", "+08", "+08:00:00", ""] {
            assert!(parse_tz(tz).is_err(), "{tz:?}");
        }
    }

    const CST: UtcOffset = time::macros::offset!(+8);

    fn period(year: Option<i32>, month: Option<u8>) -> error::Result<proto::Period> {
        resolve_period(&Query { year, month }, CST)
    }

    #[test]
    fn resolve_period_selects_a_month_from_local_midnight() {
        let p = period(Some(2026), Some(10)).unwrap();
        assert_eq!((p.year, p.month), (2026, 10));
        // 2026-10-01 and 2026-11-01 at 00:00 +08:00
        assert_eq!((p.since, p.until), (1_790_784_000, 1_793_462_400));
    }

    #[test]
    fn resolve_period_ends_december_at_the_next_year() {
        let p = period(Some(2026), Some(12)).unwrap();
        assert_eq!((p.since, p.until), (1_796_054_400, 1_798_732_800));
    }

    #[test]
    fn resolve_period_selects_the_whole_year_without_a_month() {
        let p = period(Some(2026), None).unwrap();
        assert_eq!((p.year, p.month), (2026, 0));
        assert_eq!((p.since, p.until), (1_767_196_800, 1_798_732_800));
    }

    #[test]
    fn resolve_period_selects_the_current_month_without_a_year() {
        let p = period(None, None).unwrap();
        let now = OffsetDateTime::now_utc().unix_timestamp();
        assert!(p.month != 0 && p.since <= now && now < p.until, "{p:?}");
    }

    #[test]
    fn resolve_period_rejects_a_month_without_a_year_or_out_of_range() {
        assert!(period(None, Some(10)).is_err());
        assert!(period(Some(2026), Some(0)).is_err());
        assert!(period(Some(2026), Some(13)).is_err());
    }
}
