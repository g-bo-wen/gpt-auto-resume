//! Read-only rollout observation. Never writes Codex's thread store.
use crate::{
    contracts::{DesktopInputEvidence, Quota, Session, TerminalEvent, TurnEvidence},
    desktop::{hash, parse_quota},
};
use serde_json::Value;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};
const MAX_TAIL: u64 = 8 * 1024 * 1024;
const MAX_CONFIRMATION: u64 = 16 * 1024 * 1024;
const SCAN_STEP: u64 = 4 * 1024 * 1024;

#[derive(Clone)]
struct Scan {
    session: Session,
    scope: String,
    current_turn: String,
    quota: Option<Quota>,
    pending_prompt: Option<String>,
    pending_time: Option<String>,
    bytes_read: u64,
    file_identity: (u64, u64),
    continuity_hash: String,
}

#[derive(Default)]
pub struct EvidenceCache(std::collections::HashMap<String, (u64, Scan)>);

pub fn discover(home: &Path) -> Vec<(Session, String)> {
    discover_with_evidence(
        home,
        &std::collections::HashMap::new(),
        &mut EvidenceCache::default(),
    )
}

pub fn discover_with_evidence(
    home: &Path,
    baselines: &std::collections::HashMap<String, u64>,
    cache: &mut EvidenceCache,
) -> Vec<(Session, String)> {
    cache
        .0
        .retain(|id, (baseline, _)| baselines.get(id) == Some(baseline));
    let mut paths: Vec<PathBuf> = walkdir::WalkDir::new(home.join("sessions"))
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl"))
        .map(|e| e.into_path())
        .collect();
    paths.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    paths
        .into_iter()
        .rev()
        .take(80)
        .filter_map(|p| {
            let (mut session, scope) = read(&p).ok()?;
            if let Some(&baseline) = baselines.get(&session.thread_id) {
                let prior = cache
                    .0
                    .get(&session.thread_id)
                    .map(|(_, scan)| scan.clone());
                match scan(&p, Some(baseline), prior, SCAN_STEP) {
                    Ok(evidence) => {
                        session.turns = evidence.session.turns.clone();
                        session.last_user_order = evidence.session.last_user_order;
                        session.confirmation_incomplete = evidence.session.confirmation_incomplete;
                        cache
                            .0
                            .insert(session.thread_id.clone(), (baseline, evidence));
                    }
                    Err(reason) => session.confirmation_issue = Some(reason),
                }
            }
            Some((session, scope))
        })
        .collect()
}
pub fn read(path: &Path) -> Result<(Session, String), String> {
    let scan = scan(path, None, None, MAX_TAIL)?;
    Ok((scan.session, scan.scope))
}

fn scan(
    path: &Path,
    baseline: Option<u64>,
    previous: Option<Scan>,
    budget: u64,
) -> Result<Scan, String> {
    let f = File::open(path).map_err(|_| "会话文件无法只读打开")?;
    let identity = file_identity(&f)?;
    let length = f.metadata().map_err(|_| "会话文件状态无法读取")?.len();
    let mut reader = BufReader::new(f);
    let mut first = String::new();
    reader
        .read_line(&mut first)
        .map_err(|_| "会话元信息无法读取")?;
    if first.len() > 512 * 1024 {
        return Err("会话元信息超出限制".into());
    }
    let meta: Value = serde_json::from_str(&first).map_err(|_| "会话元信息无法识别")?;
    let p = &meta["payload"];
    if meta["type"] != "session_meta" || p["originator"] != "Codex Desktop" {
        return Err("尚未核实 Desktop 来源".into());
    }
    let scope = p["creator_account_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(hash)
        .ok_or("会话账号未核实")?;
    let id = p["id"].as_str().ok_or("会话身份缺失")?;
    let mut session = Session {
        thread_id: id.into(),
        title: "未命名会话".into(),
        cwd: p["cwd"].as_str().unwrap_or("").into(),
        updated_at: meta["timestamp"]
            .as_str()
            .or_else(|| p["timestamp"].as_str())
            .unwrap_or("")
            .into(),
        source: "Desktop / Windows Native".into(),
        status: "Unknown".into(),
        latest: None,
        turns: vec![],
        observed_order: 0,
        ..Default::default()
    };
    let mut current_turn = String::new();
    let mut quota: Option<Quota> = None;
    let mut pending_prompt: Option<String> = None;
    let mut pending_time: Option<String> = None;
    let mut bytes_read = first.len() as u64;
    if let Some(prior) = previous {
        if prior.session.thread_id != id || prior.scope != scope || prior.file_identity != identity
        {
            return Err("确认日志身份变化，无法继续读取".into());
        }
        let check_bytes = prior.session.observed_order.min(4096);
        if prior
            .bytes_read
            .saturating_add(bytes_read)
            .saturating_add(check_bytes)
            > MAX_CONFIRMATION
        {
            return Err("执行证据超过16MiB累计确认读取上限；不扣次数、不重发".into());
        }
        if continuity_hash(&mut reader, prior.session.observed_order)? != prior.continuity_hash {
            return Err("确认日志已读位置内容变化，无法继续读取；不扣次数、不重发".into());
        }
        session = prior.session;
        current_turn = prior.current_turn;
        quota = prior.quota;
        pending_prompt = prior.pending_prompt;
        pending_time = prior.pending_time;
        bytes_read += prior.bytes_read + check_bytes;
    }
    let start = if let Some(baseline) = baseline {
        if baseline > length || session.observed_order > length {
            return Err("确认日志短于发送基线或已读位置，无法继续读取".into());
        }
        session.observed_order.max(baseline)
    } else {
        length.saturating_sub(MAX_TAIL)
    };
    if baseline.is_some() && start > 0 {
        reader
            .seek(SeekFrom::Start(start - 1))
            .map_err(|_| "确认读取基线无法核实")?;
        let mut delimiter = [0];
        reader
            .read_exact(&mut delimiter)
            .map_err(|_| "确认读取基线无法核实")?;
        if delimiter[0] != b'\n' {
            return Err("确认读取基线不是完整事件边界".into());
        }
    }
    reader
        .seek(SeekFrom::Start(start))
        .map_err(|_| "会话尾部读取失败")?;
    let mut offset = start;
    if start > 0 && baseline.is_none() {
        let mut skip = Vec::new();
        offset += reader
            .read_until(b'\n', &mut skip)
            .map_err(|_| "会话尾部读取失败")? as u64;
    }
    let mut bytes = Vec::new();
    session.observed_order = offset;
    loop {
        if offset >= length || offset.saturating_sub(start) >= budget {
            break;
        }
        if baseline.is_some_and(|b| offset.saturating_sub(b) >= MAX_CONFIRMATION) {
            return Err("执行证据超过16MiB确认读取上限；不扣次数、不重发".into());
        }
        bytes.clear();
        let n = reader
            .by_ref()
            .take((length - offset).min(1024 * 1024 + 1))
            .read_until(b'\n', &mut bytes)
            .map_err(|_| "会话读取失败")?;
        if n == 0 {
            break;
        }
        bytes_read += n as u64;
        if baseline.is_some() && bytes_read > MAX_CONFIRMATION {
            return Err("执行证据超过16MiB累计确认读取上限；不扣次数、不重发".into());
        }
        if baseline
            .is_some_and(|b| offset.saturating_add(n as u64).saturating_sub(b) > MAX_CONFIRMATION)
        {
            return Err("执行证据超过16MiB确认读取上限；不扣次数、不重发".into());
        }
        if n > 1024 * 1024 {
            return Err("会话事件过大，无法确认最新状态".into());
        }
        if !bytes.ends_with(b"\n") {
            if baseline.is_some() {
                break;
            }
            return Err("最新会话事件尚未写入完整".into());
        }
        offset += n as u64;
        let row: Value = serde_json::from_slice(&bytes).map_err(|_| "会话事件格式无法确认")?;
        let payload = &row["payload"];
        session.observed_order = offset;
        let time = row["timestamp"].as_str().unwrap_or("");
        if !time.is_empty() {
            session.updated_at = time.into();
        }
        if row["type"] == "event_msg" {
            match payload["type"].as_str().unwrap_or("") {
                "task_started" => {
                    current_turn = payload["turn_id"].as_str().unwrap_or("").into();
                    quota = None;
                    session.latest = None;
                    session.status = "Running".into();
                    let has_user_input = pending_prompt.is_some();
                    session.turns.push(TurnEvidence {
                        turn_id: current_turn.clone(),
                        order: offset,
                        started_at: time.into(),
                        user_message_at: pending_time.take(),
                        prompt_hash: pending_prompt.take(),
                        executed: false,
                        failed: false,
                        desktop_inputs: vec![],
                        has_user_input,
                        invalid_desktop_input: false,
                    });
                    if baseline.is_some() && session.turns.len() > 128 {
                        return Err("确认期间执行turn超过128个；不扣次数、不重发".into());
                    }
                }
                "user_message" => {
                    session.last_user_order = offset;
                    session.latest = None;
                    if let Some(message) = payload["message"].as_str() {
                        let h = hash(message);
                        if session.status == "Running" {
                            if let Some(t) = session.turns.last_mut() {
                                t.has_user_input = true;
                                t.prompt_hash = Some(h);
                                t.user_message_at = Some(time.into());
                            }
                        } else {
                            pending_prompt = Some(h);
                            pending_time = Some(time.into());
                            session.status = "Pending".into();
                        }
                    }
                }
                "token_count" => {
                    if !current_turn.is_empty() {
                        if let Some(q) = parse_quota(&payload["rate_limits"], &scope, time) {
                            quota = Some(q);
                        }
                    }
                }
                "agent_message"
                | "agent_reasoning"
                | "exec_command_begin"
                | "mcp_tool_call_begin" => {
                    if let Some(t) = session.turns.last_mut() {
                        t.executed = true;
                    }
                }
                "task_complete" | "turn_aborted" => {
                    let turn = payload["turn_id"].as_str().unwrap_or("");
                    if turn.is_empty() {
                        session.latest = None;
                        session.status = "Unknown".into();
                        continue;
                    }
                    if !current_turn.is_empty() && turn != current_turn {
                        continue; // An older completion cannot replace a newer running turn.
                    }
                    if !current_turn.is_empty() && session.status == "Pending" {
                        continue; // A queued user input has already invalidated the old turn.
                    }
                    let kind = if payload["type"] == "turn_aborted" {
                        "interrupted"
                    } else if payload["error"].is_null() {
                        "completed"
                    } else if payload["error"]["codex_error_info"] == "usage_limit_exceeded" {
                        "usage_limit_exceeded"
                    } else {
                        "failed"
                    };
                    let bound = turn == current_turn && !current_turn.is_empty();
                    // A terminal event can be displayed without a start, but cannot
                    // manufacture execution proof or same-turn quota eligibility.
                    if !bound {
                        quota = None;
                    }
                    if let Some(t) = session.turns.last_mut().filter(|_| bound) {
                        t.failed = kind != "completed";
                        if kind == "completed" {
                            t.executed = true;
                        }
                    }
                    session.latest = Some(TerminalEvent {
                        event_id: hash(&format!("{id}:{turn}:{offset}:{kind}")),
                        turn_id: turn.into(),
                        order: offset,
                        timestamp: time.into(),
                        kind: kind.into(),
                        quota: quota.clone(),
                    });
                    session.status = kind.into();
                }
                _ => {}
            }
        }
        if row["type"] == "response_item" && payload["type"] == "function_call" {
            if let Some(t) = session.turns.last_mut() {
                t.executed = true;
            }
        }
        if row["type"] == "response_item" {
            if payload["type"] == "message" && payload["role"] == "user" {
                session.last_user_order = offset;
                session.latest = None;
                if session.status == "Running" {
                    if let Some(t) = session.turns.last_mut() {
                        t.has_user_input = true;
                    }
                } else {
                    session.status = "Pending".into();
                    let text = payload["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|i| i["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    pending_prompt = Some(hash(&text));
                    pending_time = Some(time.into());
                }
            }
            if let Some(t) = session.turns.last_mut() {
                if payload["type"] == "function_call_output"
                    && payload["name"] == "send_message_to_thread"
                {
                    // This is the Desktop's persisted input item, not a user message.
                    // Its metadata explicitly identifies the containing execution turn.
                    session.latest = None;
                    let parsed = desktop_input(payload, offset, &t.turn_id);
                    match parsed {
                        Some(input) => t.desktop_inputs.push(input),
                        None => t.invalid_desktop_input = true,
                    }
                }
                if payload["type"] == "message"
                    && payload["role"] == "assistant"
                    && payload["content"].as_array().is_some_and(|items| {
                        items.iter().any(|i| {
                            i["type"] == "output_text"
                                && i["text"].as_str().is_some_and(|s| !s.is_empty())
                        })
                    })
                {
                    t.executed = true;
                }
            }
        }
    }
    if baseline.is_some() && session.turns.len() > 128 {
        return Err("确认期间执行turn超过128个；不扣次数、不重发".into());
    }
    if baseline.is_none() && session.turns.len() > 30 {
        session.turns.drain(..session.turns.len() - 30);
    }
    session.confirmation_incomplete = offset < length;
    if baseline.is_some() && bytes_read.saturating_add(offset.min(4096)) > MAX_CONFIRMATION {
        return Err("执行证据超过16MiB累计确认读取上限；不扣次数、不重发".into());
    }
    let continuity_hash = if baseline.is_some() {
        continuity_hash(&mut reader, offset)?
    } else {
        String::new()
    };
    if baseline.is_some() {
        bytes_read += offset.min(4096);
    }
    Ok(Scan {
        session,
        scope,
        current_turn,
        quota,
        pending_prompt,
        pending_time,
        bytes_read,
        file_identity: identity,
        continuity_hash,
    })
}

fn continuity_hash(reader: &mut BufReader<File>, cursor: u64) -> Result<String, String> {
    let start = cursor.saturating_sub(4096);
    reader
        .seek(SeekFrom::Start(start))
        .map_err(|_| "确认读取连续性无法核实")?;
    let mut bytes = vec![0; (cursor - start) as usize];
    reader
        .read_exact(&mut bytes)
        .map_err(|_| "确认日志已截断，无法继续读取")?;
    use sha2::{Digest, Sha256};
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(windows)]
fn file_identity(file: &File) -> Result<(u64, u64), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // The live File owns this handle; the API writes only to this initialized structure.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err("确认日志文件身份无法核实".into());
    }
    Ok((
        info.dwVolumeSerialNumber as u64,
        ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    ))
}

#[cfg(unix)]
fn file_identity(file: &File) -> Result<(u64, u64), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata().map_err(|_| "确认日志文件身份无法核实")?;
    Ok((meta.dev(), meta.ino()))
}

fn desktop_input(payload: &Value, order: u64, turn: &str) -> Option<DesktopInputEvidence> {
    if payload["namespace"] != "codex_app"
        || payload["internal_chat_message_metadata_passthrough"]["turn_id"] != turn
        || uuid::Uuid::parse_str(turn).is_err()
    {
        return None;
    }
    let item_id = payload["id"].as_str()?.strip_prefix("fco_")?;
    uuid::Uuid::parse_str(item_id).ok()?;
    let output = payload["output"].as_str()?;
    let envelope = output.strip_prefix("<codex_delegation>\n  <source_thread_id>")?;
    let (source, input) = envelope.split_once("</source_thread_id>\n  <input>")?;
    uuid::Uuid::parse_str(source).ok()?;
    let prompt = input.strip_suffix("</input>\n</codex_delegation>")?;
    Some(DesktopInputEvidence {
        item_id: format!("fco_{item_id}"),
        order,
        source_thread_id: source.into(),
        prompt_hash: hash(prompt),
    })
}

pub fn desktop_titles(list: &Value) -> std::collections::HashMap<String, String> {
    let mut titles = std::collections::HashMap::new();
    for field in ["threads", "pinnedThreads"] {
        if let Some(items) = list[field].as_array() {
            for item in items {
                // Only local Codex threads can be sent through this adapter.
                if item
                    .get("hostId")
                    .and_then(Value::as_str)
                    .is_some_and(|h| h != "local")
                {
                    continue;
                }
                if item
                    .get("source")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s == "chatgpt")
                {
                    continue;
                }
                if let (Some(id), Some(title)) = (
                    item["threadId"].as_str().or_else(|| item["id"].as_str()),
                    item["title"].as_str(),
                ) {
                    titles.insert(id.into(), title.into());
                }
            }
        }
    }
    titles
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn meta() -> Value {
        json!({"type":"session_meta","payload":{"id":"01a10032-c016-7ec0-8fc3-1c09ec019786","originator":"Codex Desktop","creator_account_id":"fixture-account"}})
    }

    #[test]
    fn long_turn_terminal_is_displayed_without_manufacturing_execution_or_quota() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{}", meta()).unwrap();
        writeln!(
            f,
            "{}",
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"long"}})
        )
        .unwrap();
        let padding = json!({"type":"padding","payload":"x".repeat(256 * 1024)});
        for _ in 0..36 {
            writeln!(f, "{padding}").unwrap();
        }
        writeln!(
            f,
            "{}",
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"long"}})
        )
        .unwrap();
        let (s, _) = read(f.path()).unwrap();
        assert_eq!(s.status, "completed");
        assert!(s.turns.is_empty());
        assert!(s.latest.unwrap().quota.is_none());
        writeln!(
            f,
            "{}",
            json!({"type":"event_msg","payload":{"type":"user_message","message":"new input"}})
        )
        .unwrap();
        let (s, _) = read(f.path()).unwrap();
        assert_eq!(s.status, "Pending");
        assert!(s.latest.is_none());
    }

    #[test]
    fn incremental_scan_keeps_candidate_across_steps_and_never_reads_before_baseline() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{}", meta()).unwrap();
        writeln!(
            f,
            "{}",
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"old"}})
        )
        .unwrap();
        let baseline = f.as_file().metadata().unwrap().len();
        for row in [
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"01a1021f-680f-7a62-b645-28d42be970f7"}}),
            json!({"type":"response_item","payload":input_fixture()}),
            json!({"type":"event_msg","payload":{"type":"agent_message"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"01a1021f-680f-7a62-b645-28d42be970f7"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"manual"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"manual"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"manual"}}),
        ] {
            writeln!(f, "{row}").unwrap();
        }
        let mut prior = None;
        let mut cursor = baseline;
        for _ in 0..10 {
            let next = scan(f.path(), Some(baseline), prior, 100).unwrap();
            assert!(next.session.observed_order > cursor);
            cursor = next.session.observed_order;
            if !next.session.confirmation_incomplete {
                assert_eq!(next.session.turns.len(), 2);
                assert!(next.session.turns[0].executed);
                assert_eq!(next.session.turns[0].desktop_inputs.len(), 1);
                assert!(next.session.turns[1].has_user_input);
                assert!(next.session.last_user_order > next.session.turns[0].order);
                assert_eq!(cursor, f.as_file().metadata().unwrap().len());
                return;
            }
            prior = Some(next);
        }
        panic!("incremental scan did not finish");
    }

    #[test]
    fn partial_record_is_retried_at_same_cursor_then_processed_once() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{}", meta()).unwrap();
        let baseline = f.as_file().metadata().unwrap().len();
        let row = json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"new"}})
            .to_string();
        write!(f, "{}", &row[..20]).unwrap();
        let incomplete = scan(f.path(), Some(baseline), None, SCAN_STEP).unwrap();
        assert!(incomplete.session.confirmation_incomplete);
        assert_eq!(incomplete.session.observed_order, baseline);
        writeln!(f, "{}", &row[20..]).unwrap();
        let complete = scan(f.path(), Some(baseline), Some(incomplete), SCAN_STEP).unwrap();
        assert!(!complete.session.confirmation_incomplete);
        assert_eq!(complete.session.turns.len(), 1);
    }

    #[test]
    fn response_user_after_completed_resume_belongs_to_next_turn() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{}", meta()).unwrap();
        let baseline = f.as_file().metadata().unwrap().len();
        for row in [
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"01a1021f-680f-7a62-b645-28d42be970f7"}}),
            json!({"type":"response_item","payload":input_fixture()}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"01a1021f-680f-7a62-b645-28d42be970f7"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"manual followup"}]}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"manual"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"manual"}}),
        ] {
            writeln!(f, "{row}").unwrap();
        }
        let scan = scan(f.path(), Some(baseline), None, SCAN_STEP).unwrap();
        assert!(!scan.session.turns[0].has_user_input);
        assert!(scan.session.turns[0].executed);
        assert!(scan.session.turns[1].has_user_input);
    }

    #[test]
    fn rewritten_prefix_and_replaced_file_are_not_treated_as_append() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{}", meta()).unwrap();
        let baseline = f.as_file().metadata().unwrap().len();
        let event = json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"old"}});
        writeln!(f, "{event}").unwrap();
        let prior = scan(f.path(), Some(baseline), None, SCAN_STEP).unwrap();
        f.as_file_mut().seek(SeekFrom::Start(baseline)).unwrap();
        writeln!(
            f,
            "{}",
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"new"}})
        )
        .unwrap();
        writeln!(f, "{event}").unwrap();
        let reason = scan(f.path(), Some(baseline), Some(prior.clone()), SCAN_STEP)
            .err()
            .unwrap();
        assert!(reason.contains("内容变化"));
        let mut replacement = tempfile::NamedTempFile::new().unwrap();
        writeln!(replacement, "{}", meta()).unwrap();
        writeln!(replacement, "{event}").unwrap();
        assert!(
            scan(replacement.path(), Some(baseline), Some(prior), SCAN_STEP)
                .err()
                .unwrap()
                .contains("身份变化")
        );
    }

    #[test]
    fn cumulative_confirmation_read_is_bounded() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{}", meta()).unwrap();
        let baseline = f.as_file().metadata().unwrap().len();
        let padding = json!({"type":"padding","payload":"x".repeat(512 * 1024)});
        for _ in 0..33 {
            writeln!(f, "{padding}").unwrap();
        }
        let mut previous = None;
        for _ in 0..6 {
            match scan(f.path(), Some(baseline), previous, SCAN_STEP) {
                Ok(next) => {
                    assert!(next.session.observed_order - baseline <= MAX_CONFIRMATION);
                    previous = Some(next);
                }
                Err(reason) => {
                    assert!(reason.contains("16MiB"));
                    return;
                }
            }
        }
        panic!("confirmation was not bounded");
    }

    fn input_fixture() -> Value {
        json!({"type":"function_call_output","id":"fco_01a1021f-683e-7170-b734-3d03dcafbfef","name":"send_message_to_thread","namespace":"codex_app",
            "output":"<codex_delegation>\n  <source_thread_id>01a10032-c016-7ec0-8fc3-1c09ec019786</source_thread_id>\n  <input>resume</input>\n</codex_delegation>",
            "internal_chat_message_metadata_passthrough":{"turn_id":"01a1021f-680f-7a62-b645-28d42be970f7"}})
    }

    #[test]
    fn desktop_tool_input_and_pure_assistant_reply_are_bound_to_explicit_turn() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let turn = "01a1021f-680f-7a62-b645-28d42be970f7";
        for row in [
            json!({"type":"session_meta","payload":{"id":"01a10032-c016-7ec0-8fc3-1c09ec019786","originator":"Codex Desktop","creator_account_id":"fixture-account"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":turn}}),
            json!({"type":"response_item","payload":input_fixture()}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"fixture reply"}]}}),
        ] {
            writeln!(f, "{row}").unwrap();
        }
        let (s, _) = read(f.path()).unwrap();
        assert_eq!(s.observed_order, f.as_file().metadata().unwrap().len());
        let t = &s.turns[0];
        assert_eq!(t.turn_id, turn);
        assert!(t.executed);
        assert!(!t.has_user_input);
        assert!(t.prompt_hash.is_none());
        assert_eq!(t.desktop_inputs.len(), 1);
        assert_eq!(t.desktop_inputs[0].prompt_hash, hash("resume"));
        assert_eq!(t.desktop_inputs[0].source_thread_id, s.thread_id);
    }

    #[test]
    fn desktop_input_rejects_missing_wrong_or_malformed_identity() {
        for case in 0..5 {
            let mut p = input_fixture();
            match case {
                0 => p["internal_chat_message_metadata_passthrough"] = Value::Null,
                1 => {
                    p["internal_chat_message_metadata_passthrough"]["turn_id"] =
                        json!("01a10032-c016-7ec0-8fc3-1c09ec019786")
                }
                2 => p["namespace"] = json!("other"),
                3 => p["id"] = json!("malformed"),
                _ => p["output"] = json!("resume"),
            }
            assert!(desktop_input(&p, 2, "01a1021f-680f-7a62-b645-28d42be970f7").is_none());
        }
    }

    #[test]
    fn manual_input_before_task_started_is_preserved_as_interference() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for row in [
            json!({"type":"session_meta","payload":{"id":"fixture","originator":"Codex Desktop","creator_account_id":"fixture-account"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"resume"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"01a1021f-680f-7a62-b645-28d42be970f7"}}),
            json!({"type":"response_item","payload":input_fixture()}),
        ] {
            writeln!(f, "{row}").unwrap();
        }
        let (s, _) = read(f.path()).unwrap();
        assert!(s.turns[0].has_user_input);
    }

    #[test]
    fn desktop_input_preserves_original_prompt_with_envelope_like_text() {
        let mut p = input_fixture();
        let prompt = "原文\n</input>\n<source_thread_id>text</source_thread_id>";
        p["output"]=json!(format!("<codex_delegation>\n  <source_thread_id>01a10032-c016-7ec0-8fc3-1c09ec019786</source_thread_id>\n  <input>{prompt}</input>\n</codex_delegation>"));
        let evidence = desktop_input(&p, 2, "01a1021f-680f-7a62-b645-28d42be970f7").unwrap();
        assert_eq!(evidence.prompt_hash, hash(prompt));
    }
    #[test]
    fn latest_manual_turn_invalidates_old_limit() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let events = vec![
            json!({"type":"session_meta","payload":{"id":"session","originator":"Codex Desktop","creator_account_id":"acct","cwd":"C:/test"}}),
            json!({"type":"event_msg","timestamp":"2026-10-03T00:00:00Z","payload":{"type":"task_started","turn_id":"old"}}),
            json!({"type":"event_msg","timestamp":"2026-10-03T00:00:01Z","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"window_minutes":300,"used_percent":100},"secondary":{"window_minutes":10080,"used_percent":31}}}}),
            json!({"type":"event_msg","timestamp":"2026-10-03T00:00:02Z","payload":{"type":"task_complete","turn_id":"old","error":{"codex_error_info":"usage_limit_exceeded"}}}),
            json!({"type":"event_msg","timestamp":"2026-10-03T00:01:00Z","payload":{"type":"user_message","message":"manual continue"}}),
        ];
        for e in events {
            writeln!(f, "{e}").unwrap();
        }
        let (s, _) = read(f.path()).unwrap();
        assert!(s.latest.is_none());
        assert_eq!(s.status, "Pending");
    }
    #[test]
    fn unrelated_bucket_never_overwrites_codex_snapshot() {
        assert!(parse_quota(&serde_json::json!({"limit_id":"premium"}), "scope", "time").is_none());
    }
}
