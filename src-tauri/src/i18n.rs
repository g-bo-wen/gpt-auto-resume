use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

fn catalog() -> &'static serde_json::Value {
    static CATALOG: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("../../src/locales/backend.json"))
            .expect("bundled language catalog must be valid JSON")
    })
}

/// A stable presentation identity. `fallback` is retained for old clients and
/// unknown Desktop responses; it is never used for control flow.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub code: String,
    #[serde(default)]
    pub params: BTreeMap<String, serde_json::Value>,
    pub fallback: String,
}

impl From<String> for Message {
    fn from(fallback: String) -> Self {
        message_from_text(&fallback)
    }
}
impl From<&str> for Message {
    fn from(fallback: &str) -> Self {
        message_from_text(fallback)
    }
}

pub fn message(code: &str, fallback: impl Into<String>) -> Message {
    Message {
        code: code.into(),
        params: BTreeMap::new(),
        fallback: fallback.into(),
    }
}

/// Map application-owned canonical text to an invariant key.  Desktop and
/// rollout text deliberately stays `backend.external` and is shown verbatim.
pub fn message_from_text(text: &str) -> Message {
    // Only new application-owned messages pass this boundary. Persisted legacy
    // rows use external_message directly and are never reverse-translated.
    if let Some(code) = catalog()["canonical"][text].as_str() {
        return message(code, text);
    }
    if let Some((base, suffix)) = text.rsplit_once(" [") {
        let child = message_from_text(base);
        if child.code != "backend.external" {
            let mut value = message("backend.withDetails", text);
            value
                .params
                .insert("message".into(), serde_json::to_value(child).unwrap());
            value
                .params
                .insert("detail".into(), format!("[{suffix}").into());
            return value;
        }
    }
    for (prefix, code) in [("SQLite: ", "backend.sqlite"), ("JSON: ", "backend.json")] {
        if let Some(detail) = text.strip_prefix(prefix) {
            let mut value = message(code, text);
            value.params.insert("detail".into(), detail.into());
            return value;
        }
    }
    for stage in ["discovery", "supervisor", "preflight", "readonly", "ready"] {
        if let Some(detail) = text.strip_prefix(&format!("{stage}: ")) {
            let mut value = message("history.diagnostic", text);
            value.params.insert(
                "stage".into(),
                serde_json::to_value(message(&format!("stage.{stage}"), stage)).unwrap(),
            );
            value.params.insert(
                "detail".into(),
                serde_json::to_value(message_from_text(detail)).unwrap(),
            );
            return value;
        }
    }
    let code = match text {
        "Paused by user" => "watch.pausedByUser",
        "Stopped by user" => "watch.stoppedByUser",
        "Enabled by user" => "watch.enabledByUser",
        "Restart requires explicit enable" => "watch.restartRequiresEnable",
        "Source interruption was invalidated" => "watch.sourceInvalidated",
        "Source interruption was already consumed; no retry" => "watch.sourceConsumed",
        "Submission recorded before IPC" => "watch.submissionRecorded",
        "Awaiting execution proof" => "watch.awaitingExecutionProof",
        "Waiting for 5h quota" => "watch.waitingQuota",
        "Eligible quota restored" => "watch.quotaRestored",
        "Weekly quota exhausted" => "watch.weeklyQuotaExhausted",
        "No current 5h interruption eligibility" => "watch.noEligibility",
        "Session unavailable" => "watch.sessionUnavailable",
        "Resume submission recorded" => "history.submissionPrepared",
        "Desktop 已接受请求，正在核对执行 turn；确认后扣次，不重发" => {
            "delivery.submitted"
        }
        "Submission accepted; awaiting execution proof" => "delivery.accepted",
        "Desktop input item and execution turn correlated" => "delivery.turnCorrelated",
        "已关联 Desktop 输入记录和执行 turn，正在确认记账" => {
            "history.turnIdentified"
        }
        "额度更新失败，等待重新核实；暂不发送" => "quota.retrying",
        "额度更新失败，等待重试；上次成功值仅供显示，暂不发送" => {
            "quota.retrying"
        }
        "Desktop 已连接" => "desktop.connected",
        "Watch 已删除；历史事件和 Attempt 记录保留" => "watch.deleted",
        "用户已暂停全部 Watch；不会中断已提交的 Codex 任务" => "watch.allPaused",
        "本次resume已执行；随后出现用户输入，后续自动发送已暂停" => {
            "watch.userInputAfterResume"
        }
        _ if text.starts_with("后台故障，已停止发送") => {
            return with_detail("backend.supervisorFault", "后台故障，已停止发送：", text)
        }
        _ if text.starts_with("Desktop compatibility is not confirmed:") => {
            return with_detail(
                "desktop.compatibilityUnconfirmed",
                "Desktop compatibility is not confirmed:",
                text,
            )
        }
        _ if text.starts_with("Desktop 提交未取得确定结果") => {
            return with_detail(
                "delivery.unknown",
                "Desktop 提交未取得确定结果；不会自动重发，请查看原会话；",
                text,
            )
        }
        _ if text.starts_with("发送前发现较新的会话事件") => "delivery.sourceChanged",
        _ if text.starts_with("提交前授权被撤销") => "delivery.authorizationRevoked",
        _ if text.starts_with("Desktop 版本不在已验证范围") => {
            "desktop.versionUnsupported"
        }
        _ if text.starts_with("Runtime 版本不在已验证范围") => {
            "desktop.runtimeUnsupported"
        }
        _ if text.starts_with("Desktop 接口未找到") => "desktop.endpointUnavailable",
        "Watch not found" => "watch.notFound",
        "Unsupported watch action" => "watch.actionUnsupported",
        "Resume eligibility is no longer valid" => "watch.eligibilityInvalid",
        "会话记录基线无法确认，未发送" => "watch.baselineUnknown",
        "No observation is available" => "observation.unavailable",
        "Selected session is unavailable" => "session.selectedUnavailable",
        "Observation is stale; no resume submission prepared" => "observation.stale",
        "threadId and prompt are required" => "config.required",
        "Prompt must be at most 64000 bytes" => "config.promptTooLong",
        "remaining must be continuous or 1..1000000" => "config.remainingInvalid",
        "Desktop 账号无法核实" => "desktop.accountUnknown",
        "适用 codex 额度桶或窗口无法确认" => "desktop.quotaUnknown",
        "没有可核实的 Desktop 本地会话" => "desktop.localSessionUnavailable",
        "Desktop 会话列表格式无法识别，已停止自动恢复" => {
            "desktop.listUnrecognized"
        }
        "接口未知" => "desktop.interfaceUnknown",
        "Runtime 版本读取失败" => "desktop.runtimeReadFailed",
        "Desktop 接口能力不完整" => "desktop.capabilityIncomplete",
        "Windows 只读发现失败" => "desktop.windowsDiscoveryFailed",
        "IPC 请求超过大小限制" => "desktop.frameTooLarge",
        "Desktop 协议帧大小不兼容" => "desktop.frameIncompatible",
        "Desktop 协议版本不兼容" => "desktop.protocolIncompatible",
        "首版仅支持 Windows Native" | "仅支持 Windows Native" => "platform.windowsOnly",
        "自启动设置失败，未保存设置" => "settings.autoStartFailed",
        "检查间隔应为 15–3600 秒，默认消息不能为空且不超过 64KB" => {
            "settings.invalid"
        }
        _ if text
            .starts_with("This watch is ready to send immediately. Confirm prompt preview:") =>
        {
            return with_detail(
                "watch.immediateConfirmation",
                "This watch is ready to send immediately. Confirm prompt preview:",
                text,
            )
        }
        _ if text.starts_with("Completed watch has no remaining resumes") => "watch.noRemaining",
        _ if text.starts_with("Desktop 尚未连接") => {
            return with_detail("desktop.notConnected", "Desktop 尚未连接", text)
        }
        _ if text.starts_with("Desktop 工具调用被拒绝或协议不兼容") => {
            return with_detail(
                "desktop.callRejected",
                "Desktop 工具调用被拒绝或协议不兼容",
                text,
            )
        }
        _ if text.starts_with("Desktop 工具未确认成功") => {
            return with_detail("desktop.callUnconfirmed", "Desktop 工具未确认成功", text)
        }
        _ if text.starts_with("Desktop 响应结构无法识别") => {
            return with_detail(
                "desktop.responseUnrecognized",
                "Desktop 响应结构无法识别",
                text,
            )
        }
        _ if text.starts_with("Desktop 响应内容无法解析") => {
            return with_detail(
                "desktop.contentUnrecognized",
                "Desktop 响应内容无法解析",
                text,
            )
        }
        _ => "backend.external",
    };
    if code == "backend.external" {
        external_message(text)
    } else {
        message(code, text)
    }
}

pub fn external_message(text: &str) -> Message {
    let mut value = message("backend.external", text);
    value.params.insert("fallback".into(), text.into());
    value
}

/// Presentation fallback is audit data, except at an explicit external leaf.
/// Rewording a translation or known fallback must not create a new event.
pub fn identity(value: &Message) -> String {
    fn stable(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(mut fields) => {
                if fields.contains_key("code") && fields.contains_key("params") {
                    fields.remove("fallback");
                }
                serde_json::Value::Object(fields.into_iter().map(|(k, v)| (k, stable(v))).collect())
            }
            serde_json::Value::Array(values) => values.into_iter().map(stable).collect(),
            other => other,
        }
    }
    stable(serde_json::to_value(value).unwrap()).to_string()
}

fn with_detail(code: &str, prefix: &str, text: &str) -> Message {
    let mut value = message(code, text);
    let detail = text.strip_prefix(prefix).unwrap_or(text).trim().to_owned();
    if !detail.is_empty() {
        // Nested reasons retain their own stable identity when known; external
        // details remain an explicit fallback leaf and cannot affect gating.
        let nested = if detail == text || code == "watch.immediateConfirmation" {
            serde_json::Value::String(detail)
        } else {
            serde_json::to_value(message_from_text(&detail))
                .unwrap_or(serde_json::Value::String(detail))
        };
        value.params.insert("detail".into(), nested);
    }
    value
}

pub fn resolved_language(preference: &str) -> String {
    match preference {
        "en" => "en".into(),
        "zh-CN" => "zh-CN".into(),
        "system" => {
            let locale = system_ui_locale();
            if locale.to_ascii_lowercase().starts_with("en") {
                "en".into()
            } else {
                "zh-CN".into()
            }
        }
        _ => "zh-CN".into(),
    }
}

pub fn normalize_preference(value: &str) -> &'static str {
    match value {
        "system" => "system",
        "en" => "en",
        _ => "zh-CN",
    }
}

pub fn localized(key: &str, language: &str) -> String {
    let language = resolved_language(language);
    catalog()[language.as_str()][key]
        .as_str()
        .unwrap_or(key)
        .to_owned()
}

pub fn render(message: &Message, language: &str) -> String {
    render_inner(message, language, 0)
}
fn render_inner(message: &Message, language: &str, depth: u8) -> String {
    if depth >= 8 {
        return message.fallback.clone();
    }
    let template = localized(&message.code, language);
    if template == message.code {
        return message.fallback.clone();
    }
    let mut remaining = template.as_str();
    let mut text = String::new();
    while let Some((head, tail)) = remaining.split_once("{{") {
        text.push_str(head);
        let Some((key, rest)) = tail.split_once("}}") else {
            return message.fallback.clone();
        };
        let Some(value) = message.params.get(key) else {
            return message.fallback.clone();
        };
        let rendered = if let Ok(child) = serde_json::from_value::<Message>(value.clone()) {
            render_inner(&child, language, depth + 1)
        } else if let Some(value) = value.as_str() {
            value.to_owned()
        } else {
            value.to_string()
        };
        text.push_str(&rendered);
        remaining = rest;
    }
    text.push_str(remaining);
    text
}

#[cfg(windows)]
fn system_ui_locale() -> String {
    // UI language is independent of the user's regional date/number settings.
    // Use the Windows UI LANGID, not GetUserDefaultLocaleName (regional locale).
    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
        fn LCIDToLocaleName(locale: u32, name: *mut u16, length: i32, flags: u32) -> i32;
    }
    let mut name = [0u16; 85];
    let count = unsafe {
        LCIDToLocaleName(
            GetUserDefaultUILanguage() as u32,
            name.as_mut_ptr(),
            name.len() as i32,
            0,
        )
    };
    if count > 1 {
        String::from_utf16_lossy(&name[..count as usize - 1])
    } else {
        String::new()
    }
}
#[cfg(not(windows))]
fn system_ui_locale() -> String {
    std::env::var("LANG")
        .or_else(|_| std::env::var("LC_ALL"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_reason_has_language_independent_identity() {
        let message = message_from_text("Paused by user");
        assert_eq!(message.code, "watch.pausedByUser");
        assert_eq!(localized(&message.code, "zh-CN"), "已由用户暂停");
        assert_eq!(localized(&message.code, "en"), "Paused by user");
    }
    #[test]
    fn external_reason_keeps_fallback_in_identity() {
        let first = message_from_text("Desktop extension detail A");
        let second = message_from_text("Desktop extension detail B");
        assert_eq!(first.code, "backend.external");
        assert_ne!(first.params, second.params);
    }
    #[test]
    fn nested_identity_ignores_known_wording_and_preserves_external_details() {
        let mut first = message_from_text("后台故障，已停止发送：Desktop 管道无法连接");
        let original = identity(&first);
        first.fallback = "New wording".into();
        first.params.get_mut("detail").unwrap()["fallback"] = "Different known wording".into();
        assert_eq!(identity(&first), original);
        assert!(render(&first, "en").contains("Cannot connect to the Desktop pipe"));
        assert_ne!(
            identity(&external_message("A")),
            identity(&external_message("B"))
        );
    }
    #[test]
    fn prompt_preview_stays_verbatim_even_when_it_matches_a_known_reason() {
        let value = message_from_text(
            "This watch is ready to send immediately. Confirm prompt preview: Paused by user",
        );
        assert!(render(&value, "zh-CN").ends_with("Paused by user"));
        let mut value = message("backend.withDetails", "fallback");
        value.params.insert("message".into(), "{{detail}}".into());
        value.params.insert("detail".into(), "unchanged".into());
        assert!(render(&value, "en").contains("{{detail}}"));
    }
    #[test]
    fn bundled_canonical_messages_have_both_languages_and_complete_parameters() {
        for (raw, code) in catalog()["canonical"].as_object().unwrap() {
            let code = code.as_str().unwrap();
            for lang in ["en", "zh-CN"] {
                assert!(catalog()[lang][code].is_string(), "{code}");
                let rendered = render(&message_from_text(raw), lang);
                assert!(!rendered.contains("{{"), "{code}: {rendered}");
                assert!(!rendered.is_empty());
            }
        }
    }
}
