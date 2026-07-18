//! Daemon-owned Web Push coordination.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::Request;
use base64::Engine as _;
use serde::Serialize;
use termd_proto::{
    DeviceId, ServerId, SessionActivityAgent, SessionActivityState, SessionAiActivityPayload,
    SessionId,
};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;
use web_push_native::jwt_simple::algorithms::ES256KeyPair;
use web_push_native::{Auth, WebPushBuilder};

use crate::state::StateError;
use crate::state::web_push::{
    PushNotificationLocale, PushNotificationMode, PushSubscription, StoredPushSubscription,
    VapidIdentity, WebPushStore,
};

const PUSH_EVENT_QUEUE_CAPACITY: usize = 32;
const PUSH_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const PUSH_MESSAGE_TTL: Duration = Duration::from_secs(60 * 60);
const PUSH_MESSAGE_MAX_BYTES: usize = 4 * 1024;
const PUSH_SESSION_NAME_MAX_CHARS: usize = 80;
const VAPID_CONTACT: &str = "mailto:notifications@termd.local";

type PushDeliveryFuture =
    Pin<Box<dyn Future<Output = Result<PushDeliveryResponse, PushDeliveryError>> + Send + 'static>>;

trait PushDelivery: Send + Sync {
    fn deliver(&self, request: Request<Vec<u8>>) -> PushDeliveryFuture;
}

struct PushDeliveryResponse {
    status: u16,
}

#[derive(Debug, Clone, Copy, Error)]
#[error("Web Push transport failed")]
struct PushDeliveryError;

struct ReqwestPushDelivery {
    client: Option<reqwest::Client>,
}

impl ReqwestPushDelivery {
    fn new() -> Self {
        let client = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(PUSH_HTTP_TIMEOUT)
            .build()
            .ok();
        Self { client }
    }
}

impl PushDelivery for ReqwestPushDelivery {
    fn deliver(&self, request: Request<Vec<u8>>) -> PushDeliveryFuture {
        let client = self.client.clone();
        Box::pin(async move {
            let client = client.ok_or(PushDeliveryError)?;
            let (parts, body) = request.into_parts();
            let response = client
                .post(parts.uri.to_string())
                .headers(parts.headers)
                .body(body)
                .send()
                .await
                .map_err(|_| PushDeliveryError)?;
            Ok(PushDeliveryResponse {
                status: response.status().as_u16(),
            })
        })
    }
}

#[derive(Debug, Error)]
pub enum PushNotificationError {
    #[error("Web Push state persistence failed")]
    State(#[from] StateError),
    #[error("Web Push state is unavailable")]
    StoreUnavailable,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SessionActivitySnapshot {
    pub session_id: SessionId,
    pub session_name: Option<String>,
    pub activity: Option<SessionAiActivityPayload>,
}

impl fmt::Debug for SessionActivitySnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionActivitySnapshot")
            .field("session_id", &self.session_id)
            .field("session_name_configured", &self.session_name.is_some())
            .field("activity", &self.activity)
            .finish()
    }
}

struct ActivityEvent {
    session_id: SessionId,
    session_name: Option<String>,
    activity: SessionAiActivityPayload,
}

#[derive(Default)]
struct ActivityTracker {
    initialized: bool,
    previous: HashMap<SessionId, Option<SessionAiActivityPayload>>,
}

impl ActivityTracker {
    fn initialize(&mut self, snapshot: Vec<SessionActivitySnapshot>) {
        if self.initialized {
            return;
        }
        self.previous = snapshot
            .iter()
            .map(|session| (session.session_id, session.activity))
            .collect::<HashMap<_, _>>();
        self.initialized = true;
    }

    fn observe_change(&mut self, session: SessionActivitySnapshot) -> Option<ActivityEvent> {
        if !self.initialized {
            self.initialized = true;
        }
        let previous = self
            .previous
            .insert(session.session_id, session.activity)
            .flatten();
        let activity = session.activity?;
        let should_notify = match activity.state {
            SessionActivityState::Attention => {
                previous.is_none_or(|previous| previous.state != SessionActivityState::Attention)
            }
            SessionActivityState::Idle | SessionActivityState::Completed => {
                previous.is_some_and(|previous| previous.state == SessionActivityState::Running)
            }
            SessionActivityState::Running => false,
        };
        should_notify.then_some(ActivityEvent {
            session_id: session.session_id,
            session_name: session.session_name,
            activity,
        })
    }
}

#[derive(Clone)]
pub struct PushNotificationCoordinator {
    server_id: ServerId,
    store: Option<Arc<Mutex<WebPushStore>>>,
    activity_tracker: Arc<Mutex<ActivityTracker>>,
    delivery: Arc<dyn PushDelivery>,
    event_tx: mpsc::Sender<ActivityEvent>,
    event_rx: Arc<Mutex<Option<mpsc::Receiver<ActivityEvent>>>>,
}

impl fmt::Debug for PushNotificationCoordinator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PushNotificationCoordinator")
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

impl PushNotificationCoordinator {
    pub fn open(server_id: ServerId, state_path: impl AsRef<Path>) -> Result<Self, StateError> {
        Self::open_with_delivery(server_id, state_path, Arc::new(ReqwestPushDelivery::new()))
    }

    fn open_with_delivery(
        server_id: ServerId,
        state_path: impl AsRef<Path>,
        delivery: Arc<dyn PushDelivery>,
    ) -> Result<Self, StateError> {
        let (event_tx, event_rx) = mpsc::channel(PUSH_EVENT_QUEUE_CAPACITY);
        Ok(Self {
            server_id,
            store: Some(Arc::new(Mutex::new(WebPushStore::open(state_path)?))),
            activity_tracker: Arc::new(Mutex::new(ActivityTracker::default())),
            delivery,
            event_tx,
            event_rx: Arc::new(Mutex::new(Some(event_rx))),
        })
    }

    pub(crate) fn unavailable(server_id: ServerId) -> Self {
        let (event_tx, event_rx) = mpsc::channel(PUSH_EVENT_QUEUE_CAPACITY);
        Self {
            server_id,
            store: None,
            activity_tracker: Arc::new(Mutex::new(ActivityTracker::default())),
            delivery: Arc::new(ReqwestPushDelivery::new()),
            event_tx,
            event_rx: Arc::new(Mutex::new(Some(event_rx))),
        }
    }

    pub(crate) fn is_available(&self) -> bool {
        self.store.is_some()
    }

    pub fn server_id(&self) -> ServerId {
        self.server_id
    }

    pub fn application_server_key(&self) -> Result<String, PushNotificationError> {
        Ok(self.lock_store()?.vapid_public_key().to_owned())
    }

    pub fn is_subscribed(&self, device_id: DeviceId) -> Result<bool, PushNotificationError> {
        Ok(self
            .lock_store()?
            .subscription_for_device(device_id)?
            .is_some())
    }

    pub fn upsert_subscription(
        &self,
        device_id: DeviceId,
        subscription: PushSubscription,
    ) -> Result<(), PushNotificationError> {
        self.lock_store()?
            .upsert_subscription(device_id, subscription)?;
        Ok(())
    }

    pub fn remove_subscription_if_endpoint(
        &self,
        device_id: DeviceId,
        endpoint: &str,
    ) -> Result<(), PushNotificationError> {
        self.lock_store()?
            .remove_subscription_if_endpoint(device_id, endpoint)?;
        Ok(())
    }

    pub fn initialize_activity_snapshot(&self, snapshot: Vec<SessionActivitySnapshot>) {
        if !self.is_available() {
            return;
        }
        match self.activity_tracker.lock() {
            Ok(mut tracker) => tracker.initialize(snapshot),
            Err(_) => {
                warn!("Web Push activity tracker is unavailable");
            }
        }
    }

    pub fn observe_activity_change(&self, change: SessionActivitySnapshot) -> bool {
        if !self.is_available() {
            return false;
        }
        let event = match self.activity_tracker.lock() {
            Ok(mut tracker) => tracker.observe_change(change),
            Err(_) => {
                warn!("Web Push activity tracker is unavailable");
                return false;
            }
        };
        let Some(event) = event else {
            return false;
        };
        if self.event_tx.try_send(event).is_ok() {
            true
        } else {
            warn!("Web Push activity queue is full or closed");
            false
        }
    }

    pub(crate) fn start_delivery_worker(&self) -> Option<JoinHandle<()>> {
        if !self.is_available() {
            return None;
        }
        let mut receiver = self.event_rx.lock().ok()?.take()?;
        let coordinator = self.clone();
        Some(tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                coordinator.deliver_activity_event(event).await;
            }
        }))
    }

    async fn deliver_activity_event(&self, event: ActivityEvent) {
        let (identity, subscriptions) = match self.lock_store() {
            Ok(store) => match store.delivery_material() {
                Ok(material) => material,
                Err(_) => {
                    warn!("Web Push delivery state could not be read");
                    return;
                }
            },
            Err(_) => {
                warn!("Web Push delivery state is unavailable");
                return;
            }
        };

        for stored in subscriptions {
            if !subscription_accepts_event(stored.subscription.mode, event.activity.state) {
                continue;
            }
            let endpoint = stored.subscription.endpoint.clone();
            let request =
                match build_push_request(self.server_id, &identity, &stored.subscription, &event) {
                    Ok(request) => request,
                    Err(()) => {
                        warn!("Stored Web Push subscription is invalid");
                        self.remove_subscription_if_current(&stored, &endpoint);
                        continue;
                    }
                };
            match self.delivery.deliver(request).await {
                Ok(response) if (200..300).contains(&response.status) => {}
                Ok(response) if matches!(response.status, 404 | 410) => {
                    self.remove_subscription_if_current(&stored, &endpoint);
                }
                Ok(response) => {
                    warn!(
                        status = response.status,
                        "Web Push provider rejected delivery"
                    );
                }
                Err(_) => warn!("Web Push delivery failed"),
            }
        }
    }

    fn remove_subscription_if_current(&self, stored: &StoredPushSubscription, endpoint: &str) {
        let result = self.lock_store().and_then(|mut store| {
            store
                .remove_subscription_if_endpoint(stored.device_id, endpoint)
                .map(|_| ())
                .map_err(PushNotificationError::from)
        });
        if result.is_err() {
            warn!("Stale Web Push subscription could not be removed");
        }
    }

    fn lock_store(&self) -> Result<std::sync::MutexGuard<'_, WebPushStore>, PushNotificationError> {
        self.store
            .as_ref()
            .ok_or(PushNotificationError::StoreUnavailable)?
            .lock()
            .map_err(|_| PushNotificationError::StoreUnavailable)
    }
}

fn subscription_accepts_event(mode: PushNotificationMode, state: SessionActivityState) -> bool {
    match mode {
        PushNotificationMode::Attention => state == SessionActivityState::Attention,
        PushNotificationMode::All => true,
    }
}

#[derive(Serialize)]
struct PushMessagePayload {
    version: u8,
    server_id: ServerId,
    session_id: SessionId,
    session_name: String,
    agent: &'static str,
    state: &'static str,
    title: &'static str,
    body: String,
    target_url: String,
}

fn build_push_request(
    server_id: ServerId,
    identity: &VapidIdentity,
    subscription: &PushSubscription,
    event: &ActivityEvent,
) -> Result<Request<Vec<u8>>, ()> {
    let private_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&identity.private_key)
        .map_err(|_| ())?;
    let vapid = ES256KeyPair::from_bytes(&private_key).map_err(|_| ())?;
    let p256dh = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&subscription.p256dh)
        .map_err(|_| ())?;
    let ua_public = web_push_native::p256::PublicKey::from_sec1_bytes(&p256dh).map_err(|_| ())?;
    let auth = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&subscription.auth)
        .map_err(|_| ())?;
    if auth.len() != 16 {
        return Err(());
    }
    let auth = Auth::clone_from_slice(&auth);
    let payload = push_message_payload(server_id, subscription.locale, event);
    let body = serde_json::to_vec(&payload).map_err(|_| ())?;
    if body.len() > PUSH_MESSAGE_MAX_BYTES {
        return Err(());
    }
    WebPushBuilder::new(
        subscription.endpoint.parse().map_err(|_| ())?,
        ua_public,
        auth,
    )
    .with_valid_duration(PUSH_MESSAGE_TTL)
    .with_vapid(&vapid, VAPID_CONTACT)
    .build(body)
    .map_err(|_| ())
}

fn push_message_payload(
    server_id: ServerId,
    locale: PushNotificationLocale,
    event: &ActivityEvent,
) -> PushMessagePayload {
    let session_name =
        sanitized_session_name(event.session_name.as_deref(), event.session_id, locale);
    let agent = activity_agent_wire(event.activity.agent);
    let state = activity_state_wire(event.activity.state);
    let agent_label = activity_agent_label(event.activity.agent);
    let status = match (locale, event.activity.state) {
        (PushNotificationLocale::ZhCn, SessionActivityState::Idle) => {
            format!("{agent_label} 已就绪")
        }
        (PushNotificationLocale::ZhCn, SessionActivityState::Attention) => {
            format!("{agent_label} 需要操作")
        }
        (PushNotificationLocale::ZhCn, SessionActivityState::Completed) => {
            format!("{agent_label} 已完成")
        }
        (PushNotificationLocale::EnUs, SessionActivityState::Idle) => {
            format!("{agent_label} is ready")
        }
        (PushNotificationLocale::EnUs, SessionActivityState::Attention) => {
            format!("{agent_label} needs attention")
        }
        (PushNotificationLocale::EnUs, SessionActivityState::Completed) => {
            format!("{agent_label} finished")
        }
        (_, SessionActivityState::Running) => format!("{agent_label} is running"),
    };
    let body = match locale {
        PushNotificationLocale::ZhCn => format!("{session_name}：{status}"),
        PushNotificationLocale::EnUs => format!("{session_name}: {status}"),
    };
    PushMessagePayload {
        version: 1,
        server_id,
        session_id: event.session_id,
        session_name,
        agent,
        state,
        title: "Termd",
        body,
        target_url: format!(
            "?termd_server_id={}&termd_session_id={}",
            server_id.0, event.session_id.0
        ),
    }
}

fn sanitized_session_name(
    name: Option<&str>,
    session_id: SessionId,
    locale: PushNotificationLocale,
) -> String {
    let sanitized = name
        .unwrap_or_default()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(PUSH_SESSION_NAME_MAX_CHARS)
        .collect::<String>();
    let sanitized = sanitized.trim();
    if !sanitized.is_empty() {
        return sanitized.to_owned();
    }
    let short_id = session_id.0.to_string();
    let short_id = &short_id[..8];
    match locale {
        PushNotificationLocale::ZhCn => format!("会话 {short_id}"),
        PushNotificationLocale::EnUs => format!("Session {short_id}"),
    }
}

fn activity_agent_wire(agent: SessionActivityAgent) -> &'static str {
    match agent {
        SessionActivityAgent::Codex => "codex",
        SessionActivityAgent::ClaudeCode => "claude_code",
        SessionActivityAgent::OpenCode => "opencode",
        SessionActivityAgent::ZCode => "zcode",
    }
}

fn activity_agent_label(agent: SessionActivityAgent) -> &'static str {
    match agent {
        SessionActivityAgent::Codex => "Codex",
        SessionActivityAgent::ClaudeCode => "Claude Code",
        SessionActivityAgent::OpenCode => "OpenCode",
        SessionActivityAgent::ZCode => "ZCode",
    }
}

fn activity_state_wire(state: SessionActivityState) -> &'static str {
    match state {
        SessionActivityState::Idle => "idle",
        SessionActivityState::Running => "running",
        SessionActivityState::Attention => "attention",
        SessionActivityState::Completed => "completed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use termd_proto::{
        SessionActivityAgent, SessionActivityKind, SessionActivityState, SessionAiActivityPayload,
        SessionId, UnixTimestampMillis,
    };
    use tokio::sync::Notify;
    use web_push_native::p256::elliptic_curve::sec1::ToEncodedPoint as _;

    struct TestStateDir(std::path::PathBuf);

    impl TestStateDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "termd-push-coordinator-{label}-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn state_path(&self) -> std::path::PathBuf {
            self.0.join("daemon-state.json")
        }
    }

    impl Drop for TestStateDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Clone, Copy)]
    enum FakeOutcome {
        Status(u16),
        TransportError,
    }

    struct FakeDelivery {
        requests: Mutex<Vec<axum::http::Request<Vec<u8>>>>,
        outcomes: Mutex<VecDeque<FakeOutcome>>,
        request_count: AtomicUsize,
        notify: Notify,
    }

    impl FakeDelivery {
        fn new(outcomes: impl IntoIterator<Item = FakeOutcome>) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                outcomes: Mutex::new(outcomes.into_iter().collect()),
                request_count: AtomicUsize::new(0),
                notify: Notify::new(),
            }
        }

        async fn wait_for_requests(&self, expected: usize) {
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if self.request_count.load(Ordering::SeqCst) >= expected {
                        return;
                    }
                    self.notify.notified().await;
                }
            })
            .await
            .unwrap();
        }

        fn take_request(&self) -> axum::http::Request<Vec<u8>> {
            self.requests.lock().unwrap().remove(0)
        }
    }

    impl PushDelivery for FakeDelivery {
        fn deliver(&self, request: axum::http::Request<Vec<u8>>) -> PushDeliveryFuture {
            self.requests.lock().unwrap().push(request);
            self.request_count.fetch_add(1, Ordering::SeqCst);
            self.notify.notify_waiters();
            let outcome = self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(FakeOutcome::Status(201));
            Box::pin(async move {
                match outcome {
                    FakeOutcome::Status(status) => Ok(PushDeliveryResponse { status }),
                    FakeOutcome::TransportError => Err(PushDeliveryError),
                }
            })
        }
    }

    fn activity_snapshot(
        session_id: SessionId,
        state: SessionActivityState,
        changed_at_ms: u64,
    ) -> SessionActivitySnapshot {
        SessionActivitySnapshot {
            session_id,
            session_name: Some("Release shell".to_owned()),
            activity: Some(SessionAiActivityPayload {
                kind: SessionActivityKind::Ai,
                agent: SessionActivityAgent::Codex,
                state,
                changed_at_ms: UnixTimestampMillis(changed_at_ms),
            }),
        }
    }

    fn test_subscription(
        endpoint: &str,
        mode: crate::state::web_push::PushNotificationMode,
    ) -> (
        crate::state::web_push::PushSubscription,
        web_push_native::p256::SecretKey,
        web_push_native::Auth,
    ) {
        use base64::Engine as _;

        let ua_private = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94")
            .unwrap();
        let ua_private = web_push_native::p256::SecretKey::from_slice(&ua_private).unwrap();
        let auth = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode("BTBZMqHH6r4Tts7J_aSIgg")
            .unwrap();
        let auth_array = web_push_native::Auth::clone_from_slice(&auth);
        (
            crate::state::web_push::PushSubscription {
                endpoint: endpoint.to_owned(),
                p256dh: base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(ua_private.public_key().to_encoded_point(false).as_bytes()),
                auth: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(auth),
                mode,
                locale: crate::state::web_push::PushNotificationLocale::ZhCn,
                updated_at_ms: 1,
            },
            ua_private,
            auth_array,
        )
    }

    #[test]
    fn delayed_delete_does_not_remove_a_replacement_endpoint() {
        let state_dir = TestStateDir::new("conditional-delete");
        let coordinator = PushNotificationCoordinator::open_with_delivery(
            ServerId::new(),
            state_dir.state_path(),
            Arc::new(FakeDelivery::new([])),
        )
        .unwrap();
        let device_id = DeviceId::new();
        let (old_subscription, _, _) = test_subscription(
            "https://push.example.test/old",
            crate::state::web_push::PushNotificationMode::All,
        );
        let (new_subscription, _, _) = test_subscription(
            "https://push.example.test/new",
            crate::state::web_push::PushNotificationMode::All,
        );

        coordinator
            .upsert_subscription(device_id, old_subscription)
            .unwrap();
        coordinator
            .upsert_subscription(device_id, new_subscription)
            .unwrap();
        coordinator
            .remove_subscription_if_endpoint(device_id, "https://push.example.test/old")
            .unwrap();

        assert!(coordinator.is_subscribed(device_id).unwrap());
        coordinator
            .remove_subscription_if_endpoint(device_id, "https://push.example.test/new")
            .unwrap();
        assert!(!coordinator.is_subscribed(device_id).unwrap());
    }

    #[tokio::test]
    async fn baseline_does_not_notify_and_running_completion_is_encrypted_once() {
        let state_dir = TestStateDir::new("baseline");
        let delivery = Arc::new(FakeDelivery::new([]));
        let coordinator = PushNotificationCoordinator::open_with_delivery(
            ServerId::new(),
            state_dir.state_path(),
            delivery.clone(),
        )
        .unwrap();
        let worker = coordinator.start_delivery_worker().unwrap();
        let device_id = DeviceId::new();
        let session_id = SessionId::new();
        let (subscription, ua_private, auth) = test_subscription(
            "https://push.example.test/baseline",
            crate::state::web_push::PushNotificationMode::All,
        );
        coordinator
            .upsert_subscription(device_id, subscription)
            .unwrap();

        coordinator.initialize_activity_snapshot(vec![activity_snapshot(
            session_id,
            SessionActivityState::Running,
            1,
        )]);
        assert!(coordinator.observe_activity_change(activity_snapshot(
            session_id,
            SessionActivityState::Completed,
            2,
        )));
        delivery.wait_for_requests(1).await;
        assert!(!coordinator.observe_activity_change(activity_snapshot(
            session_id,
            SessionActivityState::Completed,
            2,
        )));

        let request = delivery.take_request();
        assert_ne!(request.body().as_slice(), b"Release shell");
        let plaintext = web_push_native::decrypt(request.into_body(), &ua_private, &auth).unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&plaintext).unwrap();
        assert_eq!(payload["version"], 1);
        assert_eq!(payload["session_id"], session_id.0.to_string());
        assert_eq!(payload["session_name"], "Release shell");
        assert_eq!(payload["agent"], "codex");
        assert_eq!(payload["state"], "completed");
        assert_eq!(
            payload["target_url"],
            format!(
                "?termd_server_id={}&termd_session_id={}",
                coordinator.server_id().0,
                session_id.0
            )
        );
        assert!(payload.get("terminal_output").is_none());
        worker.abort();
    }

    #[test]
    fn attention_is_notified_from_idle_and_for_a_new_session() {
        let existing_session = SessionId::new();
        let new_session = SessionId::new();
        let mut tracker = ActivityTracker::default();
        tracker.initialize(vec![activity_snapshot(
            existing_session,
            SessionActivityState::Idle,
            1,
        )]);

        assert!(
            tracker
                .observe_change(activity_snapshot(
                    existing_session,
                    SessionActivityState::Attention,
                    2,
                ))
                .is_some()
        );
        assert!(
            tracker
                .observe_change(activity_snapshot(
                    new_session,
                    SessionActivityState::Attention,
                    3,
                ))
                .is_some()
        );
        assert!(
            tracker
                .observe_change(activity_snapshot(
                    new_session,
                    SessionActivityState::Attention,
                    3,
                ))
                .is_none()
        );
    }

    #[test]
    fn payload_localizes_and_sanitizes_session_names() {
        let session_id = SessionId::new();
        let fallback_event = ActivityEvent {
            session_id,
            session_name: Some("\n\t".to_owned()),
            activity: SessionAiActivityPayload {
                kind: SessionActivityKind::Ai,
                agent: SessionActivityAgent::ClaudeCode,
                state: SessionActivityState::Attention,
                changed_at_ms: UnixTimestampMillis(1),
            },
        };
        let fallback = push_message_payload(
            ServerId::new(),
            PushNotificationLocale::EnUs,
            &fallback_event,
        );
        assert_eq!(
            fallback.session_name,
            format!("Session {}", &session_id.0.to_string()[..8])
        );
        assert_eq!(fallback.agent, "claude_code");
        assert_eq!(fallback.state, "attention");
        assert!(fallback.body.ends_with(": Claude Code needs attention"));

        let sanitized_event = ActivityEvent {
            session_id,
            session_name: Some(format!("{}\nignored", "会".repeat(100))),
            activity: SessionAiActivityPayload {
                kind: SessionActivityKind::Ai,
                agent: SessionActivityAgent::ZCode,
                state: SessionActivityState::Idle,
                changed_at_ms: UnixTimestampMillis(2),
            },
        };
        let sanitized = push_message_payload(
            ServerId::new(),
            PushNotificationLocale::ZhCn,
            &sanitized_event,
        );
        assert_eq!(
            sanitized.session_name.chars().count(),
            PUSH_SESSION_NAME_MAX_CHARS
        );
        assert!(!sanitized.session_name.chars().any(char::is_control));
        assert_eq!(sanitized.agent, "zcode");
        assert_eq!(sanitized.state, "idle");
        assert!(sanitized.body.ends_with("：ZCode 已就绪"));
    }

    #[tokio::test]
    async fn attention_mode_filters_completion_but_delivers_attention() {
        let state_dir = TestStateDir::new("attention-mode");
        let delivery = Arc::new(FakeDelivery::new([]));
        let coordinator = PushNotificationCoordinator::open_with_delivery(
            ServerId::new(),
            state_dir.state_path(),
            delivery.clone(),
        )
        .unwrap();
        let worker = coordinator.start_delivery_worker().unwrap();
        let session_id = SessionId::new();
        let (subscription, ua_private, auth) = test_subscription(
            "https://push.example.test/attention",
            crate::state::web_push::PushNotificationMode::Attention,
        );
        coordinator
            .upsert_subscription(DeviceId::new(), subscription)
            .unwrap();

        coordinator.initialize_activity_snapshot(vec![activity_snapshot(
            session_id,
            SessionActivityState::Running,
            1,
        )]);
        assert!(coordinator.observe_activity_change(activity_snapshot(
            session_id,
            SessionActivityState::Completed,
            2,
        )));
        assert!(!coordinator.observe_activity_change(activity_snapshot(
            session_id,
            SessionActivityState::Running,
            3,
        )));
        assert!(coordinator.observe_activity_change(activity_snapshot(
            session_id,
            SessionActivityState::Attention,
            4,
        )));
        delivery.wait_for_requests(1).await;
        let request = delivery.take_request();
        let plaintext = web_push_native::decrypt(request.into_body(), &ua_private, &auth).unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&plaintext).unwrap();
        assert_eq!(payload["state"], "attention");
        worker.abort();
    }

    #[tokio::test]
    async fn gone_endpoint_is_removed_without_affecting_activity_processing() {
        let state_dir = TestStateDir::new("gone");
        let delivery = Arc::new(FakeDelivery::new([FakeOutcome::Status(410)]));
        let coordinator = PushNotificationCoordinator::open_with_delivery(
            ServerId::new(),
            state_dir.state_path(),
            delivery.clone(),
        )
        .unwrap();
        let worker = coordinator.start_delivery_worker().unwrap();
        let device_id = DeviceId::new();
        let session_id = SessionId::new();
        let (subscription, _, _) = test_subscription(
            "https://push.example.test/gone",
            crate::state::web_push::PushNotificationMode::All,
        );
        coordinator
            .upsert_subscription(device_id, subscription)
            .unwrap();

        coordinator.initialize_activity_snapshot(vec![activity_snapshot(
            session_id,
            SessionActivityState::Running,
            1,
        )]);
        assert!(coordinator.observe_activity_change(activity_snapshot(
            session_id,
            SessionActivityState::Completed,
            2,
        )));
        delivery.wait_for_requests(1).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.is_subscribed(device_id).unwrap() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        worker.abort();
    }

    #[tokio::test]
    async fn invalid_stored_subscription_is_removed_without_delivery() {
        let state_dir = TestStateDir::new("invalid-subscription");
        let delivery = Arc::new(FakeDelivery::new([]));
        let coordinator = PushNotificationCoordinator::open_with_delivery(
            ServerId::new(),
            state_dir.state_path(),
            delivery.clone(),
        )
        .unwrap();
        let worker = coordinator.start_delivery_worker().unwrap();
        let device_id = DeviceId::new();
        let session_id = SessionId::new();
        let (mut subscription, _, _) = test_subscription(
            "https://push.example.test/invalid",
            crate::state::web_push::PushNotificationMode::All,
        );
        subscription.p256dh = "not-base64url".to_owned();
        coordinator
            .upsert_subscription(device_id, subscription)
            .unwrap();

        coordinator.initialize_activity_snapshot(vec![activity_snapshot(
            session_id,
            SessionActivityState::Running,
            1,
        )]);
        assert!(coordinator.observe_activity_change(activity_snapshot(
            session_id,
            SessionActivityState::Completed,
            2,
        )));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.is_subscribed(device_id).unwrap() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(delivery.request_count.load(Ordering::SeqCst), 0);
        worker.abort();
    }

    #[tokio::test]
    async fn delivery_failures_keep_subscription_and_worker_processes_next_event() {
        let state_dir = TestStateDir::new("failure");
        let delivery = Arc::new(FakeDelivery::new([
            FakeOutcome::TransportError,
            FakeOutcome::Status(503),
            FakeOutcome::Status(201),
        ]));
        let coordinator = PushNotificationCoordinator::open_with_delivery(
            ServerId::new(),
            state_dir.state_path(),
            delivery.clone(),
        )
        .unwrap();
        let worker = coordinator.start_delivery_worker().unwrap();
        let device_id = DeviceId::new();
        let session_id = SessionId::new();
        let (subscription, _, _) = test_subscription(
            "https://push.example.test/failure",
            crate::state::web_push::PushNotificationMode::All,
        );
        coordinator
            .upsert_subscription(device_id, subscription)
            .unwrap();

        coordinator.initialize_activity_snapshot(vec![activity_snapshot(
            session_id,
            SessionActivityState::Running,
            1,
        )]);
        for (state, changed_at_ms) in [
            (SessionActivityState::Attention, 2),
            (SessionActivityState::Running, 3),
            (SessionActivityState::Idle, 4),
            (SessionActivityState::Running, 5),
            (SessionActivityState::Completed, 6),
        ] {
            coordinator.observe_activity_change(activity_snapshot(
                session_id,
                state,
                changed_at_ms,
            ));
        }
        delivery.wait_for_requests(3).await;
        assert!(coordinator.is_subscribed(device_id).unwrap());
        worker.abort();
    }
}
