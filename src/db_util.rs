// db_util.rs

use anyhow::Context;
use sqlx::{
    Pool, Postgres,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tokio::time::{Duration, timeout};

const DB_JOB_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Debug)]
pub struct DbCtx {
    pub dbc: Pool<Postgres>,
}

#[derive(Debug)]
pub struct UrlCtx {
    pub ts: i64,
    pub chan: String,
    pub nick: String,
    pub url: String,
}

pub async fn start_db<S>(db_url: S) -> anyhow::Result<DbCtx>
where
    S: AsRef<str>,
{
    let options = db_url
        .as_ref()
        .parse::<PgConnectOptions>()?
        .options([("statement_timeout", "5000"), ("lock_timeout", "1000")]);
    // TLS remains configurable through the connection string (sslmode).
    let dbc = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(3))
        .connect_lazy_with(options);
    Ok(DbCtx { dbc })
}

const SQL_INSERT_URL: &str = "insert into url \
    (seen, channel, nick, url) \
    values ($1, $2, $3, $4)";

pub async fn db_add_url(db: &DbCtx, ur: &UrlCtx) -> anyhow::Result<u64> {
    anyhow::ensure!(
        ur.url.len() <= crate::harvest::MAX_URL_BYTES,
        "URL exceeds 2048 bytes"
    );
    anyhow::ensure!(
        ur.chan.len() <= 1024 && ur.nick.len() <= 256,
        "URL attribution exceeds storage limits"
    );
    timeout(DB_JOB_TIMEOUT, db_add_url_once(db, ur))
        .await
        .context("URL insert timed out")?
        .with_context(|| format!("URL insert failed for channel {}", ur.chan))
}

async fn db_add_url_once(db: &DbCtx, ur: &UrlCtx) -> anyhow::Result<u64> {
    let res = sqlx::query(SQL_INSERT_URL)
        .bind(ur.ts)
        .bind(&ur.chan)
        .bind(&ur.nick)
        .bind(&ur.url)
        .execute(&db.dbc)
        .await?;

    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{io::AsyncReadExt, net::TcpListener};

    #[tokio::test]
    async fn pool_creation_is_lazy_and_invalid_options_fail_locally() {
        let db = start_db("postgres://localhost:1/url?sslmode=disable")
            .await
            .unwrap();
        assert_eq!(db.dbc.size(), 0);
        assert!(start_db("not a database URL").await.is_err());
        assert!(start_db("postgres:///url?sslmode=invalid").await.is_err());
    }

    #[tokio::test]
    async fn oversized_records_fail_before_connecting() {
        let db = start_db("postgres://localhost:1/url?sslmode=disable")
            .await
            .unwrap();
        let row = UrlCtx {
            ts: 1,
            chan: "test".into(),
            nick: "sender".into(),
            url: "a".repeat(2049),
        };
        assert!(
            db_add_url(&db, &row)
                .await
                .unwrap_err()
                .to_string()
                .contains("2048")
        );
        assert_eq!(db.dbc.size(), 0);
    }

    #[tokio::test]
    async fn stalled_database_startup_is_bounded_and_explicit_disable_sends_no_ssl_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let db = start_db(format!("postgres://user@{address}/url?sslmode=disable"))
            .await
            .unwrap();
        let row = UrlCtx {
            ts: 1234,
            chan: "test".into(),
            nick: "sender".into(),
            url: "https://example.com/".into(),
        };
        let mut insert = Box::pin(db_add_url(&db, &row));
        let mut socket = tokio::select! {
            accepted = listener.accept() => accepted.unwrap().0,
            result = &mut insert => panic!("insert finished before connection: {result:?}"),
        };
        let mut header = [0u8; 8];
        tokio::select! {
            read = socket.read_exact(&mut header) => { read.unwrap(); },
            result = &mut insert => panic!("insert finished before startup: {result:?}"),
        }
        // PostgreSQL protocol 3 startup, rather than SSLRequest's 80877103.
        assert_eq!(u32::from_be_bytes(header[4..8].try_into().unwrap()), 196608);
        assert!(
            timeout(Duration::from_secs(5), &mut insert)
                .await
                .unwrap()
                .is_err()
        );
        drop(insert);
        drop(socket);
        timeout(Duration::from_secs(2), db.dbc.close())
            .await
            .unwrap();
    }
}
