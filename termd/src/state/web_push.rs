//! Persistent Web Push identity and per-device subscriptions.
//!
//! This module owns only notification delivery state. It deliberately stays outside
//! `DaemonState`, whose trusted-device snapshot is rewritten during normal saves.

use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, types::Type};
use serde::{Deserialize, Serialize};
use termd_proto::DeviceId;
use web_push_native::p256::SecretKey;
use web_push_native::p256::elliptic_curve::rand_core::OsRng;
use web_push_native::p256::elliptic_curve::sec1::ToEncodedPoint as _;

use super::{
    StateError, ensure_compatible_connection, open_state_connection, parse_device_id, sqlite_error,
    sqlite_state_path_for_state_path, write_sqlite_state_version,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushNotificationMode {
    Attention,
    All,
}

impl PushNotificationMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Attention => "attention",
            Self::All => "all",
        }
    }

    fn parse(value: String, column: usize) -> rusqlite::Result<Self> {
        match value.as_str() {
            "attention" => Ok(Self::Attention),
            "all" => Ok(Self::All),
            _ => Err(invalid_text_value(
                column,
                "invalid Web Push notification mode",
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PushNotificationLocale {
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en-US")]
    EnUs,
}

impl PushNotificationLocale {
    fn as_str(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::EnUs => "en-US",
        }
    }

    fn parse(value: String, column: usize) -> rusqlite::Result<Self> {
        match value.as_str() {
            "zh-CN" => Ok(Self::ZhCn),
            "en-US" => Ok(Self::EnUs),
            _ => Err(invalid_text_value(column, "invalid Web Push locale")),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub mode: PushNotificationMode,
    pub locale: PushNotificationLocale,
    pub updated_at_ms: u64,
}

impl fmt::Debug for PushSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PushSubscription")
            .field("endpoint_configured", &!self.endpoint.is_empty())
            .field("p256dh_configured", &!self.p256dh.is_empty())
            .field("auth_configured", &!self.auth.is_empty())
            .field("mode", &self.mode)
            .field("locale", &self.locale)
            .field("updated_at_ms", &self.updated_at_ms)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct StoredPushSubscription {
    pub device_id: DeviceId,
    pub subscription: PushSubscription,
}

impl fmt::Debug for StoredPushSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredPushSubscription")
            .field("device_id", &self.device_id)
            .field("subscription", &self.subscription)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct VapidIdentity {
    pub public_key: String,
    pub private_key: String,
}

impl fmt::Debug for VapidIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VapidIdentity")
            .field("public_key", &self.public_key)
            .field("private_key_configured", &!self.private_key.is_empty())
            .finish()
    }
}

pub struct WebPushStore {
    path: PathBuf,
    conn: Connection,
    identity: VapidIdentity,
}

impl fmt::Debug for WebPushStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebPushStore")
            .field("path", &self.path)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl WebPushStore {
    pub fn open(state_path: impl AsRef<Path>) -> Result<Self, StateError> {
        let path = sqlite_state_path_for_state_path(state_path.as_ref());
        let mut conn = open_state_connection(&path)?;
        ensure_compatible_connection(&conn, &path)?;
        initialize_schema(&conn, &path)?;
        write_sqlite_state_version(&conn, &path)?;
        let identity = load_or_create_identity(&mut conn, &path)?;
        Ok(Self {
            path,
            conn,
            identity,
        })
    }

    pub fn vapid_public_key(&self) -> &str {
        &self.identity.public_key
    }

    pub fn upsert_subscription(
        &mut self,
        device_id: DeviceId,
        subscription: PushSubscription,
    ) -> Result<(), StateError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error(&self.path, source))?;
        tx.execute(
            "DELETE FROM web_push_subscriptions WHERE endpoint = ?1 AND device_id <> ?2",
            params![subscription.endpoint, device_id.0.to_string()],
        )
        .map_err(|source| sqlite_error(&self.path, source))?;
        tx.execute(
            r#"
            INSERT INTO web_push_subscriptions (
                device_id, endpoint, p256dh, auth, mode, locale, updated_at_ms
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(device_id) DO UPDATE SET
                endpoint = excluded.endpoint,
                p256dh = excluded.p256dh,
                auth = excluded.auth,
                mode = excluded.mode,
                locale = excluded.locale,
                updated_at_ms = excluded.updated_at_ms
            "#,
            params![
                device_id.0.to_string(),
                subscription.endpoint,
                subscription.p256dh,
                subscription.auth,
                subscription.mode.as_str(),
                subscription.locale.as_str(),
                subscription.updated_at_ms as i64,
            ],
        )
        .map_err(|source| sqlite_error(&self.path, source))?;
        tx.commit()
            .map_err(|source| sqlite_error(&self.path, source))
    }

    pub fn remove_subscription(&mut self, device_id: DeviceId) -> Result<bool, StateError> {
        let deleted = self
            .conn
            .execute(
                "DELETE FROM web_push_subscriptions WHERE device_id = ?1",
                params![device_id.0.to_string()],
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(deleted > 0)
    }

    pub(crate) fn remove_subscription_if_endpoint(
        &mut self,
        device_id: DeviceId,
        endpoint: &str,
    ) -> Result<bool, StateError> {
        let deleted = self
            .conn
            .execute(
                r#"
                DELETE FROM web_push_subscriptions
                WHERE device_id = ?1 AND endpoint = ?2
                "#,
                params![device_id.0.to_string(), endpoint],
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        Ok(deleted > 0)
    }

    pub fn subscription_for_device(
        &self,
        device_id: DeviceId,
    ) -> Result<Option<PushSubscription>, StateError> {
        self.conn
            .query_row(
                r#"
                SELECT endpoint, p256dh, auth, mode, locale, updated_at_ms
                FROM web_push_subscriptions
                WHERE device_id = ?1
                "#,
                params![device_id.0.to_string()],
                parse_subscription,
            )
            .optional()
            .map_err(|source| sqlite_error(&self.path, source))
    }

    pub(crate) fn list_stored_subscriptions(
        &self,
    ) -> Result<Vec<StoredPushSubscription>, StateError> {
        let mut statement = self
            .conn
            .prepare(
                r#"
                SELECT device_id, endpoint, p256dh, auth, mode, locale, updated_at_ms
                FROM web_push_subscriptions
                ORDER BY device_id
                "#,
            )
            .map_err(|source| sqlite_error(&self.path, source))?;
        let rows = statement
            .query_map([], |row| {
                Ok(StoredPushSubscription {
                    device_id: parse_device_id(row.get::<_, String>(0)?)?,
                    subscription: parse_subscription_offset(row, 1)?,
                })
            })
            .map_err(|source| sqlite_error(&self.path, source))?;
        let mut subscriptions = Vec::new();
        for row in rows {
            subscriptions.push(row.map_err(|source| sqlite_error(&self.path, source))?);
        }
        Ok(subscriptions)
    }

    pub fn list_subscriptions(&self) -> Result<Vec<(DeviceId, PushSubscription)>, StateError> {
        Ok(self
            .list_stored_subscriptions()?
            .into_iter()
            .map(|stored| (stored.device_id, stored.subscription))
            .collect())
    }

    pub(crate) fn delivery_material(
        &self,
    ) -> Result<(VapidIdentity, Vec<StoredPushSubscription>), StateError> {
        Ok((self.identity.clone(), self.list_stored_subscriptions()?))
    }
}

fn initialize_schema(conn: &Connection, path: &Path) -> Result<(), StateError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS web_push_identity (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            public_key TEXT NOT NULL,
            private_key TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS web_push_subscriptions (
            device_id TEXT PRIMARY KEY,
            endpoint TEXT NOT NULL UNIQUE,
            p256dh TEXT NOT NULL,
            auth TEXT NOT NULL,
            mode TEXT NOT NULL CHECK (mode IN ('attention', 'all')),
            locale TEXT NOT NULL CHECK (locale IN ('zh-CN', 'en-US')),
            updated_at_ms INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_web_push_subscriptions_updated
            ON web_push_subscriptions(updated_at_ms, device_id);
        "#,
    )
    .map_err(|source| sqlite_error(path, source))
}

fn load_or_create_identity(
    conn: &mut Connection,
    path: &Path,
) -> Result<VapidIdentity, StateError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| sqlite_error(path, source))?;
    let identity = tx
        .query_row(
            "SELECT public_key, private_key FROM web_push_identity WHERE singleton = 1",
            [],
            |row| {
                Ok(VapidIdentity {
                    public_key: row.get(0)?,
                    private_key: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(|source| sqlite_error(path, source))?;
    let identity = match identity {
        Some(identity) => validate_identity(identity)?,
        None => {
            let identity = generate_identity();
            tx.execute(
                r#"
                INSERT INTO web_push_identity (
                    singleton, public_key, private_key, created_at_ms
                ) VALUES (1, ?1, ?2, ?3)
                "#,
                params![
                    identity.public_key,
                    identity.private_key,
                    current_unix_timestamp_millis(),
                ],
            )
            .map_err(|source| sqlite_error(path, source))?;
            identity
        }
    };
    tx.commit().map_err(|source| sqlite_error(path, source))?;
    Ok(identity)
}

fn generate_identity() -> VapidIdentity {
    let secret = SecretKey::random(&mut OsRng);
    VapidIdentity {
        public_key: base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(secret.public_key().to_encoded_point(false).as_bytes()),
        private_key: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret.to_bytes()),
    }
}

fn validate_identity(identity: VapidIdentity) -> Result<VapidIdentity, StateError> {
    let private_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&identity.private_key)
        .map_err(|error| StateError::InvalidWebPushIdentity {
            source: error.to_string(),
        })?;
    let secret = SecretKey::from_slice(&private_bytes).map_err(|error| {
        StateError::InvalidWebPushIdentity {
            source: error.to_string(),
        }
    })?;
    let expected_public = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(secret.public_key().to_encoded_point(false).as_bytes());
    if identity.public_key != expected_public {
        return Err(StateError::InvalidWebPushIdentity {
            source: "public key does not match private key".to_owned(),
        });
    }
    Ok(identity)
}

fn parse_subscription(row: &rusqlite::Row<'_>) -> rusqlite::Result<PushSubscription> {
    parse_subscription_offset(row, 0)
}

fn parse_subscription_offset(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<PushSubscription> {
    let updated_at_ms = row.get::<_, i64>(offset + 5)?;
    if updated_at_ms < 0 {
        return Err(invalid_text_value(
            offset + 5,
            "invalid Web Push update timestamp",
        ));
    }
    Ok(PushSubscription {
        endpoint: row.get(offset)?,
        p256dh: row.get(offset + 1)?,
        auth: row.get(offset + 2)?,
        mode: PushNotificationMode::parse(row.get(offset + 3)?, offset + 3)?,
        locale: PushNotificationLocale::parse(row.get(offset + 4)?, offset + 4)?,
        updated_at_ms: updated_at_ms as u64,
    })
}

fn invalid_text_value(column: usize, message: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )),
    )
}

fn current_unix_timestamp_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
