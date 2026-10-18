mod db;
mod hysteria;
mod lark;
mod users;
mod xray;

use actix_cors::Cors;
use actix_web::web::{self, Data, Json};
use actix_web::{App, HttpResponse, HttpServer, Responder, error, guard, middleware};
use anyhow::Result;
use clap::Parser;
use log::{debug, error, info, warn};
use prost::Message;
use serde::Deserialize;
use sqlx::{Error::RowNotFound, query_as, query_scalar};
use time::{Date, Month, OffsetDateTime, UtcOffset};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{LocalSet, spawn_local};

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::db::connect;
use crate::lark::worker;
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

#[derive(Debug, Clone)]
struct AppProps {
    db_url: String,
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
    error!("sql error: {:?}", e);
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
    info!("request: {:?}", period);
    kick(&data.chan).await?;
    let mut conn = connect(&data.db_url, true).await.map_err(mapper)?;

    let rows = query_as!(
        Record,
        "SELECT * FROM Record WHERE time >= ? AND time < ? ORDER BY time",
        period.since,
        period.until,
    )
    .fetch_all(&mut conn)
    .await
    .map_err(mapper)?
    .into_iter()
    .map(|row| proto::Row {
        tid: row.tid as u64,
        time: row.time as u64,
        tx: row.tx as u64,
        rx: row.rx as u64,
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

#[actix_web::get("/{uid}")]
async fn query_sum(req: web::Path<u32>, data: Data<AppProps>) -> error::Result<impl Responder> {
    let uid = req.into_inner();
    info!("query sum for uid {}", uid);
    kick(&data.chan).await?;
    let mut conn = connect(&data.db_url, true).await.map_err(mapper)?;

    match query_scalar!("SELECT sum FROM Month WHERE uid = ? LIMIT 1", uid)
        .fetch_one(&mut conn)
        .await
    {
        Err(RowNotFound) => {
            warn!("uid {} not found", uid);
            Ok(HttpResponse::Ok().body("0"))
        }
        Err(e) => {
            error!("query {}: {:?}", uid, e);
            Err(mapper(e))
        }
        Ok(sum) => {
            info!("uid {} has sum {}", uid, sum);
            Ok(HttpResponse::Ok().body(format!("{}", sum)))
        }
    }
}

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Cli {
    // sqlx database urls have to be UTF-8 (&str)
    db_path: String,

    json_path: PathBuf,

    #[clap(short, long)]
    umask: Option<u32>,

    #[clap(short = 'A', long)]
    no_auth: bool,

    #[clap(short, long, default_value = "127.0.0.1:10090")]
    bind: OsString,

    #[clap(short = 'R', long)]
    no_reset: bool,

    #[clap(short, long, default_value = "300")]
    interval: u16,

    #[clap(short, long, default_value = "config.json")]
    config: PathBuf,
}

fn parse_socket_addr(s: &OsStr) -> Option<SocketAddr> {
    if let Some(addr) = s.to_str() {
        let addr: Result<SocketAddr, _> = addr.parse();
        if let Ok(addr) = addr {
            return Some(addr);
        }
    }
    None
}

#[derive(Deserialize)]
pub struct Config {
    token: String,
    tz: Option<(i8, i8, i8)>,
    #[serde(flatten)]
    lark: crate::lark::Config,
}

async fn entry() -> std::io::Result<()> {
    let args = Cli::parse();
    info!("running with {:?}", args);

    let users = load_users(&args.json_path);
    info!("found {} users", users.len());

    if let Some(mode) = args.umask {
        info!("setting umask to {:#o}", mode);
        unsafe {
            libc::umask(mode);
        }
    }

    let config: Config = serde_json::from_reader(BufReader::new(File::open(&args.config)?))?;

    let tz = match config.tz {
        Some((h, m, s)) => UtcOffset::from_hms(h, m, s).unwrap(),
        None => UtcOffset::current_local_offset().unwrap(),
    };
    info!("using timezone {}", tz);

    let users = Box::new(users).leak();
    let db_url = format!("sqlite://{}", args.db_path);
    let (tx, rx) = mpsc::channel(3);
    let _task = spawn_local(worker(
        &*users,
        tz,
        db_url.clone(),
        rx,
        args.interval,
        !args.no_reset,
        config.lark,
    ));

    let has_auth: bool = !args.no_auth;
    let data = Data::new(AppProps {
        db_url,
        users: &*users,
        tz,
        chan: tx,
    });

    let token = &*Box::leak(Box::new(config.token));
    let server = HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allow_any_header()
            .allow_any_method()
            .max_age(0);

        let mut scope = web::scope("/traffic");
        if has_auth {
            scope = scope.guard(guard::Header("Authorization", token));
        }

        App::new()
            .wrap(middleware::Logger::default())
            .wrap(cors)
            .app_data(data.clone())
            .service(scope.route("", web::post().to(index)).service(query_sum))
    })
    .workers(1);

    if let Some(addr) = parse_socket_addr(&args.bind) {
        server.bind(addr)
    } else {
        server.bind_uds(args.bind)
    }?
    .run()
    .await
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
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
