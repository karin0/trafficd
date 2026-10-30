use anyhow::Result;
use log::{Level, debug, error, info, log, trace, warn};
use sqlx::{Connection, SqliteConnection, SqlitePool, query, query_scalar};
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, Instant, sleep, timeout};

use std::collections::BTreeMap;
use std::time::SystemTime;

use crate::hysteria;
use crate::users::User;
use crate::xray;

const KIND_XRAY: u8 = 0;
const KIND_HYSTERIA: u8 = 1;

fn encode_tid(uid: u32, kind: u8) -> u64 {
    (u64::from(uid) << 8) | u64::from(kind)
}

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

// Traffic by tid, fetched with its counters reset and not committed yet
type Pending = BTreeMap<u64, (u64, u64)>;

#[derive(Debug)]
struct RefreshContext {
    time: u64,
    last_time: u64,
    reset: bool,
    stat_updated: u32,
    stat_skipped: u32,
    stat_tx: u64,
    stat_rx: u64,
}

impl RefreshContext {
    async fn update(
        &mut self,
        conn: &mut SqliteConnection,
        tid: u64,
        tx: u64,
        rx: u64,
    ) -> Result<()> {
        if tx == 0 && rx == 0 {
            self.stat_skipped += 1;
            return Ok(());
        }

        let time: i64 = self.time.try_into().unwrap();
        let tid: i64 = tid.try_into().unwrap();
        let itx: i64 = tx.try_into().unwrap();
        let irx: i64 = rx.try_into().unwrap();
        let last_time = self.last_time;

        if last_time > 0 {
            // Query Last by tid
            let tid_last_time: u64 =
                query_scalar!("SELECT time FROM Last WHERE tid = ? LIMIT 1", tid)
                    .fetch_optional(&mut *conn)
                    .await?
                    .map_or(0, |x| x.try_into().unwrap());
            trace!("update: {tid}: last {tid_last_time:?}");

            if tid_last_time < last_time {
                // Flush a zero to Record first
                trace!("update: {tid}: last {tid_last_time} < {last_time}, inserting zeros");

                let last_time = last_time.cast_signed();
                query!(
                    "INSERT INTO Record (tid, time, tx, rx) VALUES (?, ?, 0, 0)",
                    tid,
                    last_time,
                )
                .execute(&mut *conn)
                .await?;
            }
        }

        // Update Last
        trace!("update: {tid}: updating last {time}");
        query!(
            "INSERT OR REPLACE INTO Last (tid, time) VALUES (?, ?)",
            tid,
            time,
        )
        .execute(&mut *conn)
        .await?;

        // Insert Record
        trace!("update: {tid}: inserting {time} {tx} {rx}");
        query!(
            "INSERT INTO Record (tid, time, tx, rx) VALUES (?, ?, ?, ?)",
            tid,
            time,
            itx,
            irx,
        )
        .execute(&mut *conn)
        .await?;

        self.stat_updated += 1;
        self.stat_tx += tx;
        self.stat_rx += rx;
        Ok(())
    }

    async fn refresh_xray(
        &self,
        src: &xray::Source,
        name_map: &BTreeMap<&str, u32>,
    ) -> Result<impl Iterator<Item = (u32, u64, u64)> + use<>> {
        let res = timeout(FETCH_TIMEOUT, src.query(self.reset)).await??;
        debug!("refresh_xray: got {res:?}");

        let map = sum_xray(res.iter(), name_map);
        trace!("refresh_xray: parsed {map:?}");

        Ok(map.into_iter().map(|(uid, (tx, rx))| (uid, tx, rx)))
    }

    async fn refresh_hysteria<'a>(
        &self,
        src: &'a hysteria::Source,
        uid_map: &'a BTreeMap<&str, u32>,
    ) -> Result<impl Iterator<Item = (u32, u64, u64)> + use<'a>> {
        let res = timeout(FETCH_TIMEOUT, src.traffic(self.reset)).await??;
        debug!("refresh_hysteria: got {res:?}");

        Ok(res.into_iter().filter_map(|(user, traffic)| {
            uid_map
                .get(user.as_str())
                .map(|uid| (*uid, traffic.tx, traffic.rx))
                .or_else(|| {
                    error!("refresh_hysteria: bad user {user:?} with traffic {traffic:?}");
                    None
                })
        }))
    }
}

// Every email of a user adds to the user's traffic
fn sum_xray<'a>(
    stats: impl Iterator<Item = xray::Stat<'a>>,
    name_map: &BTreeMap<&str, u32>,
) -> BTreeMap<u32, (u64, u64)> {
    let mut map: BTreeMap<u32, (u64, u64)> = BTreeMap::new();
    for stat in stats {
        if let Some(uid) = name_map.get(User::name_from_xray_email(stat.email)) {
            let (tx, rx) = map.entry(*uid).or_default();
            *if stat.is_tx { tx } else { rx } += stat.value;
        } else {
            error!("refresh_xray: unknown email in stat {stat:?}");
        }
    }
    map
}

fn collect<T: Iterator<Item = (u32, u64, u64)>>(
    pending: &mut Pending,
    traffic: Result<T>,
    title: &'static str,
    kind: u8,
) -> bool {
    match traffic {
        Ok(traffic) => {
            for (uid, tx, rx) in traffic {
                let (ptx, prx) = pending.entry(encode_tid(uid, kind)).or_default();
                *ptx += tx;
                *prx += rx;
            }
            true
        }
        Err(e) => {
            error!("Failed to refresh {title}: {e}");
            false
        }
    }
}

pub struct Sources {
    xray: xray::Source,
    xray_map: BTreeMap<&'static str, u32>,
    hysteria: hysteria::Source,
    hysteria_map: BTreeMap<&'static str, u32>,
}

impl Sources {
    pub fn new(users: &'static [User], xray: xray::Source, hysteria: hysteria::Source) -> Self {
        Self {
            xray,
            xray_map: users.iter().map(|u| (u.name.as_str(), u.uid)).collect(),
            hysteria,
            hysteria_map: users.iter().map(|u| (u.hysteria_user(), u.uid)).collect(),
        }
    }
}

async fn refresh(
    pool: &SqlitePool,
    src: &Sources,
    reset: bool,
    pending: &mut Pending,
) -> Result<()> {
    let begin = Instant::now();
    let mut conn = pool.acquire().await?;

    let last_time = query_scalar!("SELECT time FROM Meta LIMIT 1")
        .fetch_optional(&mut *conn)
        .await?
        .map_or(0, |x| x.try_into().unwrap());
    trace!("refresh: last {last_time:?}");

    let time = OffsetDateTime::now_utc().unix_timestamp().cast_unsigned();
    trace!("refresh: start at {time}");

    if time <= last_time {
        warn!("refresh: too soon");
        return Ok(());
    }

    let mut ctx = RefreshContext {
        time,
        last_time,
        reset,
        stat_updated: 0,
        stat_skipped: 0,
        stat_tx: 0,
        stat_rx: 0,
    };

    let (xray, hysteria) = tokio::join!(
        ctx.refresh_xray(&src.xray, &src.xray_map),
        ctx.refresh_hysteria(&src.hysteria, &src.hysteria_map)
    );
    let fetch_dt = begin.elapsed().as_millis();

    // The counters are reset by now, so pending keeps the traffic until the
    // transaction below commits
    let xray_ok = collect(pending, xray, "xray", KIND_XRAY);
    let hysteria_ok = collect(pending, hysteria, "hysteria", KIND_HYSTERIA);
    let ok = xray_ok && hysteria_ok;

    let mut trx = conn.begin().await?;

    for (&tid, &(tx, rx)) in pending.iter() {
        ctx.update(&mut trx, tid, tx, rx).await?;
    }

    if ok {
        // Update the last successful refresh time
        let itime: i64 = time.try_into().unwrap();
        query!(
            "INSERT OR REPLACE INTO Meta (id, time) VALUES (0, ?)",
            itime
        )
        .execute(&mut *trx)
        .await?;
    }

    trx.commit().await?;
    pending.clear();

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

pub async fn worker(
    src: Sources,
    pool: SqlitePool,
    mut chan: mpsc::Receiver<oneshot::Sender<bool>>,
    interval: u16,
    reset: bool,
) {
    let delay_millis = u64::from(interval) * 1000;

    let mut last_tx: Option<oneshot::Sender<bool>> = None;
    let mut pending = Pending::new();

    loop {
        let ok = if let Err(e) = refresh(&pool, &src, reset, &mut pending).await {
            error!("failed to refresh: {e}");
            false
        } else {
            true
        };

        if let Some(tx) = last_tx.take() {
            if let Err(e) = tx.send(ok) {
                error!("failed to respond: {e}");
            } else {
                debug!("responded {ok} to request");
            }
        }

        // Clear the channel
        let mut cnt = 0;
        while let Ok(tx) = chan.try_recv() {
            if let Err(e) = tx.send(ok) {
                error!("failed to send: {e}");
            }
            cnt += 1;
        }
        if cnt > 0 {
            info!("cleared {cnt} requests");
        }

        let t: u64 = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .try_into()
            .unwrap();

        let dt = delay_millis - t % delay_millis;
        debug!("sleeping for {} ms from {} to {}", dt, t, t + dt);

        tokio::select! {
            () = sleep(Duration::from_millis(dt)) => {}
            req = chan.recv() => {
                if let Some(tx) = req {
                    trace!("woke from request {tx:?}");
                    last_tx = Some(tx);
                } else {
                    error!("channel closed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_xray_adds_the_emails_of_a_user() {
        let stat = |email, is_tx, value| xray::Stat {
            email,
            is_tx,
            value,
        };
        let name_map = BTreeMap::from([("a", 1), ("b", 2)]);
        let stats = [
            stat("a@x", true, 1),
            stat("a@y", true, 2),
            stat("a@x", false, 4),
            stat("b@x", false, 8),
            stat("c@x", true, 16),
        ];
        assert_eq!(
            sum_xray(stats.into_iter(), &name_map),
            BTreeMap::from([(1, (3, 4)), (2, (0, 8))])
        );
    }
}
