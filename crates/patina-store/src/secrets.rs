//! Encrypted API keys. Keys are sealed with XChaCha20-Poly1305 and bound to their name.
//! This protects backups and copied database files, not a compromised host account.
use crate::{Db, StoreError};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use std::path::Path;

pub const MASTER_KEY_ENV: &str = "PATINA_MASTER_KEY";
const KEY_FILE: &str = "master.key";

pub fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("operating system random source");
    b
}

fn parse_key(hex: &str, origin: &str) -> Result<[u8; 32], StoreError> {
    from_hex(hex)
        .and_then(|v| <[u8; 32]>::try_from(v).ok())
        .ok_or_else(|| StoreError::Other(format!("{origin} must hold a 32-byte key as 64 hex characters")))
}

/// The single place the master key is found. A keyring source can be added here later.
/// Order: the `PATINA_MASTER_KEY` variable, then `master.key` in the data folder (0600), created on first use.
pub fn master_key(data_dir: &Path) -> Result<[u8; 32], StoreError> {
    if let Ok(v) = std::env::var(MASTER_KEY_ENV) {
        return parse_key(&v, MASTER_KEY_ENV);
    }
    let path = data_dir.join(KEY_FILE);
    match std::fs::read_to_string(&path) {
        Ok(t) => parse_key(&t, &path.display().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = random_bytes::<32>();
            write_private(&path, to_hex(&key).as_bytes())
                .map_err(|e| StoreError::Other(format!("cannot write {}: {e}", path.display())))?;
            Ok(key)
        }
        Err(e) => Err(StoreError::Other(format!("cannot read {}: {e}", path.display()))),
    }
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)?.write_all(data)
}

#[derive(Clone)]
pub struct Secrets {
    db: Db,
    key: [u8; 32],
}

impl Secrets {
    pub fn new(db: Db, key: [u8; 32]) -> Secrets {
        Secrets { db, key }
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new((&self.key).into())
    }

    pub fn seal(&self, name: &str, plaintext: &str) -> Result<([u8; 24], Vec<u8>), StoreError> {
        let nonce = random_bytes::<24>();
        let ct = self
            .cipher()
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: name.as_bytes(),
                },
            )
            .map_err(|_| StoreError::Other("encryption failed".into()))?;
        Ok((nonce, ct))
    }

    pub fn open_sealed(&self, name: &str, nonce: &[u8], ct: &[u8]) -> Result<String, StoreError> {
        let nonce = XNonce::try_from(nonce).map_err(|_| StoreError::Other("stored secret is corrupt".into()))?;
        let pt = self
            .cipher()
            .decrypt(
                &nonce,
                Payload {
                    msg: ct,
                    aad: name.as_bytes(),
                },
            )
            .map_err(|_| StoreError::Other(format!("cannot decrypt secret {name:?}: wrong master key or corrupt data")))?;
        String::from_utf8(pt).map_err(|_| StoreError::Other("stored secret is not text".into()))
    }

    pub async fn put(&self, name: &str, value: &str) -> Result<(), StoreError> {
        let (nonce, ct) = self.seal(name, value)?;
        let name = name.to_string();
        self.db
            .call(move |c| {
                c.execute(
                    "INSERT INTO secrets(name, nonce, ciphertext) VALUES(?1, ?2, ?3)
                     ON CONFLICT(name) DO UPDATE SET nonce = excluded.nonce, ciphertext = excluded.ciphertext",
                    rusqlite::params![name, nonce.to_vec(), ct],
                )?;
                Ok(())
            })
            .await
    }

    /// For server-side use only: the plaintext must never be returned by the API.
    pub async fn get(&self, name: &str) -> Result<Option<String>, StoreError> {
        let n = name.to_string();
        let row = self
            .db
            .call(move |c| {
                let mut s = c.prepare("SELECT nonce, ciphertext FROM secrets WHERE name = ?1")?;
                let mut rows = s.query([n])?;
                Ok(match rows.next()? {
                    Some(r) => Some((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
                    None => None,
                })
            })
            .await?;
        row.map(|(n, c)| self.open_sealed(name, &n, &c)).transpose()
    }

    /// Every stored value, for removing them from command output. For server-side use only.
    pub async fn values(&self) -> Result<Vec<String>, StoreError> {
        let rows = self
            .db
            .call(|c| {
                let mut s = c.prepare("SELECT name, nonce, ciphertext FROM secrets")?;
                let rows = s
                    .query_map([], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, Vec<u8>>(2)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?;
        // A value that cannot be opened is skipped here; reading it for use reports the problem.
        Ok(rows
            .into_iter()
            .filter_map(|(name, nonce, ct)| self.open_sealed(&name, &nonce, &ct).ok())
            .collect())
    }

    /// The last four characters, for display. Never more.
    pub async fn hint(&self, name: &str) -> Result<Option<String>, StoreError> {
        Ok(self.get(name).await?.map(|v| {
            let chars: Vec<char> = v.chars().collect();
            chars[chars.len().saturating_sub(4)..].iter().collect()
        }))
    }

    pub async fn delete(&self, name: &str) -> Result<(), StoreError> {
        let n = name.to_string();
        self.db
            .call(move |c| {
                c.execute("DELETE FROM secrets WHERE name = ?1", [n])?;
                Ok(())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn secrets(dir: &Path) -> Secrets {
        let db = Db::open(&dir.join("p.db")).unwrap();
        Secrets::new(db, [7u8; 32])
    }

    #[tokio::test]
    async fn round_trip_and_ciphertext_hides_the_key() {
        let t = tempfile::tempdir().unwrap();
        let s = secrets(t.path()).await;
        s.put("openai", "sk-secret-value-1234").await.unwrap();
        assert_eq!(s.get("openai").await.unwrap().as_deref(), Some("sk-secret-value-1234"));
        assert_eq!(s.hint("openai").await.unwrap().as_deref(), Some("1234"));
        assert_eq!(s.get("missing").await.unwrap(), None);
        s.put("mcp/search/TOKEN", "tok-0987654321").await.unwrap();
        let mut all = s.values().await.unwrap();
        all.sort();
        assert_eq!(all, ["sk-secret-value-1234", "tok-0987654321"]);
        // The stored bytes never contain the plaintext.
        let raw = std::fs::read(t.path().join("p.db")).unwrap();
        let wal = std::fs::read(t.path().join("p.db-wal")).unwrap_or_default();
        for blob in [raw, wal] {
            assert!(!blob.windows(9).any(|w| w == b"sk-secret"));
        }
    }

    #[tokio::test]
    async fn ciphertext_is_bound_to_its_name_and_key() {
        let t = tempfile::tempdir().unwrap();
        let s = secrets(t.path()).await;
        let (n, ct) = s.seal("a", "value").unwrap();
        assert!(s.open_sealed("b", &n, &ct).is_err(), "renamed secret must not decrypt");
        let other = Secrets::new(s.db.clone(), [8u8; 32]);
        assert!(other.open_sealed("a", &n, &ct).is_err(), "wrong master key must not decrypt");
    }

    #[tokio::test]
    async fn overwrite_and_delete() {
        let t = tempfile::tempdir().unwrap();
        let s = secrets(t.path()).await;
        s.put("k", "one").await.unwrap();
        s.put("k", "two").await.unwrap();
        assert_eq!(s.get("k").await.unwrap().as_deref(), Some("two"));
        s.delete("k").await.unwrap();
        assert_eq!(s.get("k").await.unwrap(), None);
    }

    #[test]
    fn master_key_file_is_created_private_and_stable() {
        let t = tempfile::tempdir().unwrap();
        // SAFETY of env: no other test in this crate reads PATINA_MASTER_KEY.
        let a = master_key(t.path()).unwrap();
        assert_eq!(master_key(t.path()).unwrap(), a);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(t.path().join(KEY_FILE)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(from_hex(&to_hex(&[0, 255, 16])), Some(vec![0, 255, 16]));
        assert_eq!(from_hex("zz"), None);
    }
}
