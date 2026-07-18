use base64::Engine as _;
use termd::state::web_push::{
    PushNotificationLocale, PushNotificationMode, PushSubscription, WebPushStore,
};
use termd::state::{DaemonState, StateStore};
use termd_proto::DeviceId;

struct TestStateDir {
    path: std::path::PathBuf,
}

impl TestStateDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("termd-web-push-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn state_path(&self) -> std::path::PathBuf {
        self.path.join("daemon-state.json")
    }
}

impl Drop for TestStateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn subscription(endpoint: &str, mode: PushNotificationMode) -> PushSubscription {
    PushSubscription {
        endpoint: endpoint.to_owned(),
        p256dh: "BOr7h1hC51A1QJ9XQq9D3EjHh8I1FCbZ1t8oErT1Y4G4n_KL5QFz3lTZFYq2SVCYnNEZ4uF7G2xT1dP4mS3aF5A".to_owned(),
        auth: "BTBZMqHH6r4Tts7J_aSIgg".to_owned(),
        mode,
        locale: PushNotificationLocale::ZhCn,
        updated_at_ms: 1_721_234_567_890,
    }
}

#[test]
fn vapid_identity_is_generated_once_and_survives_reopen() {
    let state_dir = TestStateDir::new("identity");
    let state_path = state_dir.state_path();

    let first = WebPushStore::open(&state_path).unwrap();
    let first_public_key = first.vapid_public_key().to_owned();
    drop(first);

    let reopened = WebPushStore::open(&state_path).unwrap();
    assert_eq!(reopened.vapid_public_key(), first_public_key);

    let public_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(first_public_key)
        .unwrap();
    assert_eq!(public_key.len(), 65);
    assert_eq!(public_key[0], 4);
}

#[test]
fn subscriptions_are_device_owned_replaceable_and_independent_from_state_snapshots() {
    let state_dir = TestStateDir::new("subscriptions");
    let state_path = state_dir.state_path();
    let device_a = DeviceId::new();
    let device_b = DeviceId::new();
    let mut store = WebPushStore::open(&state_path).unwrap();

    store
        .upsert_subscription(
            device_a,
            subscription(
                "https://push.example.test/a-old",
                PushNotificationMode::Attention,
            ),
        )
        .unwrap();
    store
        .upsert_subscription(
            device_b,
            subscription("https://push.example.test/b", PushNotificationMode::All),
        )
        .unwrap();
    let replacement = subscription("https://push.example.test/a-new", PushNotificationMode::All);
    store
        .upsert_subscription(device_a, replacement.clone())
        .unwrap();

    assert_eq!(store.list_subscriptions().unwrap().len(), 2);
    assert_eq!(
        store.subscription_for_device(device_a).unwrap(),
        Some(replacement)
    );
    drop(store);

    StateStore::save(&state_path, &DaemonState::default()).unwrap();
    let mut reopened = WebPushStore::open(&state_path).unwrap();
    assert_eq!(reopened.list_subscriptions().unwrap().len(), 2);
    assert!(reopened.remove_subscription(device_a).unwrap());
    assert!(!reopened.remove_subscription(device_a).unwrap());
    assert!(
        reopened
            .subscription_for_device(device_b)
            .unwrap()
            .is_some()
    );
}
