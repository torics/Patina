//! Owner credentials and sessions. Tokens are stored only as SHA-256 hashes.
use crate::secrets::{random_bytes, to_hex};
use crate::{Db, StoreError};
use sha2::{Digest, Sha256};

pub fn hash_token(token: &str) -> String {
    to_hex(&Sha256::digest(token.as_bytes()))
}

/// A random 256-bit value as hex.
pub fn new_token() -> String {
    to_hex(&random_bytes::<32>())
}

/// Compare two strings without an early exit on the first difference.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().min(b.len()) {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

pub async fn get_setting(db: &Db, key: &str) -> Result<Option<String>, StoreError> {
    let key = key.to_string();
    db.call(move |c| {
        let mut s = c.prepare("SELECT value FROM settings WHERE key = ?1")?;
        let mut rows = s.query([key])?;
        Ok(match rows.next()? {
            Some(r) => Some(r.get(0)?),
            None => None,
        })
    })
    .await
}

pub async fn set_setting(db: &Db, key: &str, value: Option<&str>) -> Result<(), StoreError> {
    let key = key.to_string();
    let value = value.map(str::to_string);
    db.call(move |c| {
        match value {
            Some(v) => c.execute(
                "INSERT INTO settings(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![key, v],
            )?,
            None => c.execute("DELETE FROM settings WHERE key = ?1", [key])?,
        };
        Ok(())
    })
    .await
}

pub const PASSWORD_HASH: &str = "owner_password_hash";
pub const SETUP_TOKEN_HASH: &str = "setup_token_hash";

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub csrf_token: String,
    pub expires_at: i64,
}

/// Returns `(session token, csrf token)`. Only the token's hash is stored.
pub async fn create_session(db: &Db, now_ms: i64, ttl_ms: i64) -> Result<(String, String), StoreError> {
    let token = new_token();
    let csrf = new_token();
    let (h, c) = (hash_token(&token), csrf.clone());
    db.call(move |conn| {
        conn.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now_ms])?;
        conn.execute(
            "INSERT INTO sessions(token_hash, csrf_token, created_at, expires_at) VALUES(?1, ?2, ?3, ?4)",
            rusqlite::params![h, c, now_ms, now_ms + ttl_ms],
        )?;
        Ok(())
    })
    .await?;
    Ok((token, csrf))
}

pub async fn lookup_session(db: &Db, token: &str, now_ms: i64) -> Result<Option<Session>, StoreError> {
    let h = hash_token(token);
    db.call(move |c| {
        let mut s = c.prepare("SELECT csrf_token, expires_at FROM sessions WHERE token_hash = ?1 AND expires_at > ?2")?;
        let mut rows = s.query(rusqlite::params![h, now_ms])?;
        Ok(match rows.next()? {
            Some(r) => Some(Session { csrf_token: r.get(0)?, expires_at: r.get(1)? }),
            None => None,
        })
    })
    .await
}

pub async fn delete_session(db: &Db, token: &str) -> Result<(), StoreError> {
    let h = hash_token(token);
    db.call(move |c| {
        c.execute("DELETE FROM sessions WHERE token_hash = ?1", [h])?;
        Ok(())
    })
    .await
}

/// Used when the password is reset: every existing session ends.
pub async fn delete_all_sessions(db: &Db) -> Result<(), StoreError> {
    db.call(|c| {
        c.execute("DELETE FROM sessions", [])?;
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sessions_store_only_hashes_and_expire() {
        let t = tempfile::tempdir().unwrap();
        let db = Db::open(&t.path().join("p.db")).unwrap();
        let (token, csrf) = create_session(&db, 1_000, 5_000).await.unwrap();
        let s = lookup_session(&db, &token, 2_000).await.unwrap().unwrap();
        assert_eq!(s.csrf_token, csrf);
        assert!(lookup_session(&db, "wrong", 2_000).await.unwrap().is_none());
        assert!(lookup_session(&db, &token, 6_000).await.unwrap().is_none(), "expired");
        let stored: String = db
            .call(|c| Ok(c.query_row("SELECT token_hash FROM sessions", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_ne!(stored, token);
        delete_session(&db, &token).await.unwrap();
        assert!(lookup_session(&db, &token, 2_000).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn settings_set_get_clear() {
        let t = tempfile::tempdir().unwrap();
        let db = Db::open(&t.path().join("p.db")).unwrap();
        assert_eq!(get_setting(&db, "a").await.unwrap(), None);
        set_setting(&db, "a", Some("1")).await.unwrap();
        set_setting(&db, "a", Some("2")).await.unwrap();
        assert_eq!(get_setting(&db, "a").await.unwrap().as_deref(), Some("2"));
        set_setting(&db, "a", None).await.unwrap();
        assert_eq!(get_setting(&db, "a").await.unwrap(), None);
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }
}
