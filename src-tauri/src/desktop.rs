//! Windows Desktop private IPC adapter. No CLI writer or keyboard fallback.
use crate::contracts::{Delivery, Diagnostic, Quota, Submission};
use crate::i18n::message_from_text;
use chrono::Utc;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    process::{Command, Stdio},
    time::Duration,
};
use uuid::Uuid;

const MAX_FRAME: usize = 8 * 1024 * 1024;
type Capabilities = HashMap<String, jsonschema::Validator>;
pub fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
pub fn now() -> String {
    Utc::now().to_rfc3339()
}

#[derive(Default)]
pub struct Desktop {
    pipe: Option<String>,
    version: Option<String>,
    runtime: Option<String>,
    capabilities: Capabilities,
    last_failure: std::sync::Mutex<Option<Value>>,
}
impl Desktop {
    pub fn quota_temporarily_unavailable(&self) -> bool {
        self.last_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(is_temporary_quota_failure)
    }
    pub fn read_temporarily_unavailable(&self) -> bool {
        self.last_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|failure| {
                is_temporary_quota_failure(failure)
                    || (failure["tool"] == "list_threads"
                        && failure["category"] == "read_retryable")
            })
    }
    pub fn take_failure(&self) -> Option<Value> {
        self.last_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn record_failure(
        &self,
        tool: &str,
        context: &str,
        call_id: &str,
        elapsed_ms: u128,
        reason: &str,
        response: Option<&Value>,
        transport_code: Option<&str>,
    ) {
        let payload = response.map(|response| {
            let texts = response["result"]["contentItems"].as_array().into_iter().flatten()
                .filter(|item| item["type"] == "inputText")
                .filter_map(|item| item["text"].as_str()).take(16)
                .flat_map(|text| text.chars().chain(std::iter::once('\n')))
                .take(16384).collect::<String>();
            json!({"rpcError":response.get("error").map(|v|v.to_string().chars().take(8192).collect::<String>()),
                "success":response["result"]["success"],"failureText":texts,
                "textLimit":16384,"responseId":response["id"],
                "resultKeys":response["result"].as_object().map(|v|v.keys().take(32).collect::<Vec<_>>())})
        });
        let category = failure_category(tool, transport_code, payload.as_ref());
        *self.last_failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(json!({
            "tool":diagnostic_tool_name(tool),"callerThreadId":context,"callId":call_id,
            "elapsedMs":elapsed_ms.min(u64::MAX as u128) as u64,"reason":reason,
            "reasonCode":transport_code.map(str::to_owned).unwrap_or_else(|| message_from_text(reason).code),"category":category,"response":payload
        }));
    }

    pub fn diagnostic(&self, compatible: bool, reason: String, stage: &str) -> Diagnostic {
        Diagnostic {
            compatible,
            reason_message: Some(message_from_text(&reason)),
            reason,
            stage: stage.into(),
            checked_at: now(),
            desktop_version: self.version.clone(),
            runtime_version: self.runtime.clone(),
        }
    }
    pub fn connect(&mut self, runtime_path: &str) -> Result<(), String> {
        if !cfg!(windows) {
            self.pipe = None;
            return Err("首版仅支持 Windows Native".into());
        }
        // Versions are diagnostic only: this adapter communicates through Desktop IPC,
        // not through the CLI runtime. Missing metadata cannot determine compatibility.
        let mut version_command = hidden_command("pwsh.exe");
        version_command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-AppxPackage -Name OpenAI.Codex -ErrorAction Stop).Version.ToString()",
        ]);
        self.version = diagnostic_output(version_command, Duration::from_secs(5))
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        self.runtime = runtime_version(runtime_path);
        if let Some(path) = &self.pipe {
            match catalog(path) {
                Ok(capabilities) => {
                    self.capabilities = capabilities;
                    return Ok(());
                }
                Err(_) => {
                    self.pipe = None;
                    self.capabilities.clear();
                }
            }
        }
        let listing = run_ps("[IO.Directory]::GetFiles('\\\\.\\pipe\\') | Where-Object { [IO.Path]::GetFileName($_) -like 'codex-browser-use-*' } | ConvertTo-Json -Compress")?;
        let value: Value =
            serde_json::from_str(listing.trim()).map_err(|_| "Desktop 管道发现失败")?;
        let paths: Vec<String> = match value {
            Value::String(s) => vec![s],
            Value::Array(v) => v
                .into_iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect(),
            _ => vec![],
        };
        let mut matches = vec![];
        let mut capability_failure = None;
        for path in paths.into_iter().take(64) {
            match catalog(&path) {
                Ok(capabilities) => matches.push((path, capabilities)),
                Err(reason) if is_capability_failure(&reason) => {
                    capability_failure = Some(reason);
                }
                Err(_) => {}
            }
        }
        if matches.len() != 1 {
            if matches.is_empty() {
                if let Some(reason) = capability_failure {
                    return Err(reason);
                }
            }
            return Err("Desktop 接口未找到或存在多个匹配端点，已停止自动恢复".into());
        }
        let (path, capabilities) = matches.pop().unwrap();
        self.pipe = Some(path);
        self.capabilities = capabilities;
        Ok(())
    }
    fn validate_arguments(&self, tool: &str, arguments: &Value) -> Result<(), String> {
        validate_arguments(&self.capabilities, tool, arguments)
    }
    pub fn check_send(&self, thread: &str, prompt: &str) -> Result<(), String> {
        self.validate_arguments("send_message_to_thread", &send_arguments(thread, prompt))
    }
    pub fn tool(&self, context: &str, tool: &str, arguments: Value) -> Result<Value, String> {
        // Validate actual arguments (including the unchanged user prompt) before any IPC write.
        self.validate_arguments(tool, &arguments)?;
        let label = diagnostic_tool_name(tool);
        let pipe = self
            .pipe
            .as_ref()
            .ok_or_else(|| format!("Desktop 尚未连接 [tool={label}]"))?;
        let id = Uuid::new_v4().to_string();
        let started = std::time::Instant::now();
        let response = rpc_transport(
            pipe,
            "tools/call",
            json!({"namespace":"codex_app","tool":tool,"arguments":arguments,
            "callerSource":"codex","threadId":context,"callId":format!("auto-resume-{id}"),"turnId":format!("mcp-turn-auto-resume-{id}")}),
            Duration::from_secs(10),
        ).map_err(|failure| {
            let reason=format!("{} [tool={label}; category=transport]", failure.reason);
            self.record_failure(tool,context,&format!("auto-resume-{id}"),started.elapsed().as_millis(),&reason,None,Some(failure.code));
            reason
        })?;
        let parsed = (|| -> Result<Value, String> {
            if response.get("error").is_some() {
                return Err(format!(
                    "Desktop 工具调用被拒绝或协议不兼容 {}",
                    failure_diagnostic(&response, label)
                ));
            }
            let result = &response["result"];
            if result["success"] != true {
                return Err(format!(
                    "Desktop 工具未确认成功 {}",
                    failure_diagnostic(&response, label)
                ));
            }
            let items = result["contentItems"].as_array().ok_or_else(|| {
                format!("Desktop 响应结构无法识别 [tool={label}; category=content_items]")
            })?;
            let text = items
                .iter()
                .filter(|v| v["type"] == "inputText")
                .filter_map(|v| v["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            serde_json::from_str(&text).map_err(|_| {
                format!("Desktop 响应内容无法解析 [tool={label}; category=content_json]")
            })
        })();
        if let Err(reason) = &parsed {
            self.record_failure(
                tool,
                context,
                &format!("auto-resume-{id}"),
                started.elapsed().as_millis(),
                reason,
                Some(&response),
                None,
            );
        }
        parsed
    }
    pub fn quota(&self, context: &str) -> Result<Quota, String> {
        let value = self.tool(context, "get_usage_limits", json!({}))?;
        let account = value["accountId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("Desktop 账号无法核实")?;
        parse_quota(&value, &hash(account), &now()).ok_or("适用 codex 额度桶或窗口无法确认".into())
    }
    pub fn list(&self, context: &str) -> Result<Value, String> {
        self.tool(context, "list_threads", json!({"limit":50}))
    }
    pub fn check_thread(&self, thread: &str) -> Result<(), String> {
        let value = self.tool(thread,"read_thread",json!({"threadId":thread,"hostId":"local","turnLimit":1,"includeOutputs":false,"maxOutputCharsPerItem":0}))?;
        check_thread_response(&value, thread)
    }
    pub fn send(
        &self,
        submission: &Submission,
        home: &std::path::Path,
        authorized: impl Fn() -> bool,
    ) -> Delivery {
        if let Err(reason) = self.check_send(&submission.thread_id, &submission.prompt) {
            return Delivery::Rejected(reason);
        }
        // Invocation is exclusively a supervisor effect, never a UI test button.
        let before = crate::sessions::discover(home)
            .into_iter()
            .find(|(s, _)| s.thread_id == submission.thread_id)
            .map(|(s, _)| s);
        if !before.as_ref().is_some_and(|s| {
            s.latest
                .as_ref()
                .is_some_and(|e| e.event_id == submission.source_event_id)
                && s.observed_order == submission.baseline_order
        }) {
            return Delivery::Rejected("发送前发现较新的会话事件或无法核实原中断，未发送".into());
        }
        if !authorized() {
            return Delivery::Rejected("提交前授权被撤销，未发送".into());
        }
        let response = self.tool(
            &submission.thread_id,
            "send_message_to_thread",
            send_arguments(&submission.thread_id, &submission.prompt),
        );
        match response {
            Ok(value) => classify_delivery(&value, &submission.thread_id),
            Err(reason) => Delivery::Unknown(format!(
                "Desktop 提交未取得确定结果；不会自动重发，请查看原会话；{reason}"
            )),
        }
    }
}

fn is_temporary_quota_failure(failure: &Value) -> bool {
    // Only the verified Desktop handler's retryable failure is allowlisted.
    // Authentication, invalid schemas and RPC rejection remain hard failures.
    failure["tool"] == "get_usage_limits" && failure["category"] == "quota_retryable"
}

fn failure_category(tool: &str, code: Option<&str>, response: Option<&Value>) -> &'static str {
    if tool != "get_usage_limits" && tool != "list_threads" {
        return "hard_failure";
    }
    if response.is_none()
        && matches!(
            code,
            Some(
                "transport.timeout"
                    | "transport.disconnected"
                    | "transport.pipeUnavailable"
                    | "transport.writeFailed"
                    | "transport.incomplete"
            )
        )
    {
        return if tool == "list_threads" {
            "read_retryable"
        } else {
            "quota_retryable"
        };
    }
    if tool != "get_usage_limits" {
        return "hard_failure";
    }
    // This exact third-party response remains the verified protocol allowlist.
    if response.is_some_and(|payload| {
        payload["rpcError"].is_null()
            && payload["success"] == false
            && payload["failureText"].as_str().is_some_and(|text| {
                text.trim() == "Could not read current usage limits. Try again later."
            })
    }) {
        "quota_retryable"
    } else {
        "hard_failure"
    }
}
fn diagnostic_tool_name(tool: &str) -> &'static str {
    match tool {
        "get_usage_limits" => "get_usage_limits",
        "list_threads" => "list_threads",
        "read_thread" => "read_thread",
        "send_message_to_thread" => "send_message_to_thread",
        _ => "other",
    }
}

// Never log returned text or arbitrary error codes: Desktop can include user
// content and identity in an error. Emit only allowlisted categories and numbers.
fn failure_diagnostic(response: &Value, tool: &str) -> String {
    let result = &response["result"];
    let success = match result.get("success") {
        Some(Value::Bool(true)) => "true",
        Some(Value::Bool(false)) => "false",
        None => "missing",
        _ => "invalid",
    };
    let code = response["error"]["code"]
        .as_i64()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "none".into());
    let mut category = "unclassified";
    let texts = response["error"]["message"].as_str().into_iter().chain(
        result["contentItems"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "inputText")
            .filter_map(|item| item["text"].as_str()),
    );
    for text in texts.take(16) {
        let text: String = text.chars().take(4096).collect::<String>().to_lowercase();
        let found = if text.contains("unauthorized")
            || text.contains("not authorized")
            || text.contains("permission denied")
        {
            "authorization"
        } else if text.contains("thread not found")
            || text.contains("caller thread")
            || text.contains("thread unavailable")
        {
            "thread_context"
        } else if text.contains("timeout") || text.contains("timed out") {
            "timeout"
        } else if text.contains("usage limit") || text.contains("rate limit") {
            "quota"
        } else if text.contains("invalid app tool request") || text.contains("invalid argument") {
            "arguments"
        } else if text.contains("app-server") || text.contains("app server") {
            "app_server"
        } else {
            "unclassified"
        };
        if found != "unclassified" {
            category = found;
            break;
        }
    }
    let items = result["contentItems"]
        .as_array()
        .map(|v| v.len().to_string())
        .unwrap_or_else(|| "missing".into());
    format!(
        "[tool={}; success={success}; rpcCode={code}; category={category}; items={items}]",
        diagnostic_tool_name(tool)
    )
}

fn classify_delivery(value: &Value, target: &str) -> Delivery {
    if value["threadId"] != target {
        return Delivery::Unknown("Desktop 返回会话不匹配，结果未知；不重发".into());
    }
    match value["turnId"]
        .as_str()
        .filter(|s| Uuid::parse_str(s).is_ok())
    {
        Some(turn) => Delivery::Accepted {
            turn_id: turn.into(),
        },
        None if value.get("turnId").is_none() => Delivery::Submitted,
        None => Delivery::Unknown("Desktop 返回的 turn 身份无法识别；不扣次数、不重发".into()),
    }
}
pub fn parse_quota(value: &Value, scope: &str, captured: &str) -> Option<Quota> {
    let b = value
        .get("rateLimitsByLimitId")
        .and_then(|m| m.get("codex"))
        .or_else(|| value.get("rateLimits").filter(|b| b["limitId"] == "codex"))
        .or_else(|| (value["limit_id"] == "codex").then_some(value))?;
    let p = &b["primary"];
    let w = &b["secondary"];
    let pmins = p["windowDurationMins"]
        .as_u64()
        .or_else(|| p["window_minutes"].as_u64())?;
    let wmins = w["windowDurationMins"]
        .as_u64()
        .or_else(|| w["window_minutes"].as_u64())?;
    if pmins != 300 || wmins != 10080 || scope.is_empty() {
        return None;
    }
    let five = p["usedPercent"]
        .as_f64()
        .or_else(|| p["used_percent"].as_f64())?;
    let weekly = w["usedPercent"]
        .as_f64()
        .or_else(|| w["used_percent"].as_f64())?;
    if !five.is_finite()
        || !weekly.is_finite()
        || !(0.0..=100.0).contains(&five)
        || !(0.0..=100.0).contains(&weekly)
    {
        return None;
    }
    Some(Quota {
        scope: scope.into(),
        bucket: "codex".into(),
        captured_at: captured.into(),
        five_hour_used: five,
        weekly_used: weekly,
        reset_at: p["resetsAt"].as_i64().or_else(|| p["resets_at"].as_i64()),
    })
}
fn send_arguments(thread: &str, prompt: &str) -> Value {
    json!({"threadId":thread,"hostId":"local","prompt":prompt})
}

fn check_thread_response(value: &Value, target: &str) -> Result<(), String> {
    let thread = &value["thread"];
    let id = thread["id"]
        .as_str()
        .or_else(|| thread["threadId"].as_str());
    if id != Some(target) || !value["turns"].is_array() {
        return Err("Desktop 会话读取响应不兼容，已停止自动恢复".into());
    }
    if thread["hostId"].as_str() != Some("local") {
        return Err("Desktop 会话读取响应不兼容，已停止自动恢复".into());
    }
    Ok(())
}

fn validate_arguments(
    capabilities: &Capabilities,
    tool: &str,
    arguments: &Value,
) -> Result<(), String> {
    let label = diagnostic_tool_name(tool);
    let validator = capabilities
        .get(tool)
        .ok_or_else(|| format!("Desktop 接口能力不完整 [tool={label}]"))?;
    // Do not expose validator errors: they can contain the entire resume prompt.
    if !validator.is_valid(arguments) {
        return Err(format!(
            "Desktop 工具参数不兼容，已停止自动恢复 [tool={label}]"
        ));
    }
    Ok(())
}

fn is_capability_failure(reason: &str) -> bool {
    let message = message_from_text(reason);
    let code = if message.code == "backend.withDetails" {
        message
            .params
            .get("message")
            .and_then(|v| v["code"].as_str())
            .unwrap_or("")
    } else {
        &message.code
    };
    matches!(
        code,
        "desktop.capabilityIncomplete" | "desktop.schemaUnrecognized"
    )
}

fn parse_capabilities(value: &Value) -> Result<Capabilities, String> {
    if value.get("error").is_some() {
        return Err("Desktop 接口能力不完整".into());
    }
    let tools = value["result"]["tools"]
        .as_array()
        .ok_or("Desktop 接口能力不完整")?;
    let mut capabilities = HashMap::new();
    for name in [
        "send_message_to_thread",
        "list_threads",
        "read_thread",
        "get_usage_limits",
    ] {
        let mut matching = tools
            .iter()
            .filter(|t| t["namespace"] == "codex_app" && t["name"] == name);
        let tool = matching
            .next()
            .ok_or_else(|| format!("Desktop 接口能力不完整 [tool={name}]"))?;
        if matching.next().is_some() {
            return Err(format!("Desktop 接口能力不完整 [tool={name}]"));
        }
        let schema = tool
            .get("inputSchema")
            .filter(|s| s.is_object())
            .ok_or_else(|| format!("Desktop 工具参数协议无法识别，已停止自动恢复 [tool={name}]"))?;
        // default-features=false disables HTTP/file reference retrieval. Local $defs
        // and combinators remain supported by the standard JSON Schema validator.
        let validator = jsonschema::options()
            .should_validate_formats(true)
            .should_ignore_unknown_formats(false)
            .build(schema)
            .map_err(|_| format!("Desktop 工具参数协议无法识别，已停止自动恢复 [tool={name}]"))?;
        capabilities.insert(name.to_owned(), validator);
    }
    Ok(capabilities)
}

fn catalog(path: &str) -> Result<Capabilities, String> {
    let v = rpc(
        path,
        "tools/list",
        json!({"threadStartKind":"all"}),
        Duration::from_millis(600),
    )?;
    parse_capabilities(&v)
}

fn runtime_version(runtime_path: &str) -> Option<String> {
    let runtime = if runtime_path.is_empty() {
        let base = std::env::var("LOCALAPPDATA").ok()?;
        let mut paths: Vec<_> =
            walkdir::WalkDir::new(std::path::Path::new(&base).join("OpenAI/Codex/bin"))
                .max_depth(3)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_file() && e.file_name() == "codex.exe")
                .map(|e| e.into_path())
                .collect();
        paths.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        paths.pop()?
    } else {
        std::path::PathBuf::from(runtime_path)
    };
    // This is metadata only; never start an App Server or CLI writer.
    let mut command = hidden_command(runtime);
    command.arg("--version");
    let output = diagnostic_output(command, Duration::from_secs(2))?;
    let version = output.trim().trim_start_matches("codex-cli ").to_owned();
    (!version.is_empty()).then_some(version)
}

fn diagnostic_output(command: Command, timeout: Duration) -> Option<String> {
    use tokio::io::AsyncReadExt;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        let mut command = tokio::process::Command::from(command);
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let result = tokio::time::timeout(timeout, async {
            let mut bytes = Vec::new();
            child
                .stdout
                .take()?
                .take(4097)
                .read_to_end(&mut bytes)
                .await
                .ok()?;
            if bytes.len() > 4096 {
                return None;
            }
            if !child.wait().await.ok()?.success() {
                return None;
            }
            String::from_utf8(bytes).ok()
        })
        .await
        .ok()
        .flatten();
        if result.is_none() {
            // Reap the metadata child when possible; drop still requests a kill
            // if cleanup itself cannot finish within the bound.
            let _ = tokio::time::timeout(Duration::from_secs(1), child.kill()).await;
        }
        result
    })
}
fn hidden_command(exe: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut c = Command::new(exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    c.stdin(Stdio::null()).stderr(Stdio::null());
    c
}
fn run_ps(command: &str) -> Result<String, String> {
    let output = hidden_command("pwsh.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            command,
        ])
        .output()
        .map_err(|_| "PowerShell 7 不可用")?;
    if !output.status.success() {
        return Err("Windows 只读发现失败".into());
    }
    String::from_utf8(output.stdout).map_err(|_| "发现结果编码无法识别".into())
}
#[derive(Debug)]
struct RpcFailure {
    code: &'static str,
    reason: &'static str,
}
impl RpcFailure {
    fn new(code: &'static str, reason: &'static str) -> Self {
        Self { code, reason }
    }
}
fn rpc(path: &str, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
    rpc_transport(path, method, params, timeout).map_err(|failure| failure.reason.to_owned())
}
fn rpc_transport(
    path: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, RpcFailure> {
    #[cfg(windows)]
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| RpcFailure::new("transport.runtimeFailed", "IPC Runtime 初始化失败"))?;
        rt.block_on(async {
            tokio::time::timeout(timeout, async {
                let mut client = tokio::net::windows::named_pipe::ClientOptions::new()
                    .open(path)
                    .map_err(|_| {
                        RpcFailure::new("transport.pipeUnavailable", "Desktop 管道无法连接")
                    })?;
                let id = Uuid::new_v4().to_string();
                let body = serde_json::to_vec(
                    &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
                )
                .map_err(|_| RpcFailure::new("transport.encodingFailed", "IPC 编码失败"))?;
                if body.len() > MAX_FRAME {
                    return Err(RpcFailure::new(
                        "desktop.frameTooLarge",
                        "IPC 请求超过大小限制",
                    ));
                }
                client
                    .write_all(&(body.len() as u32).to_le_bytes())
                    .await
                    .map_err(|_| RpcFailure::new("transport.writeFailed", "Desktop 写入失败"))?;
                client
                    .write_all(&body)
                    .await
                    .map_err(|_| RpcFailure::new("transport.writeFailed", "Desktop 写入失败"))?;
                loop {
                    let mut head = [0u8; 4];
                    client.read_exact(&mut head).await.map_err(|_| {
                        RpcFailure::new("transport.disconnected", "Desktop 连接中断")
                    })?;
                    let size = u32::from_le_bytes(head) as usize;
                    if size == 0 || size > MAX_FRAME {
                        return Err(RpcFailure::new(
                            "desktop.frameIncompatible",
                            "Desktop 协议帧大小不兼容",
                        ));
                    }
                    let mut bytes = vec![0; size];
                    client.read_exact(&mut bytes).await.map_err(|_| {
                        RpcFailure::new("transport.incomplete", "Desktop 响应不完整")
                    })?;
                    let v: Value = serde_json::from_slice(&bytes).map_err(|_| {
                        RpcFailure::new("transport.contentIncompatible", "Desktop 协议内容不兼容")
                    })?;
                    if v["id"] == id {
                        if v["jsonrpc"] != "2.0" {
                            return Err(RpcFailure::new(
                                "desktop.protocolIncompatible",
                                "Desktop 协议版本不兼容",
                            ));
                        }
                        return Ok(v);
                    }
                }
            })
            .await
            .map_err(|_| RpcFailure::new("transport.timeout", "Desktop 请求超时，停止自动恢复"))?
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (path, method, params, timeout);
        Err(RpcFailure::new(
            "platform.windowsOnly",
            "首版仅支持 Windows Native",
        ))
    }
}
#[cfg(test)]
mod tests {
    fn capability_fixture() -> Value {
        let mut tools = vec![];
        for (name, schema) in [
            (
                "get_usage_limits",
                json!({"type":"object","properties":{},"additionalProperties":false}),
            ),
            (
                "list_threads",
                json!({"type":"object","properties":{"limit":{"type":"integer","minimum":1,"maximum":50}},"additionalProperties":false}),
            ),
            (
                "read_thread",
                json!({"type":"object","properties":{"threadId":{"type":"string"},"hostId":{"type":"string"},"turnLimit":{"type":"integer"},"includeOutputs":{"type":"boolean"},"maxOutputCharsPerItem":{"type":"integer"}},"required":["threadId"],"additionalProperties":false}),
            ),
            (
                "send_message_to_thread",
                json!({"type":"object","properties":{"threadId":{"type":"string"},"hostId":{"enum":["local"]},"prompt":{"type":"string","minLength":1}},"required":["threadId","prompt"],"additionalProperties":false}),
            ),
        ] {
            tools.push(json!({"namespace":"codex_app","name":name,"inputSchema":schema}));
        }
        json!({"result":{"tools":tools}})
    }

    #[test]
    fn capabilities_accept_actual_calls_without_version_metadata() {
        let desktop = Desktop {
            capabilities: parse_capabilities(&capability_fixture()).unwrap(),
            ..Default::default()
        };
        assert!(desktop.version.is_none() && desktop.runtime.is_none());
        assert!(desktop.check_send("thread", "原文 resume").is_ok());
        assert!(desktop
            .validate_arguments("get_usage_limits", &json!({}))
            .is_ok());
        assert!(desktop
            .validate_arguments("list_threads", &json!({"limit":50}))
            .is_ok());
        assert!(desktop.validate_arguments("read_thread", &json!({"threadId":"thread","hostId":"local","turnLimit":1,"includeOutputs":false,"maxOutputCharsPerItem":0})).is_ok());
        assert!(runtime_version("this-runtime-does-not-exist.exe").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn diagnostic_metadata_is_bounded_and_failure_is_optional() {
        fn command(script: &str) -> Command {
            let mut command = hidden_command("pwsh.exe");
            command.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ]);
            command
        }
        assert_eq!(
            diagnostic_output(
                command("[Console]::Write('codex-cli fixture')"),
                Duration::from_secs(10)
            )
            .as_deref(),
            Some("codex-cli fixture")
        );
        assert!(diagnostic_output(
            command("[Console]::Write('x' * 4097)"),
            Duration::from_secs(10)
        )
        .is_none());
        assert!(diagnostic_output(command("exit 1"), Duration::from_secs(10)).is_none());
        let started = std::time::Instant::now();
        assert!(diagnostic_output(
            command("[Threading.Thread]::Sleep(30000)"),
            Duration::from_millis(100)
        )
        .is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn missing_duplicate_wrong_namespace_and_unreadable_schemas_block() {
        for variant in 0..6 {
            let mut value = capability_fixture();
            let tools = value["result"]["tools"].as_array_mut().unwrap();
            match variant {
                0 => {
                    tools.pop();
                }
                1 => {
                    tools.push(tools[3].clone());
                }
                2 => {
                    tools[3]["namespace"] = json!("other");
                }
                3 => {
                    tools[3].as_object_mut().unwrap().remove("inputSchema");
                }
                4 => {
                    tools[3]["inputSchema"] = json!({"type":"not-a-json-schema-type"});
                }
                _ => {
                    tools[3]["inputSchema"] = json!({"$ref":"https://example.invalid/schema.json"});
                }
            }
            let reason = parse_capabilities(&value).err().expect("must block");
            assert!(is_capability_failure(&reason), "variant {variant}");
        }
        let mut value = capability_fixture();
        value["result"]["tools"][3]["inputSchema"] =
            json!({"$ref":"file:///C:/private/schema.json"});
        assert!(parse_capabilities(&value).is_err());
    }

    #[test]
    fn schema_changes_reject_before_ipc_without_exposing_prompt() {
        for schema in [
            json!({"type":"object","required":["newRequiredField"]}),
            json!({"type":"object","properties":{"hostId":{"enum":["remote"]}}}),
            json!({"type":"object","properties":{"prompt":{"type":"integer"}}}),
            json!({"type":"object","properties":{"prompt":{"maxLength":3}}}),
            json!({"type":"object","properties":{"prompt":{"pattern":"^different$"}}}),
            json!({"type":"object","properties":{"threadId":{"type":"string"}},"additionalProperties":false}),
        ] {
            let mut value = capability_fixture();
            value["result"]["tools"][3]["inputSchema"] = schema;
            // No pipe is supplied. Schema errors must occur before connection/IPC.
            let desktop = Desktop {
                capabilities: parse_capabilities(&value).unwrap(),
                ..Default::default()
            };
            let reason = desktop
                .tool(
                    "thread",
                    "send_message_to_thread",
                    send_arguments("thread", "PRIVATE_PROMPT"),
                )
                .unwrap_err();
            assert!(reason.starts_with("Desktop 工具参数不兼容"));
            assert!(!reason.contains("PRIVATE_PROMPT"));
            assert_eq!(message_from_text(&reason).code, "backend.withDetails");
        }
    }

    #[test]
    fn additive_schema_changes_and_local_refs_remain_compatible() {
        let mut value = capability_fixture();
        let schema = &mut value["result"]["tools"][3]["inputSchema"];
        schema["$defs"] = json!({"text":{"type":"string"}});
        schema["properties"]["prompt"] = json!({"$ref":"#/$defs/text"});
        schema["properties"]["newOptionalField"] =
            json!({"anyOf":[{"type":"string"},{"type":"null"}]});
        schema["allOf"] = json!([{"properties":{"hostId":{"const":"local"}}}]);
        let desktop = Desktop {
            capabilities: parse_capabilities(&value).unwrap(),
            ..Default::default()
        };
        assert!(desktop.check_send("thread", "原文不改写").is_ok());
        assert_eq!(
            send_arguments("thread", "原文不改写")["prompt"],
            "原文不改写"
        );
        assert!(desktop
            .validate_arguments("list_threads", &json!({"limit":51}))
            .is_err());
    }

    #[test]
    fn read_thread_response_requires_target_and_turn_structure() {
        assert!(check_thread_response(
            &json!({"thread":{"id":"target","hostId":"local"},"turns":[]}),
            "target"
        )
        .is_ok());
        for response in [
            json!({}),
            json!({"thread":{"id":"target"},"turns":[]}),
            json!({"thread":{"id":"target","hostId":null},"turns":[]}),
            json!({"thread":{"id":"other"},"turns":[]}),
            json!({"thread":{"id":"target"},"turns":{}}),
            json!({"thread":{"id":"target","hostId":"remote"},"turns":[]}),
        ] {
            assert!(check_thread_response(&response, "target").is_err());
        }
    }
    #[cfg(not(windows))]
    #[test]
    fn experimental_platform_blocks_before_windows_discovery() {
        let mut desktop = Desktop::default();
        assert_eq!(
            desktop.connect("").unwrap_err(),
            "首版仅支持 Windows Native"
        );
        assert_eq!(
            crate::i18n::message_from_text("首版仅支持 Windows Native").code,
            "platform.windowsOnly"
        );
    }
    #[test]
    fn quota_retry_allowlist_excludes_auth_schema_rpc_and_other_tools() {
        let mut payload = json!({"success":false,"rpcError":null,"failureText":"Could not read current usage limits. Try again later.\n"});
        assert_eq!(
            failure_category("get_usage_limits", None, Some(&payload)),
            "quota_retryable"
        );
        assert_eq!(
            failure_category("send_message_to_thread", None, Some(&payload)),
            "hard_failure"
        );
        payload["failureText"] =
            json!("Usage limits require a ChatGPT account on this task's host.");
        assert_eq!(
            failure_category("get_usage_limits", None, Some(&payload)),
            "hard_failure"
        );
        payload["failureText"] = json!("Could not read current usage limits. Try again later.");
        payload["rpcError"] = json!("rejected");
        assert_eq!(
            failure_category("get_usage_limits", None, Some(&payload)),
            "hard_failure"
        );
        assert_eq!(
            failure_category("get_usage_limits", Some("transport.timeout"), None),
            "quota_retryable"
        );
        assert_eq!(
            failure_category(
                "get_usage_limits",
                Some("transport.contentIncompatible"),
                None
            ),
            "hard_failure"
        );
        let desktop = Desktop::default();
        desktop.record_failure(
            "get_usage_limits",
            "fixture",
            "fixture",
            1,
            "Translated text is irrelevant",
            None,
            Some("transport.timeout"),
        );
        assert!(desktop.quota_temporarily_unavailable());
    }
    #[test]
    fn list_transport_failure_is_retryable_but_send_and_protocol_failures_are_hard() {
        for code in [
            "transport.timeout",
            "transport.disconnected",
            "transport.pipeUnavailable",
            "transport.writeFailed",
            "transport.incomplete",
        ] {
            assert_eq!(
                failure_category("list_threads", Some(code), None),
                "read_retryable"
            );
            assert_eq!(
                failure_category("send_message_to_thread", Some(code), None),
                "hard_failure"
            );
            assert_eq!(
                failure_category("read_thread", Some(code), None),
                "hard_failure"
            );
        }
        assert_eq!(
            failure_category("list_threads", Some("transport.contentIncompatible"), None),
            "hard_failure"
        );
        assert_eq!(
            failure_category("list_threads", None, Some(&json!({"rpcError":"denied"}))),
            "hard_failure"
        );
        let desktop = Desktop::default();
        desktop.record_failure(
            "list_threads",
            "fixture",
            "fixture",
            10000,
            "timeout",
            None,
            Some("transport.timeout"),
        );
        assert!(desktop.read_temporarily_unavailable());
        assert!(!desktop.quota_temporarily_unavailable());
        desktop.take_failure();
        assert!(!desktop.read_temporarily_unavailable());
    }
    use super::*;
    #[test]
    fn local_failure_record_retains_error_text_context_and_drains_once() {
        let desktop = Desktop::default();
        let response = json!({"id":"rpc-fixture","error":{"code":-32602,"message":"Invalid app tool request: fixture explanation"},"result":{"success":false,"contentItems":[{"type":"inputText","text":"Caller thread is not loaded: fixture details"}]}});
        desktop.record_failure(
            "get_usage_limits",
            "caller-fixture",
            "call-fixture",
            123,
            "fixture failure",
            Some(&response),
            None,
        );
        let record = desktop.take_failure().unwrap();
        assert_eq!(record["tool"], "get_usage_limits");
        assert_eq!(record["callerThreadId"], "caller-fixture");
        assert_eq!(record["callId"], "call-fixture");
        assert_eq!(record["elapsedMs"], 123);
        assert!(record["response"]["failureText"]
            .as_str()
            .unwrap()
            .contains("fixture details"));
        assert!(record["response"]["rpcError"]
            .as_str()
            .unwrap()
            .contains("fixture explanation"));
        assert!(desktop.take_failure().is_none());
        let oversized = json!({"result":{"success":false,"contentItems":[{"type":"inputText","text":"x".repeat(20000)}]}});
        desktop.record_failure(
            "list_threads",
            "caller",
            "call",
            0,
            "failed",
            Some(&oversized),
            None,
        );
        assert_eq!(
            desktop.take_failure().unwrap()["response"]["failureText"]
                .as_str()
                .unwrap()
                .len(),
            16384
        );
    }

    #[test]
    fn tool_failure_diagnostics_distinguish_status_without_exposing_payloads() {
        let response = json!({"result":{"success":false,"contentItems":[{"type":"inputText","text":"caller thread unavailable: credential=PRIVATE_SECRET request=PRIVATE_MESSAGE"}]}});
        let detail = failure_diagnostic(&response, "get_usage_limits");
        assert!(detail.contains("tool=get_usage_limits"));
        assert!(detail.contains("success=false"));
        assert!(detail.contains("category=thread_context"));
        assert!(!detail.contains("PRIVATE_SECRET"));
        assert!(!detail.contains("PRIVATE_MESSAGE"));
        let missing = failure_diagnostic(&json!({"result":{}}), "list_threads");
        assert!(missing.contains("tool=list_threads"));
        assert!(missing.contains("success=missing"));
        assert!(missing.contains("items=missing"));
    }

    #[test]
    fn rpc_diagnostics_allow_only_numeric_codes_and_known_tool_names() {
        let detail = failure_diagnostic(
            &json!({"error":{"code":-32602,"message":"Invalid app tool request PRIVATE_MESSAGE"}}),
            "read_thread",
        );
        assert!(detail.contains("rpcCode=-32602"));
        assert!(detail.contains("category=arguments"));
        assert!(!detail.contains("PRIVATE_MESSAGE"));
        let untrusted = failure_diagnostic(
            &json!({"error":{"code":"PRIVATE_SECRET","message":"PRIVATE_MESSAGE"},"result":{"success":"PRIVATE_SECRET"}}),
            "PRIVATE_TOOL",
        );
        assert!(untrusted.contains("tool=other"));
        assert!(untrusted.contains("success=invalid"));
        assert!(!untrusted.contains("PRIVATE_"));
    }

    #[test]
    fn quota_requires_exact_windows_and_account() {
        let value = json!({"limit_id":"codex","primary":{"window_minutes":300,"used_percent":100.0},"secondary":{"window_minutes":10080,"used_percent":31.0}});
        assert!(parse_quota(&value, "scope", "2026-10-03T00:00:00Z").is_some());
        assert!(parse_quota(&value, "", "time").is_none());
        let mut changed = value;
        changed["primary"]["window_minutes"] = json!(10080);
        assert!(parse_quota(&changed, "scope", "time").is_none());
    }
    #[test]
    fn thread_identity_alone_is_never_execution_identity() {
        assert!(matches!(
            classify_delivery(&json!({"threadId":"thread"}), "thread"),
            Delivery::Submitted
        ));
        assert!(matches!(
            classify_delivery(
                &json!({"threadId":"thread","turnId":"01a10032-c016-7ec0-8fc3-1c09ec019786"}),
                "thread"
            ),
            Delivery::Accepted { .. }
        ));
        assert!(matches!(
            classify_delivery(
                &json!({"threadId":"wrong","turnId":"01a10032-c016-7ec0-8fc3-1c09ec019786"}),
                "thread"
            ),
            Delivery::Unknown(_)
        ));
    }
    #[cfg(windows)]
    fn fixture_pipe(oversized: bool) -> (String, std::thread::JoinHandle<()>) {
        let path = format!("\\\\.\\pipe\\auto-resume-fixture-{}", Uuid::new_v4());
        let server_path = path.clone();
        let (ready, listen) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut server = tokio::net::windows::named_pipe::ServerOptions::new()
                    .create(server_path)
                    .unwrap();
                ready.send(()).unwrap();
                server.connect().await.unwrap();
                let n = server.read_u32_le().await.unwrap();
                let mut b = vec![0; n as usize];
                server.read_exact(&mut b).await.unwrap();
                let request: Value = serde_json::from_slice(&b).unwrap();
                if oversized {
                    server
                        .write_all(&((MAX_FRAME + 1) as u32).to_le_bytes())
                        .await
                        .unwrap();
                    return;
                }
                let response = serde_json::to_vec(
                    &json!({"jsonrpc":"2.0","id":request["id"],"result":{"fixture":true}}),
                )
                .unwrap();
                let header = (response.len() as u32).to_le_bytes();
                server.write_all(&header[..2]).await.unwrap();
                server.write_all(&header[2..]).await.unwrap();
                for chunk in response.chunks(7) {
                    server.write_all(chunk).await.unwrap();
                }
                // Keep pipe alive until the client receives all buffered fragments.
                let mut end = [0u8; 1];
                let _ = tokio::time::timeout(Duration::from_secs(2), server.read(&mut end)).await;
            });
        });
        listen.recv_timeout(Duration::from_secs(2)).unwrap();
        (path, handle)
    }
    #[test]
    #[cfg(windows)]
    fn isolated_pipe_reassembles_fragmented_frames() {
        let (path, server) = fixture_pipe(false);
        assert_eq!(
            rpc(&path, "fixture/read", json!({}), Duration::from_secs(2)).unwrap()["result"]
                ["fixture"],
            true
        );
        server.join().unwrap();
    }
    #[test]
    #[cfg(windows)]
    fn isolated_pipe_rejects_oversized_header_without_reading_body() {
        let (path, server) = fixture_pipe(true);
        assert!(rpc(&path, "fixture/read", json!({}), Duration::from_secs(2)).is_err());
        server.join().unwrap();
    }
}
