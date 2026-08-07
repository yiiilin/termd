//! v070 metadata 通道上的客户端诊断上报（`client.diagnostics`）。
//!
//! 前端在设置里显式开启「上送分析日志」后，把本页的诊断事件批量回传 daemon；
//! 这里只做严格校验、脱敏和单连接预算控制，然后写一条结构化 `tracing` 记录进入
//! daemon 日志（journalctl）。不落盘、不持久化、不进入 relay，也不持有任何状态。
//!
//! direct（`server::run_metadata_websocket`）与 relay（`relay::run_relay_v070_metadata`）
//! 两条 metadata 连接路径共用本模块，保证行为一致。

use serde_json::Value;
use termd_proto::DeviceId;

pub(crate) const CLIENT_DIAGNOSTICS_TYPE: &str = "client.diagnostics";

/// 单批最多事件数；前端每 2 秒或满 300 条 flush 一次，这里留足余量。
const MAX_EVENTS_PER_BATCH: usize = 512;
const MAX_CONTEXT_ID_LEN: usize = 64;
const MAX_EVENT_NAME_LEN: usize = 128;
const MAX_FIELD_KEYS_PER_EVENT: usize = 64;
/// 单个字段值序列化后的最大长度；超长字段直接丢弃，避免日志被单个异常值撑爆。
const MAX_FIELD_VALUE_JSON_LEN: usize = 1024;
const MAX_STACK_LEN: usize = 2048;
/// `context_started_at` 的上限（2100-01-01），防止异常时钟值污染时间戳。
const MAX_CONTEXT_STARTED_AT_MS: f64 = 4_102_444_800_000.0;

/// 单条 metadata 连接上的上送预算；连接结束即随循环丢弃。
/// 事件按条计数、按原始消息字节计数，双上限任一耗尽后本连接不再接受上报。
#[derive(Debug)]
pub(crate) struct ClientDiagnosticsBudget {
    events_left: u32,
    bytes_left: u32,
    exhausted: bool,
}

impl Default for ClientDiagnosticsBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientDiagnosticsBudget {
    pub(crate) fn new() -> Self {
        // 50k 事件 / 64 MiB 大约对应一条长连接上数小时的连续上报，
        // 正常使用远到不了；到限只是停止收集，不影响连接本身。
        Self {
            events_left: 50_000,
            bytes_left: 64 * 1024 * 1024,
            exhausted: false,
        }
    }
}

/// 一条校验通过的诊断上报：脱敏后的事件列表与上下文。
struct SanitizedClientDiagnostics {
    context_id: String,
    events: Vec<Value>,
    message_bytes: u64,
}

/// 解析并校验一条 `client.diagnostics` 消息，产出最终会写进日志的脱敏事件。
/// 非本类型消息或校验失败返回 `None`（调用方继续按原逻辑处理，例如 `metadata.ping`）。
fn sanitize_client_diagnostics(raw: &str) -> Option<SanitizedClientDiagnostics> {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return None;
    };
    if value.get("type").and_then(Value::as_str) != Some(CLIENT_DIAGNOSTICS_TYPE) {
        return None;
    }
    let payload = value.get("payload")?.as_object()?;
    let context_id = payload.get("context_id")?.as_str()?;
    if !valid_context_id(context_id) {
        return None;
    }
    let context_started_at_ms = payload
        .get("context_started_at")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0 && *v <= MAX_CONTEXT_STARTED_AT_MS)
        .unwrap_or(0.0);
    let events = payload.get("events")?.as_array()?;
    if events.is_empty() || events.len() > MAX_EVENTS_PER_BATCH {
        return None;
    }

    let sanitized: Vec<Value> = events
        .iter()
        .filter_map(|event| sanitize_event(event, context_started_at_ms))
        .collect();
    if sanitized.is_empty() {
        return None;
    }
    Some(SanitizedClientDiagnostics {
        context_id: context_id.to_owned(),
        events: sanitized,
        message_bytes: raw.len() as u64,
    })
}

/// 接收一条 `client.diagnostics` 消息：校验、扣除单连接预算并写一条结构化
/// `tracing` 记录进入 daemon 日志（journalctl）。
///
/// 返回 `true` 表示消息被接受并写入日志；非本类型消息、校验失败或预算耗尽
/// 都返回 `false`。
pub(crate) fn handle_client_diagnostics_message(
    device_id: &DeviceId,
    raw: &str,
    budget: &mut ClientDiagnosticsBudget,
) -> bool {
    if budget.exhausted {
        return false;
    }
    let Some(batch) = sanitize_client_diagnostics(raw) else {
        return false;
    };
    if (batch.events.len() as u64) > u64::from(budget.events_left)
        || batch.message_bytes > u64::from(budget.bytes_left)
    {
        budget.exhausted = true;
        tracing::debug!(
            device_id = %device_id.0,
            context_id = %batch.context_id,
            "client diagnostics budget exhausted; dropping further uploads on this connection"
        );
        return false;
    }
    budget.events_left -= batch.events.len() as u32;
    budget.bytes_left -= batch.message_bytes as u32;

    tracing::info!(
        device_id = %device_id.0,
        context_id = %batch.context_id,
        event_count = batch.events.len(),
        bytes = batch.message_bytes,
        events = ?serde_json::Value::Array(batch.events),
        "client diagnostics batch reported"
    );
    true
}

fn valid_context_id(context_id: &str) -> bool {
    !context_id.is_empty()
        && context_id.len() <= MAX_CONTEXT_ID_LEN
        && context_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// 单事件脱敏：校验并截断各字段，输出带墙上时间戳的干净 JSON 对象。
/// 字段键做递归敏感词过滤（与前端 `protocol/errors.ts` 的敏感词集合一致），
/// 任何命中 `token|secret|private|signature|ciphertext|authorization|bearer|preview`
/// 的键（含嵌套对象/数组内）都不会进入最终日志。
fn sanitize_event(event: &Value, context_started_at_ms: f64) -> Option<Value> {
    let object = event.as_object()?;
    let name = object.get("name")?.as_str()?.trim();
    if name.is_empty() || name.chars().count() > MAX_EVENT_NAME_LEN {
        return None;
    }
    let t_ms = object
        .get("t")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(0.0);

    let mut clean = serde_json::Map::new();
    clean.insert("name".to_owned(), Value::String(name.to_owned()));
    clean.insert(
        "ts_ms".to_owned(),
        Value::from((context_started_at_ms + t_ms).round() as i64),
    );
    if let Some(fields) = object.get("fields").and_then(Value::as_object) {
        let mut safe_fields = serde_json::Map::new();
        for (key, field_value) in fields {
            if safe_fields.len() >= MAX_FIELD_KEYS_PER_EVENT {
                break;
            }
            let Some(sanitized) = sanitize_field_value(key, field_value) else {
                continue;
            };
            if serde_json::to_string(&sanitized)
                .map(|encoded| encoded.len() > MAX_FIELD_VALUE_JSON_LEN)
                .unwrap_or(true)
            {
                continue;
            }
            safe_fields.insert(key.clone(), sanitized);
        }
        if !safe_fields.is_empty() {
            clean.insert("fields".to_owned(), Value::Object(safe_fields));
        }
    }
    if let Some(stack) = object.get("stack").and_then(Value::as_str) {
        let stack = stack.trim();
        if !stack.is_empty() {
            let capped: String = stack.chars().take(MAX_STACK_LEN).collect();
            clean.insert("stack".to_owned(), Value::String(capped));
        }
    }
    Some(Value::Object(clean))
}

fn sensitive_key(key: &str) -> bool {
    const SENSITIVE_SUBSTRINGS: [&str; 8] = [
        "token",
        "secret",
        "private",
        "signature",
        "ciphertext",
        "authorization",
        "bearer",
        "preview",
    ];
    let lower = key.to_ascii_lowercase();
    SENSITIVE_SUBSTRINGS
        .iter()
        .any(|needle| lower.contains(needle))
}

/// 递归脱敏单个字段值：键命中敏感词则整支丢弃；对象/数组内容逐层过滤。
fn sanitize_field_value(key: &str, value: &Value) -> Option<Value> {
    if sensitive_key(key) {
        return None;
    }
    match value {
        Value::Object(map) => {
            let mut clean = serde_json::Map::new();
            for (nested_key, nested_value) in map {
                if let Some(sanitized) = sanitize_field_value(nested_key, nested_value) {
                    clean.insert(nested_key.clone(), sanitized);
                }
            }
            if clean.is_empty() {
                None
            } else {
                Some(Value::Object(clean))
            }
        }
        Value::Array(items) => {
            let clean: Vec<Value> = items
                .iter()
                .filter_map(|item| sanitize_field_value("", item))
                .collect();
            if clean.is_empty() {
                None
            } else {
                Some(Value::Array(clean))
            }
        }
        other => Some(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn device_id() -> DeviceId {
        DeviceId::new()
    }

    fn valid_batch(events: Value) -> String {
        json!({
            "type": "client.diagnostics",
            "payload": {
                "context_id": "page-m0ckabc-123456",
                "context_started_at": 1_750_000_000_000_f64,
                "events": events,
            },
        })
        .to_string()
    }

    #[test]
    fn accepts_valid_batch_with_wall_timestamps() {
        let mut budget = ClientDiagnosticsBudget::new();
        let raw = valid_batch(json!([
            { "t": 1234.5, "name": "terminal_writer_sequence_gap", "fields": { "seq": 9, "expected": 10 } },
        ]));
        assert!(handle_client_diagnostics_message(
            &device_id(),
            &raw,
            &mut budget
        ));
        assert_eq!(budget.events_left, 50_000 - 1);
        assert!(budget.bytes_left < 64 * 1024 * 1024);
    }

    #[test]
    fn ignores_non_diagnostics_messages() {
        let mut budget = ClientDiagnosticsBudget::new();
        let raw = json!({ "type": "metadata.ping", "payload": { "timestamp_ms": 1 } }).to_string();
        assert!(!handle_client_diagnostics_message(
            &device_id(),
            &raw,
            &mut budget
        ));
        assert_eq!(budget.events_left, 50_000);
    }

    #[test]
    fn rejects_invalid_payloads() {
        let mut budget = ClientDiagnosticsBudget::new();
        let cases: Vec<Value> = vec![
            // 缺 payload
            json!({ "type": "client.diagnostics" }),
            // 缺 context_id / 非法字符 / 路径穿越 / 超长
            json!({ "type": "client.diagnostics", "payload": { "context_id": 3, "events": [{}] } }),
            json!({ "type": "client.diagnostics", "payload": { "context_id": "../etc/passwd", "events": [{}] } }),
            json!({ "type": "client.diagnostics", "payload": { "context_id": "a b", "events": [{}] } }),
            json!({ "type": "client.diagnostics", "payload": { "context_id": "x".repeat(65), "events": [{}] } }),
            // events 缺失 / 空 / 超量
            json!({ "type": "client.diagnostics", "payload": { "context_id": "page-ok" } }),
            json!({ "type": "client.diagnostics", "payload": { "context_id": "page-ok", "events": [] } }),
            json!({ "type": "client.diagnostics", "payload": { "context_id": "page-ok", "events": (0..513).map(|i| json!({ "t": i, "name": format!("e{i}") })).collect::<Vec<_>>() } }),
        ];
        for case in cases {
            assert!(
                !handle_client_diagnostics_message(&device_id(), &case.to_string(), &mut budget),
                "should reject: {case}"
            );
        }
        assert_eq!(budget.events_left, 50_000);
    }

    #[test]
    fn drops_bad_events_but_keeps_good_ones() {
        let mut budget = ClientDiagnosticsBudget::new();
        let raw = valid_batch(json!([
            { "t": 1.0, "name": "good_event", "fields": { "a": 1 } },
            { "t": 2.0, "name": "" },
            { "t": 3.0, "name": "x".repeat(129) },
            { "t": 4.0, "name": "no_t" },
        ]));
        assert!(handle_client_diagnostics_message(
            &device_id(),
            &raw,
            &mut budget
        ));
        assert_eq!(budget.events_left, 50_000 - 2);
    }

    #[test]
    fn strips_preview_fields_and_caps_oversized_values_and_stack() {
        let mut budget = ClientDiagnosticsBudget::new();
        let huge_value = "h".repeat(5000);
        let huge_stack = "s".repeat(5000);
        let raw = json!({
            "type": "client.diagnostics",
            "payload": {
                "context_id": "page-m0ckabc-123456",
                "context_started_at": 0_f64,
                "events": [{
                    "t": 1.0,
                    "name": "sanitized",
                    "fields": {
                        "preview": "must-not-leak",
                        "PagePreviewRow": "also-must-not-leak",
                        "small": 42,
                        "huge": huge_value,
                    },
                    "stack": huge_stack,
                }],
            },
        });
        let raw = raw.to_string();
        assert!(handle_client_diagnostics_message(
            &device_id(),
            &raw,
            &mut budget
        ));
        let batch = sanitize_client_diagnostics(&raw).expect("valid batch");
        let fields = batch.events[0]
            .get("fields")
            .and_then(Value::as_object)
            .expect("fields");
        assert_eq!(fields.get("small"), Some(&json!(42)));
        assert!(!fields.contains_key("preview"));
        assert!(!fields.contains_key("PagePreviewRow"));
        assert!(
            !fields.contains_key("huge"),
            "oversized field value must be dropped"
        );
        let stack = batch.events[0]
            .get("stack")
            .and_then(Value::as_str)
            .expect("stack");
        assert_eq!(stack.len(), 2048, "stack must be capped");
    }

    #[test]
    fn recursively_strips_sensitive_keys_from_nested_fields() {
        let raw = json!({
            "type": "client.diagnostics",
            "payload": {
                "context_id": "page-m0ckabc-123456",
                "context_started_at": 1_750_000_000_000_f64,
                "events": [{
                    "t": 1234.0,
                    "name": "nested_fields",
                    "fields": {
                        "ok": 1,
                        "access_token": "must-not-leak",
                        "nested": { "inner_token": "must-not-leak", "keep": "value" },
                        "list": [{ "signature": "must-not-leak", "ok": 2 }, 3, { "bearer": "must-not-leak" }],
                    },
                }],
            },
        });
        let batch = sanitize_client_diagnostics(&raw.to_string()).expect("valid batch");
        assert_eq!(batch.events.len(), 1);
        let event = &batch.events[0];
        assert_eq!(event["ts_ms"], json!(1_750_000_001_234_i64));
        let fields = event
            .get("fields")
            .and_then(Value::as_object)
            .expect("fields");
        assert_eq!(fields.get("ok"), Some(&json!(1)));
        assert!(
            !fields.contains_key("access_token"),
            "top-level sensitive key must be dropped"
        );
        let nested = fields
            .get("nested")
            .and_then(Value::as_object)
            .expect("nested");
        assert_eq!(nested.get("keep"), Some(&json!("value")));
        assert!(
            !nested.contains_key("inner_token"),
            "nested sensitive key must be dropped"
        );
        let list = fields.get("list").and_then(Value::as_array).expect("list");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], json!({ "ok": 2 }));
        assert_eq!(list[1], json!(3));
    }

    #[test]
    fn sanitized_output_is_exactly_what_gets_logged() {
        let raw = json!({
            "type": "client.diagnostics",
            "payload": {
                "context_id": "page-abc-123",
                "context_started_at": 1_000_000_000_000_f64,
                "events": [
                    { "t": 100.0, "name": "terminal_writer_sequence_gap", "fields": { "expected": 4, "actual": 9 } },
                ],
            },
        });
        let batch = sanitize_client_diagnostics(&raw.to_string()).expect("valid batch");
        assert_eq!(
            batch.events,
            vec![json!({
                "name": "terminal_writer_sequence_gap",
                "ts_ms": 1_000_000_000_100_i64,
                "fields": { "expected": 4, "actual": 9 },
            })]
        );
        assert_eq!(batch.context_id, "page-abc-123");
    }

    #[test]
    fn budget_exhaustion_blocks_further_batches() {
        let mut budget = ClientDiagnosticsBudget::new();
        budget.events_left = 1;
        let raw = valid_batch(json!([{ "t": 1.0, "name": "e1" }]));
        assert!(handle_client_diagnostics_message(
            &device_id(),
            &raw,
            &mut budget
        ));
        assert!(!handle_client_diagnostics_message(
            &device_id(),
            &raw,
            &mut budget
        ));
        // 预算耗尽后即使重置也不会放行
        budget.events_left = 1_000;
        assert!(!handle_client_diagnostics_message(
            &device_id(),
            &raw,
            &mut budget
        ));
    }

    #[test]
    fn byte_budget_exhausts_on_large_messages() {
        let mut budget = ClientDiagnosticsBudget::new();
        budget.bytes_left = 10;
        let raw = valid_batch(json!([{ "t": 1.0, "name": "e1" }]));
        assert!(!handle_client_diagnostics_message(
            &device_id(),
            &raw,
            &mut budget
        ));
        assert!(budget.exhausted);
    }
}
