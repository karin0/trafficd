use anyhow::Result;
use log::{Level, debug, error, info, log, trace, warn};
use serde::Deserialize;
use sqlx::{Connection, SqliteConnection, query, query_scalar};
use time::{OffsetDateTime, UtcOffset};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, Instant, sleep};

use std::collections::{BTreeMap, btree_map::Entry};
use std::time::SystemTime;

use crate::db::connect;
use crate::hysteria;
use crate::users::User;
use crate::xray;

const KIND_XRAY: u8 = 0;
const KIND_HYSTERIA: u8 = 1;

fn encode_tid(uid: u32, kind: u8) -> u64 {
    ((uid as u64) << 8) | kind as u64
}

#[derive(Debug)]
struct RefreshContext {
    conn: SqliteConnection,
    time: u64,
    last_time: u64,
    reset: bool,
    stat_updated: u32,
    stat_skipped: u32,
    stat_tx: u64,
    stat_rx: u64,
}

impl RefreshContext {
    async fn update(&mut self, tid: u64, tx: u64, rx: u64) -> Result<()> {
        if tx == 0 && rx == 0 {
            self.stat_skipped += 1;
            return Ok(());
        }

        // Start a transaction
        let mut trx = self.conn.begin().await?;

        let time: i64 = self.time.try_into().unwrap();
        let tid: i64 = tid.try_into().unwrap();
        let itx: i64 = tx.try_into().unwrap();
        let irx: i64 = rx.try_into().unwrap();
        let last_time = self.last_time;

        if last_time > 0 {
            // Query Last by tid
            let tid_last_time: u64 =
                query_scalar!("SELECT time FROM Last WHERE tid = ? LIMIT 1", tid)
                    .fetch_optional(&mut *trx)
                    .await?
                    .map(|x| x.try_into().unwrap())
                    .unwrap_or(0);
            trace!("update: {}: last {:?}", tid, tid_last_time);

            if tid_last_time < last_time {
                // Flush a zero to Record first
                trace!(
                    "update: {}: last {} < {}, inserting zeros",
                    tid, tid_last_time, last_time
                );

                let last_time = last_time as i64;
                query!(
                    "INSERT INTO Record (tid, time, tx, rx) VALUES (?, ?, 0, 0)",
                    tid,
                    last_time,
                )
                .execute(&mut *trx)
                .await?;
            }
        }

        // Update Last
        trace!("update: {}: updating last {}", tid, time);
        query!(
            "INSERT OR REPLACE INTO Last (tid, time) VALUES (?, ?)",
            tid,
            time,
        )
        .execute(&mut *trx)
        .await?;

        // Insert Record
        trace!("update: {}: inserting {} {} {}", tid, time, tx, rx);
        query!(
            "INSERT INTO Record (tid, time, tx, rx) VALUES (?, ?, ?, ?)",
            tid,
            time,
            itx,
            irx,
        )
        .execute(&mut *trx)
        .await?;

        trx.commit().await?;
        self.stat_updated += 1;
        self.stat_tx += tx;
        self.stat_rx += rx;
        Ok(())
    }

    async fn refresh_xray(
        &self,
        server: &str,
        name_map: &BTreeMap<&str, u32>,
    ) -> Result<impl Iterator<Item = (u32, u64, u64)> + use<>> {
        let res = xray::query(server, self.reset).await?;
        debug!("refresh_xray: got {:?}", res);

        let mut map: BTreeMap<u32, (u64, u64)> = BTreeMap::new();
        for stat in res.iter() {
            if let Some(uid) = name_map.get(User::name_from_xray_email(stat.email)) {
                match map.entry(*uid) {
                    Entry::Vacant(e) => {
                        if stat.is_tx {
                            e.insert((stat.value, 0));
                        } else {
                            e.insert((0, stat.value));
                        }
                    }
                    Entry::Occupied(mut e) => {
                        let (tx, rx) = e.get_mut();
                        if stat.is_tx {
                            assert_eq!(*tx, 0);
                            *tx = stat.value;
                        } else {
                            assert_eq!(*rx, 0);
                            *rx = stat.value;
                        }
                    }
                }
            } else {
                error!("refresh_xray: unknown email in stat {:?}", stat);
            }
        }
        trace!("refresh_xray: parsed {:?}", map);

        Ok(map.into_iter().map(|(uid, (tx, rx))| (uid, tx, rx)))
    }

    async fn refresh_hysteria<'a>(
        &self,
        src: &'a hysteria::Source,
        uid_map: &'a BTreeMap<&str, u32>,
    ) -> Result<impl Iterator<Item = (u32, u64, u64)> + use<'a>> {
        let res = src.traffic(self.reset).await?;
        debug!("refresh_hysteria: got {:?}", res);

        Ok(res.into_iter().flat_map(|(user, traffic)| {
            uid_map
                .get(user.as_str())
                .map(|uid| (*uid, traffic.tx, traffic.rx))
                .or_else(|| {
                    error!(
                        "refresh_hysteria: bad user {:?} with traffic {:?}",
                        user, traffic
                    );
                    None
                })
        }))
    }

    async fn try_update<T: Iterator<Item = (u32, u64, u64)>>(
        &mut self,
        traffic: Result<T>,
        title: &'static str,
        kind: u8,
        uid_sum: &mut BTreeMap<u32, u64>,
    ) -> Result<bool> {
        match traffic {
            Ok(traffic) => {
                for (uid, tx, rx) in traffic {
                    self.update(encode_tid(uid, kind), tx, rx).await?;
                    match uid_sum.entry(uid) {
                        Entry::Vacant(e) => {
                            e.insert(tx + rx);
                        }
                        Entry::Occupied(mut e) => {
                            *e.get_mut() += tx + rx;
                        }
                    }
                }
                Ok(true)
            }
            Err(e) => {
                error!("Failed to refresh {}: {}", title, e);
                Ok(false)
            }
        }
    }
}

async fn refresh(
    tz: UtcOffset,
    db_url: &str,
    name_map: &BTreeMap<&str, u32>,
    xray: &str,
    hys: &hysteria::Source,
    hys_map: &BTreeMap<&str, u32>,
    reset: bool,
) -> Result<()> {
    let begin = Instant::now();
    let mut conn = connect(db_url, false).await?;

    let last_time = query_scalar!("SELECT time FROM Meta LIMIT 1")
        .fetch_optional(&mut conn)
        .await?
        .map(|x| x.try_into().unwrap())
        .unwrap_or(0);
    trace!("refresh: last {:?}", last_time);

    let dt = OffsetDateTime::now_utc().to_offset(tz);
    let time = dt.unix_timestamp() as u64;
    trace!("refresh: start at {}", time);

    if time <= last_time {
        warn!("refresh: too soon");
        return Ok(());
    }

    let mut ctx = RefreshContext {
        conn,
        time,
        last_time,
        reset,
        stat_updated: 0,
        stat_skipped: 0,
        stat_tx: 0,
        stat_rx: 0,
    };

    let (xray, hysteria) = tokio::join!(
        ctx.refresh_xray(xray, name_map),
        ctx.refresh_hysteria(hys, hys_map)
    );
    let fetch_dt = begin.elapsed().as_millis();

    let month: u8 = dt.month().into();
    let last_month = if last_time > 0 {
        OffsetDateTime::from_unix_timestamp(last_time as i64)
            .unwrap()
            .to_offset(tz)
            .month()
            .into()
    } else {
        0
    };
    if month != last_month {
        let r = query!("UPDATE Month SET sum = 0")
            .execute(&mut ctx.conn)
            .await?
            .rows_affected();
        warn!(
            "refresh: month changed from {} to {}, reset {} rows",
            last_month, month, r
        );
    } else {
        trace!("refresh: month is still {}", month);
    }

    let mut sum = BTreeMap::new();
    let ok = ctx.try_update(xray, "xray", KIND_XRAY, &mut sum).await?
        && ctx
            .try_update(hysteria, "hysteria", KIND_HYSTERIA, &mut sum)
            .await?;

    for (uid, sum) in sum {
        let uid = uid as i32;
        let sum = sum as i64;
        query!(
            "INSERT INTO Month (uid, sum) VALUES (?, ?) ON CONFLICT(uid) DO UPDATE SET sum = sum + excluded.sum",
            uid,
            sum,
        )
        .execute(&mut ctx.conn)
        .await?;
    }

    if ok {
        // Update the last successful refresh time
        let itime: i64 = time.try_into().unwrap();
        query!(
            "INSERT OR REPLACE INTO Meta (id, time) VALUES (0, ?)",
            itime
        )
        .execute(&mut ctx.conn)
        .await?;
    }

    let dt = begin.elapsed().as_millis();
    log!(
        if ok { Level::Debug } else { Level::Error },
        "refresh: updated {} skipped {} tx {} rx {} in {} + {} ms after {} s",
        ctx.stat_updated,
        ctx.stat_skipped,
        ctx.stat_tx,
        ctx.stat_rx,
        fetch_dt,
        dt - fetch_dt,
        time - last_time,
    );

    Ok(())
}

#[derive(Deserialize)]
pub struct Config {
    pub xray: crate::xray::Config,
    pub hysteria: crate::hysteria::Config,
    pub tz: Option<(i8, i8, i8)>,
}

pub async fn worker(
    users: &[User],
    db_url: String,
    mut chan: mpsc::Receiver<oneshot::Sender<bool>>,
    interval: u16,
    reset: bool,
    config: Config,
) {
    let delay_millis = interval as u64 * 1000;

    let name_map = BTreeMap::from_iter(users.iter().map(|u| (u.name.as_str(), u.uid)));
    let hys = hysteria::Source::new(config.hysteria);
    let hys_uid_map = BTreeMap::from_iter(users.iter().map(|u| (u.hysteria_user(), u.uid)));
    let mut last_tx: Option<oneshot::Sender<bool>> = None;

    let tz = match config.tz {
        Some((h, m, s)) => UtcOffset::from_hms(h, m, s).unwrap(),
        None => UtcOffset::current_local_offset().unwrap(),
    };
    info!("using timezone {}", tz);

    loop {
        let ok = if let Err(e) = refresh(
            tz,
            &db_url,
            &name_map,
            &config.xray.server,
            &hys,
            &hys_uid_map,
            reset,
        )
        .await
        {
            error!("failed to refresh: {}", e);
            false
        } else {
            true
        };

        if let Some(tx) = last_tx.take() {
            if let Err(e) = tx.send(ok) {
                error!("failed to respond: {}", e);
            } else {
                debug!("responded {} to request", ok);
            }
        }

        // Clear the channel
        let mut cnt = 0;
        while let Ok(tx) = chan.try_recv() {
            if let Err(e) = tx.send(ok) {
                error!("failed to send: {}", e);
            }
            cnt += 1;
        }
        if cnt > 0 {
            info!("cleared {} requests", cnt);
        }

        let t = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let dt = delay_millis - t % delay_millis;
        debug!("sleeping for {} ms from {} to {}", dt, t, t + dt);

        tokio::select! {
            _ = sleep(Duration::from_millis(dt)) => {}
            req = chan.recv() => {
                if let Some(tx) = req {
                    trace!("woke from request {:?}", tx);
                    last_tx = Some(tx);
                } else {
                    error!("channel closed");
                }
            }
        }
    }
}
