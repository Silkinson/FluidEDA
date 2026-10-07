//! Capability links (Phase 7.2): who may do what in a room, without accounts.
//!
//! A token is `room.role.expires.epoch.signature`:
//! * `role` — `viewer`, `editor` or `owner`;
//! * `expires` — Unix seconds, `0` = never;
//! * `epoch` — the room's key epoch; rotating it ([`Auth::rotate`]) revokes
//!   every link issued before;
//! * `signature` — HMAC-SHA256 of the rest under the server's secret
//!   (`store/secret`, 32 random bytes made on first start), hex.
//!
//! The token is not secret about what it grants (the client reads its role
//! to disable editing); it only cannot be forged or altered.

use axum::http::{header, HeaderMap, StatusCode};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::Path;

/// What a link lets its holder do. Ordered: each role can do what the ones
/// before it can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Watch the room: its state and every edit, live. Edits are dropped.
    Viewer,
    /// Edit, upload designs, export.
    Editor,
    /// Share (mint editor and viewer links) and revoke (rotate the key).
    Owner,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "viewer" => Some(Role::Viewer),
            "editor" => Some(Role::Editor),
            "owner" => Some(Role::Owner),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Editor => "editor",
            Role::Owner => "owner",
        }
    }
}

/// A checked token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub room: String,
    pub role: Role,
    pub expires: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Denied {
    /// No token where one is required.
    Missing,
    /// Not a token, or the signature does not match: forged or altered.
    Forged,
    Expired,
    /// Signed for an older key epoch: revoked by a rotation.
    Revoked,
    /// Valid, but for another room or a lesser role.
    NotAllowed,
}

impl Denied {
    /// 401 when the token does not count, 403 when it counts but falls short.
    pub fn status(self) -> StatusCode {
        match self {
            Denied::NotAllowed => StatusCode::FORBIDDEN,
            _ => StatusCode::UNAUTHORIZED,
        }
    }
    pub fn reason(self) -> &'static str {
        match self {
            Denied::Missing => "this server needs a link token (?t=…)",
            Denied::Forged => "the link token is not valid",
            Denied::Expired => "the link has expired",
            Denied::Revoked => "the link was revoked",
            Denied::NotAllowed => "the link does not allow this",
        }
    }
}

/// The token a request carries: `Authorization: Bearer <t>`, else `?t=<t>`
/// (a WebSocket cannot set headers).
pub fn token_of(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.trim().to_string());
    bearer.or_else(|| query.map(str::to_string)).filter(|t| !t.is_empty())
}

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub struct Auth {
    key: [u8; 32],
    /// Checks enforced (off on a loopback address unless asked for).
    pub required: bool,
}

const BLOCK: usize = 64;

/// HMAC-SHA256 (RFC 2104).
fn hmac(key: &[u8; 32], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; BLOCK];
    k[..32].copy_from_slice(key);
    let (mut ipad, mut opad) = ([0x36u8; BLOCK], [0x5cu8; BLOCK]);
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = Sha256::new().chain_update(ipad).chain_update(msg).finalize();
    Sha256::new().chain_update(opad).chain_update(inner).finalize().into()
}

/// Compare without an early exit, so timing does not leak the signature.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

impl Auth {
    /// The store's secret (`<root>/secret`), made on first use.
    pub fn open(root: &Path, required: bool) -> io::Result<Auth> {
        let path = root.join("secret");
        let key: [u8; 32] = match fs::read(&path) {
            Ok(k) if k.len() == 32 => k.try_into().unwrap(),
            Ok(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "store/secret is not 32 bytes")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut k = [0u8; 32];
                getrandom(&mut k)?;
                fs::write(&path, k)?;
                k
            }
            Err(e) => return Err(e),
        };
        Ok(Auth { key, required })
    }

    fn sign(&self, body: &str) -> String {
        crate::store::hex(&hmac(&self.key, body.as_bytes()))
    }

    /// A token for `room` as `role` until `expires` (0 = never), at key `epoch`.
    pub fn mint(&self, room: &str, role: Role, expires: u64, epoch: u64) -> String {
        let body = format!("{room}.{}.{expires}.{epoch}", role.as_str());
        let sig = self.sign(&body);
        format!("{body}.{sig}")
    }

    /// Check `token` at time `now` (Unix s) against the room's current key
    /// `epoch` (looked up by `epoch_of`).
    pub fn check(&self, token: &str, now: u64, epoch_of: impl Fn(&str) -> u64) -> Result<Grant, Denied> {
        let (body, sig) = token.rsplit_once('.').ok_or(Denied::Forged)?;
        if !same(self.sign(body).as_bytes(), sig.as_bytes()) {
            return Err(Denied::Forged);
        }
        let mut it = body.split('.');
        let (Some(room), Some(role), Some(expires), Some(epoch), None) = (it.next(), it.next(), it.next(), it.next(), it.next()) else {
            return Err(Denied::Forged);
        };
        let role = Role::parse(role).ok_or(Denied::Forged)?;
        let expires: u64 = expires.parse().map_err(|_| Denied::Forged)?;
        let epoch: u64 = epoch.parse().map_err(|_| Denied::Forged)?;
        if expires != 0 && now >= expires {
            return Err(Denied::Expired);
        }
        if epoch != epoch_of(room) {
            return Err(Denied::Revoked);
        }
        Ok(Grant { room: room.to_string(), role, expires })
    }
}

/// Random bytes from the OS.
fn getrandom(buf: &mut [u8]) -> io::Result<()> {
    getrandom::getrandom(buf).map_err(|e| io::Error::other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> Auth {
        Auth { key: [7u8; 32], required: true }
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // RFC 4231 test case 2 has a 4-byte key; ours are 32 bytes, so check
        // against the same construction with that key zero-padded instead:
        // HMAC(K) depends only on K padded to the block size.
        let mut key = [0u8; 32];
        key[..4].copy_from_slice(b"Jefe");
        let mac = hmac(&key, b"what do ya want for nothing?");
        assert_eq!(crate::store::hex(&mac), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }

    #[test]
    fn tokens_check_and_refuse() {
        let a = auth();
        let t = a.mint("r1", Role::Editor, 0, 0);
        assert_eq!(a.check(&t, 1_000, |_| 0), Ok(Grant { room: "r1".into(), role: Role::Editor, expires: 0 }));
        // Altered: the role, the room, the signature.
        assert_eq!(a.check(&t.replace("editor", "owner"), 1_000, |_| 0), Err(Denied::Forged));
        assert_eq!(a.check(&t.replacen("r1", "r2", 1), 1_000, |_| 0), Err(Denied::Forged));
        assert_eq!(a.check(&format!("{t}0"), 1_000, |_| 0), Err(Denied::Forged));
        assert_eq!(a.check("garbage", 1_000, |_| 0), Err(Denied::Forged));
        // Signed by another secret.
        let other = Auth { key: [8u8; 32], required: true };
        assert_eq!(a.check(&other.mint("r1", Role::Owner, 0, 0), 1_000, |_| 0), Err(Denied::Forged));
        // Expiry, and revocation by a newer key epoch.
        let short = a.mint("r1", Role::Viewer, 2_000, 0);
        assert!(a.check(&short, 1_999, |_| 0).is_ok());
        assert_eq!(a.check(&short, 2_000, |_| 0), Err(Denied::Expired));
        assert_eq!(a.check(&t, 1_000, |_| 1), Err(Denied::Revoked));
        assert!(a.check(&a.mint("r1", Role::Viewer, 0, 1), 1_000, |_| 1).is_ok());
    }

    #[test]
    fn roles_are_ordered() {
        assert!(Role::Viewer < Role::Editor && Role::Editor < Role::Owner);
        for r in [Role::Viewer, Role::Editor, Role::Owner] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
    }
}
