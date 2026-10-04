use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use rusqlite::functions::FunctionFlags;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::contracts::{
    Delivery, Diagnostic, Observation, Quota, Session, Settings, Submission, WatchConfig,
};
use crate::i18n::{message_from_text, Message};

/// SQLite-backed, deliberately single-process state machine.  The application
/// holds this behind one mutex; attempts are persisted before an IPC call.
pub struct Engine {
    db: Connection,
}

impl Engine {
    pub fn open(path: &Path) -> Result<Self, String> {
        let db = Connection::open(path).map_err(db_err)?;
        db.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY CHECK(id=1), value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS watches (
               thread_id TEXT PRIMARY KEY, prompt TEXT NOT NULL, remaining INTEGER NULL,
               state TEXT NOT NULL, reason TEXT NULL, updated_at TEXT NOT NULL,
               reason_code TEXT NULL, reason_params TEXT NULL,
               source_event_id TEXT NULL, source_order INTEGER NULL, source_quota TEXT NULL,
               authorization INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS attempts (
               id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, prompt_hash TEXT NOT NULL,
               source_event_id TEXT NOT NULL, source_order INTEGER NOT NULL,
               status TEXT NOT NULL, counted INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL,
               detail TEXT NULL, accepted_turn_id TEXT NULL, baseline_order INTEGER NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS attempts_source_once
               ON attempts(thread_id, source_event_id);
             CREATE TABLE IF NOT EXISTS history (
               id INTEGER PRIMARY KEY AUTOINCREMENT, thread_id TEXT NULL, time TEXT NOT NULL,
               kind TEXT NOT NULL, message TEXT NOT NULL
               ,message_code TEXT NULL, message_params TEXT NULL
             );
             CREATE TABLE IF NOT EXISTS invalidated_sources (
               thread_id TEXT NOT NULL, event_id TEXT NOT NULL, reason TEXT NOT NULL,
               invalidated_at TEXT NOT NULL, PRIMARY KEY(thread_id,event_id)
             );
             CREATE TABLE IF NOT EXISTS observation (id INTEGER PRIMARY KEY CHECK(id=1), value TEXT NOT NULL, observed_at TEXT NOT NULL);",
        ).map_err(db_err)?;
        Self::ensure_attempt_column(&db)?;
        Self::ensure_message_columns(&db)?;
        Self::register_message_functions(&db)?;
        Self::install_message_trigger(&db)?;
        db.execute(
            "INSERT OR IGNORE INTO settings(id,value) VALUES(1,?1)",
            params![serde_json::to_string(&Settings::default()).map_err(json_err)?],
        )
        .map_err(db_err)?;

        // A process lifetime is an authorization and confirmation boundary.
        // Unconfirmed attempts become audit-only Unknown records after a crash.
        let tx = db.unchecked_transaction().map_err(db_err)?;
        let now = now();
        let restart = message_from_text("Restart requires explicit enable");
        let paused=tx.execute("UPDATE watches SET state='PausedAfterRestart', reason='Restart requires explicit enable', reason_code=?2, reason_params=?3, authorization=0, updated_at=?1 WHERE state NOT IN ('Stopped','Completed')", params![now, &restart.code, serde_json::to_string(&restart.params).map_err(json_err)?]).map_err(db_err)?;
        if paused > 0 {
            Self::history_tx(
                &tx,
                None,
                "restart_pause",
                "应用重新启动，旧 Watch 已暂停，需要显式重新开启",
            )?;
        }
        tx.execute("UPDATE attempts SET status='Unknown', detail='Application restarted before execution proof' WHERE status IN ('Inflight','Submitted','Accepted')", []).map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(Self { db })
    }

    fn ensure_attempt_column(db: &Connection) -> Result<(), String> {
        let mut stmt = db.prepare("PRAGMA table_info(attempts)").map_err(db_err)?;
        let columns: Result<Vec<String>, _> =
            stmt.query_map([], |r| r.get(1)).map_err(db_err)?.collect();
        let columns = columns.map_err(db_err)?;
        drop(stmt);
        if !columns.iter().any(|name| name == "accepted_turn_id") {
            db.execute(
                "ALTER TABLE attempts ADD COLUMN accepted_turn_id TEXT NULL",
                [],
            )
            .map_err(db_err)?;
        }
        if !columns.iter().any(|name| name == "baseline_order") {
            db.execute(
                "ALTER TABLE attempts ADD COLUMN baseline_order INTEGER NULL",
                [],
            )
            .map_err(db_err)?;
        }
        Ok(())
    }

    fn ensure_message_columns(db: &Connection) -> Result<(), String> {
        for (table, column, definition) in [
            ("watches", "reason_code", "TEXT NULL"),
            ("watches", "reason_params", "TEXT NULL"),
            ("history", "message_code", "TEXT NULL"),
            ("history", "message_params", "TEXT NULL"),
            ("attempts", "detail_code", "TEXT NULL"),
            ("attempts", "detail_params", "TEXT NULL"),
        ] {
            let mut stmt = db
                .prepare(&format!("PRAGMA table_info({table})"))
                .map_err(db_err)?;
            let names: Result<Vec<String>, _> =
                stmt.query_map([], |r| r.get(1)).map_err(db_err)?.collect();
            drop(stmt);
            if !names.map_err(db_err)?.iter().any(|name| name == column) {
                db.execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
                    [],
                )
                .map_err(db_err)?;
            }
        }
        Ok(())
    }

    fn install_message_trigger(db: &Connection) -> Result<(), String> {
        // Direct state-machine SQL also writes reason metadata. Existing rows
        // are untouched, so old databases continue to present their audit text
        // verbatim until a new state transition replaces it.
        db.execute_batch(
            "DROP TRIGGER IF EXISTS watches_reason_message;
             CREATE TRIGGER watches_reason_message
             AFTER UPDATE OF reason ON watches
             BEGIN
               UPDATE watches SET reason_code=CASE WHEN NEW.reason IS NULL THEN NULL ELSE message_code(NEW.reason) END,
                 reason_params=CASE WHEN NEW.reason IS NULL THEN NULL ELSE message_params(NEW.reason) END
                WHERE thread_id=NEW.thread_id;
             END;
             DROP TRIGGER IF EXISTS attempts_detail_message;
             CREATE TRIGGER attempts_detail_message AFTER UPDATE OF detail ON attempts
             BEGIN
               UPDATE attempts SET detail_code=CASE WHEN NEW.detail IS NULL THEN NULL ELSE message_code(NEW.detail) END,
                 detail_params=CASE WHEN NEW.detail IS NULL THEN NULL ELSE message_params(NEW.detail) END WHERE id=NEW.id;
             END;
             DROP TRIGGER IF EXISTS attempts_insert_message;
             CREATE TRIGGER attempts_insert_message AFTER INSERT ON attempts WHEN NEW.detail IS NOT NULL
             BEGIN
               UPDATE attempts SET detail_code=message_code(NEW.detail),detail_params=message_params(NEW.detail) WHERE id=NEW.id;
             END;"
        ).map_err(db_err)
    }

    fn register_message_functions(db: &Connection) -> Result<(), String> {
        db.create_scalar_function(
            "message_code",
            1,
            FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let raw: String = ctx.get(0)?;
                Ok(message_from_text(&raw).code)
            },
        )
        .map_err(db_err)?;
        db.create_scalar_function(
            "message_params",
            1,
            FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let raw: String = ctx.get(0)?;
                serde_json::to_string(&message_from_text(&raw).params)
                    .map_err(|_| rusqlite::Error::InvalidQuery)
            },
        )
        .map_err(db_err)
    }

    pub fn snapshot(&self) -> Result<Value, String> {
        let watches = self.read_watches()?;
        let mut stmt = self
            .db
            .prepare(
                "SELECT id,thread_id,time,kind,message,message_code,message_params FROM history ORDER BY id DESC LIMIT 500",
            )
            .map_err(db_err)?;
        let history: Result<Vec<Value>, _> = stmt.query_map([], |r| {
            let raw: String = r.get(4)?;
            let display = Self::stored_message(r.get(5)?, r.get(6)?, &raw);
            Ok(json!({"id":r.get::<_,i64>(0)?,"threadId":r.get::<_,Option<String>>(1)?,"time":r.get::<_,String>(2)?,"kind":r.get::<_,String>(3)?,"message":raw,"displayMessage":display}))
        })
            .map_err(db_err)?.collect();
        let observation: Option<Value> = self
            .db
            .query_row("SELECT value FROM observation WHERE id=1", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(db_err)?
            .map(|s| serde_json::from_str(&s).map_err(json_err))
            .transpose()?;
        Ok(
            json!({"watches":watches,"history":history.map_err(db_err)?,"settings":self.settings()?,"resolvedLanguage":crate::i18n::resolved_language(&self.settings()?.language),"observation":observation,"attempts":self.read_attempts()?}),
        )
    }

    pub fn settings(&self) -> Result<Settings, String> {
        let value: String = self
            .db
            .query_row("SELECT value FROM settings WHERE id=1", [], |r| r.get(0))
            .map_err(db_err)?;
        let mut settings: Settings = serde_json::from_str(&value).map_err(json_err)?;
        settings.language = crate::i18n::normalize_preference(&settings.language).into();
        Ok(settings)
    }

    pub fn confirmation_pending(&self) -> Result<bool, String> {
        self.db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM attempts WHERE status IN ('Submitted','Accepted') AND counted=0)",
                [],
                |r| r.get(0),
            )
            .map_err(db_err)
    }

    pub fn pending_baselines(&self) -> Result<std::collections::HashMap<String, u64>, String> {
        let mut stmt = self.db.prepare("SELECT thread_id,baseline_order,created_at FROM attempts WHERE status IN ('Submitted','Accepted') AND counted=0 AND baseline_order IS NOT NULL").map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(db_err)?;
        let mut baselines = std::collections::HashMap::new();
        for row in rows {
            let (thread, baseline, created) = row.map_err(db_err)?;
            if DateTime::parse_from_rfc3339(&created)
                .is_ok_and(|t| Utc::now().signed_duration_since(t) < Duration::seconds(120))
            {
                if let Ok(baseline) = u64::try_from(baseline) {
                    baselines.insert(thread, baseline);
                }
            }
        }
        Ok(baselines)
    }

    pub fn save_settings(&self, settings: Settings) -> Result<(), String> {
        self.db
            .execute(
                "UPDATE settings SET value=?1 WHERE id=1",
                params![serde_json::to_string(&settings).map_err(json_err)?],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn set_language(&self, language: &str) -> Result<(), String> {
        let mut settings = self.settings()?;
        settings.language = crate::i18n::normalize_preference(language).into();
        self.db
            .execute(
                "UPDATE settings SET value=?1 WHERE id=1",
                params![serde_json::to_string(&settings).map_err(json_err)?],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn observe(&self, observation: Observation) -> Result<(), String> {
        let tx = self.db.unchecked_transaction().map_err(db_err)?;
        let serialized = serde_json::to_string(&observation).map_err(json_err)?;
        tx.execute("INSERT INTO observation(id,value,observed_at) VALUES(1,?1,?2) ON CONFLICT(id) DO UPDATE SET value=excluded.value,observed_at=excluded.observed_at", params![serialized, now()]).map_err(db_err)?;
        if !observation.diagnostic.compatible {
            tx.execute("UPDATE attempts SET status='Unknown',accepted_turn_id=NULL,detail='Desktop compatibility lost while confirming; no retry' WHERE status IN ('Submitted','Accepted') AND counted=0",[]).map_err(db_err)?;
            tx.execute("UPDATE watches SET state='NeedsAttention', reason=?1, authorization=0, updated_at=?2 WHERE state NOT IN ('Stopped','Completed')", params![redact(&observation.diagnostic.reason), now()]).map_err(db_err)?;
            Self::diagnostic_history_tx(
                &tx,
                "diagnostic_incompatible",
                &format!(
                    "{}: {}",
                    redact(&observation.diagnostic.stage),
                    redact(&observation.diagnostic.reason)
                ),
            )?;
        } else {
            self.confirm_attempts_tx(&tx, &observation)?;
            self.refresh_watches_tx(&tx, &observation)?;
        }
        tx.commit().map_err(db_err)
    }

    pub fn configure(&self, config: WatchConfig, allow_immediate: bool) -> Result<(), String> {
        validate_config(&config)?;
        let observation = self.current_observation()?;
        require_session(&observation, &config.thread_id)?;
        require_compatible(&observation.diagnostic)?;
        if self.would_send_now(&observation, &config.thread_id)? && !allow_immediate {
            return Err(format!(
                "This watch is ready to send immediately. Confirm prompt preview: {}",
                preview(&config.prompt)
            ));
        }
        let exists: Option<String> = self
            .db
            .query_row(
                "SELECT state FROM watches WHERE thread_id=?1",
                params![config.thread_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        let now = now();
        match exists.as_deref() {
            None => {
                self.db.execute("INSERT INTO watches(thread_id,prompt,remaining,state,reason,updated_at,authorization) VALUES(?1,?2,?3,'Monitoring',NULL,?4,1)", params![config.thread_id, config.prompt, config.remaining, now]).map_err(db_err)?;
            }
            Some("Monitoring") | Some("WaitingQuota") | Some("ReadyToResume") => {
                self.db.execute("UPDATE watches SET prompt=?2,remaining=?3,updated_at=?4 WHERE thread_id=?1", params![config.thread_id, config.prompt, config.remaining, now]).map_err(db_err)?;
            }
            _ => {
                self.db.execute("UPDATE watches SET prompt=?2,remaining=?3,updated_at=?4 WHERE thread_id=?1", params![config.thread_id, config.prompt, config.remaining, now]).map_err(db_err)?;
            }
        }
        self.observe(observation)
    }

    pub fn action(
        &self,
        thread_id: &str,
        action: &str,
        allow_immediate: bool,
    ) -> Result<(), String> {
        let old: Option<(String, Option<u32>)> = self
            .db
            .query_row(
                "SELECT state,remaining FROM watches WHERE thread_id=?1",
                params![thread_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db_err)?;
        if old.is_none() {
            return Err("Watch not found".into());
        }
        match action {
            "pause" => self.set_watch(thread_id, "PausedByUser", "Paused by user", false),
            "stop" => self.set_watch(thread_id, "Stopped", "Stopped by user", false),
            "delete" => self.delete_watch(thread_id),
            "enable" => {
                if matches!(old, Some((ref state, Some(0))) if state == "Completed") {
                    return Err("Completed watch has no remaining resumes; configure a positive count first".into());
                }
                let observation = self.current_observation()?;
                require_session(&observation, thread_id)?;
                require_compatible(&observation.diagnostic)?;
                if self.would_send_now(&observation, thread_id)? && !allow_immediate {
                    let prompt: String = self
                        .db
                        .query_row(
                            "SELECT prompt FROM watches WHERE thread_id=?1",
                            params![thread_id],
                            |r| r.get(0),
                        )
                        .map_err(db_err)?;
                    return Err(format!(
                        "This watch is ready to send immediately. Confirm prompt preview: {}",
                        preview(&prompt)
                    ));
                }
                self.set_watch(thread_id, "Monitoring", "Enabled by user", true)?;
                self.observe(observation)
            }
            "reconcile" => self.observe(self.current_observation()?),
            _ => Err("Unsupported watch action".into()),
        }
    }

    pub fn pause_all(&self) -> Result<(), String> {
        let tx = self.db.unchecked_transaction().map_err(db_err)?;
        let paused = message_from_text("Paused by user");
        let changed=tx.execute("UPDATE watches SET state='PausedByUser',reason='Paused by user',reason_code=?2,reason_params=?3,authorization=0,updated_at=?1 WHERE state NOT IN ('Stopped','Completed')", params![now(), &paused.code, serde_json::to_string(&paused.params).map_err(json_err)?]).map_err(db_err)?;
        if changed > 0 {
            Self::history_tx(
                &tx,
                None,
                "pause_all",
                "用户已暂停全部 Watch；不会中断已提交的 Codex 任务",
            )?;
        }
        tx.commit().map_err(db_err)
    }

    pub fn clear_history(&self) -> Result<(), String> {
        self.db.execute("DELETE FROM history", []).map_err(db_err)?;
        Ok(())
    }

    pub fn prepare(&self) -> Result<Option<Submission>, String> {
        let observation = self.current_observation()?;
        require_compatible(&observation.diagnostic)?;
        if observation.quota_stale {
            return Ok(None);
        }
        let poll_seconds = self.settings()?.poll_seconds;
        let tx =
            Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate).map_err(db_err)?;
        let mut stmt = tx.prepare("SELECT thread_id,prompt,source_event_id,source_order,source_quota FROM watches WHERE state='ReadyToResume' AND authorization=1 ORDER BY updated_at LIMIT 1").map_err(db_err)?;
        let candidate: Option<(String, String, String, i64, String)> = stmt
            .query_row([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()
            .map_err(db_err)?;
        drop(stmt);
        let Some((thread_id, prompt, event_id, event_order, source_quota)) = candidate else {
            tx.commit().map_err(db_err)?;
            return Ok(None);
        };
        let session = require_session(&observation, &thread_id)?;
        require_fresh(&observation, poll_seconds)?;
        let (event, historic_quota, quota) = eligibility(session, &observation.quota)
            .ok_or_else(|| "Resume eligibility is no longer valid".to_string())?;
        if event.event_id != event_id
            || event.order as i64 != event_order
            || serde_json::to_string(historic_quota).map_err(json_err)? != source_quota
            || quota.weekly_used >= 100.0
            || quota.five_hour_used >= 100.0
        {
            return Err("Resume eligibility is no longer valid".into());
        }
        if Self::source_invalidated_tx(&tx, &thread_id, &event_id)? {
            tx.execute("UPDATE watches SET state='Monitoring',reason='Source interruption was invalidated',authorization=1,updated_at=?2 WHERE thread_id=?1", params![thread_id,now()]).map_err(db_err)?;
            tx.commit().map_err(db_err)?;
            return Ok(None);
        }
        let already_consumed: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM attempts WHERE thread_id=?1 AND source_event_id=?2)",
                params![thread_id, event_id],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if already_consumed {
            tx.execute(
                "UPDATE watches SET state='NeedsAttention',reason='Source interruption was already consumed; no retry',authorization=0,updated_at=?2 WHERE thread_id=?1",
                params![thread_id, now()],
            )
            .map_err(db_err)?;
            tx.commit().map_err(db_err)?;
            return Ok(None);
        }
        let attempt_id = Uuid::new_v4().to_string();
        let baseline_order = session.observed_order;
        if baseline_order < event.order {
            return Err("会话记录基线无法确认，未发送".into());
        }
        tx.execute("INSERT INTO attempts(id,thread_id,prompt_hash,source_event_id,source_order,status,counted,created_at,baseline_order) VALUES(?1,?2,?3,?4,?5,'Inflight',0,?6,?7)", params![attempt_id,thread_id,prompt_hash(&prompt),event_id,event_order,now(),baseline_order as i64]).map_err(db_err)?;
        tx.execute("UPDATE watches SET state='Resuming',reason='Submission recorded before IPC',authorization=0,updated_at=?2 WHERE thread_id=?1", params![thread_id,now()]).map_err(db_err)?;
        Self::history_tx(
            &tx,
            Some(&thread_id),
            "submission_prepared",
            "Resume submission recorded",
        )?;
        tx.commit().map_err(db_err)?;
        Ok(Some(Submission {
            attempt_id,
            thread_id,
            prompt,
            source_event_id: event_id,
            baseline_order,
        }))
    }

    pub fn delivered(&self, attempt_id: &str, delivery: Delivery) -> Result<(), String> {
        let (thread_id, status): (String, String) = self
            .db
            .query_row(
                "SELECT thread_id,status FROM attempts WHERE id=?1",
                params![attempt_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(db_err)?;
        if status != "Inflight" {
            return Ok(());
        }
        let (next, detail, watch_state, accepted_turn_id) = match delivery {
            Delivery::Accepted { turn_id } => (
                "Accepted",
                "Submission accepted; awaiting execution proof".to_string(),
                "Resuming",
                Some(turn_id),
            ),
            Delivery::Submitted => (
                "Submitted",
                "Desktop 已接受请求，正在核对执行 turn；确认后扣次，不重发".into(),
                "Resuming",
                None,
            ),
            Delivery::Rejected(message) => ("Rejected", redact(&message), "NeedsAttention", None),
            Delivery::Unknown(message) => ("Unknown", redact(&message), "NeedsAttention", None),
        };
        let tx = self.db.unchecked_transaction().map_err(db_err)?;
        tx.execute(
            "UPDATE attempts SET status=?2,detail=?3,accepted_turn_id=?4 WHERE id=?1",
            params![attempt_id, next, detail, accepted_turn_id],
        )
        .map_err(db_err)?;
        tx.execute("UPDATE watches SET state=?2,reason=?3,authorization=0,updated_at=?4 WHERE thread_id=?1 AND state='Resuming'", params![thread_id,watch_state, if next == "Accepted" { "Awaiting execution proof" } else { detail.as_str() }, now()]).map_err(db_err)?;
        Self::history_tx(&tx, Some(&thread_id), "delivery", detail.as_str())?;
        tx.commit().map_err(db_err)
    }

    fn current_observation(&self) -> Result<Observation, String> {
        let value: String = self
            .db
            .query_row("SELECT value FROM observation WHERE id=1", [], |r| r.get(0))
            .optional()
            .map_err(db_err)?
            .ok_or_else(|| "No observation is available".to_string())?;
        serde_json::from_str(&value).map_err(json_err)
    }

    fn set_watch(
        &self,
        thread_id: &str,
        state: &str,
        reason: &str,
        authorized: bool,
    ) -> Result<(), String> {
        let tx = self.db.unchecked_transaction().map_err(db_err)?;
        let display = message_from_text(reason);
        tx.execute("UPDATE watches SET state=?2,reason=?3,reason_code=?4,reason_params=?5,authorization=?6,updated_at=?7 WHERE thread_id=?1", params![thread_id,state,reason,&display.code,serde_json::to_string(&display.params).map_err(json_err)?,authorized as i32,now()]).map_err(db_err)?;
        Self::history_tx(&tx, Some(thread_id), "watch_action", reason)?;
        tx.commit().map_err(db_err)
    }

    fn delete_watch(&self, thread_id: &str) -> Result<(), String> {
        let tx = self.db.unchecked_transaction().map_err(db_err)?;
        tx.execute("DELETE FROM watches WHERE thread_id=?1", params![thread_id])
            .map_err(db_err)?;
        Self::history_tx(
            &tx,
            Some(thread_id),
            "watch_deleted",
            "Watch 已删除；历史事件和 Attempt 记录保留",
        )?;
        tx.commit().map_err(db_err)
    }

    fn would_send_now(&self, observation: &Observation, thread_id: &str) -> Result<bool, String> {
        if observation.quota_stale {
            return Ok(false);
        }
        let session = require_session(observation, thread_id)?;
        Ok(eligibility(session, &observation.quota)
            .map(|(_, _, quota)| quota.five_hour_used < 100.0 && quota.weekly_used < 100.0)
            .unwrap_or(false))
    }

    fn refresh_watches_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        observation: &Observation,
    ) -> Result<(), String> {
        let mut stmt = tx.prepare("SELECT thread_id,state,authorization,source_event_id FROM watches WHERE state NOT IN ('Stopped','Completed')").map_err(db_err)?;
        let rows: Result<Vec<(String, String, bool, Option<String>)>, _> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get::<_, i32>(2)? != 0, r.get(3)?))
            })
            .map_err(db_err)?
            .collect();
        drop(stmt);
        for (thread_id, state, authorized, previous_source) in rows.map_err(db_err)? {
            let session = match observation
                .sessions
                .iter()
                .find(|s| s.thread_id == thread_id)
            {
                Some(s) => s,
                None if observation.quota_stale => continue,
                None => {
                    self.update_state_tx(
                        tx,
                        &thread_id,
                        "NeedsAttention",
                        "Session unavailable",
                        false,
                    )?;
                    continue;
                }
            };
            if let Some(source_event_id) = previous_source.as_deref() {
                if session.latest.as_ref().map(|event| event.event_id.as_str())
                    != Some(source_event_id)
                {
                    Self::invalidate_source_tx(
                        tx,
                        &thread_id,
                        source_event_id,
                        "A newer session event invalidated this interruption",
                    )?;
                }
            }
            if !authorized || state == "NeedsAttention" || state == "Resuming" {
                continue;
            }
            if observation.quota_stale {
                // Keep authorization while retrying, but invalidate local sources above.
                tx.execute("UPDATE watches SET reason='Desktop 读取暂时失败，跳过本轮；保留授权，暂不发送' WHERE thread_id=?1", params![thread_id]).map_err(db_err)?;
                continue;
            }
            if let Some((event, historic_quota, quota)) = eligibility(session, &observation.quota) {
                if quota.weekly_used >= 100.0 {
                    Self::invalidate_source_tx(
                        tx,
                        &thread_id,
                        &event.event_id,
                        "Weekly quota exhausted",
                    )?;
                    self.update_state_tx(
                        tx,
                        &thread_id,
                        "Monitoring",
                        "Weekly quota exhausted",
                        true,
                    )?;
                    continue;
                }
                if Self::source_invalidated_tx(tx, &thread_id, &event.event_id)? {
                    self.update_state_tx(
                        tx,
                        &thread_id,
                        "Monitoring",
                        "Source interruption was invalidated",
                        true,
                    )?;
                    continue;
                }
                let next = if quota.five_hour_used >= 100.0 {
                    "WaitingQuota"
                } else {
                    "ReadyToResume"
                };
                tx.execute("UPDATE watches SET state=?2,reason=?3,source_event_id=?4,source_order=?5,source_quota=?6,updated_at=?7 WHERE thread_id=?1", params![thread_id,next, if next == "WaitingQuota" { "Waiting for 5h quota" } else { "Eligible quota restored" },event.event_id,event.order as i64,serde_json::to_string(historic_quota).map_err(json_err)?,now()]).map_err(db_err)?;
            } else {
                self.update_state_tx(
                    tx,
                    &thread_id,
                    "Monitoring",
                    "No current 5h interruption eligibility",
                    true,
                )?;
            }
        }
        Ok(())
    }

    fn resolve_submitted_tx(
        &self,
        tx: &Transaction<'_>,
        observation: &Observation,
    ) -> Result<(), String> {
        let mut stmt = tx.prepare("SELECT id,thread_id,prompt_hash,baseline_order,created_at,status FROM attempts WHERE status IN ('Submitted','Accepted') AND counted=0").map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?
                        .and_then(|v| u64::try_from(v).ok()),
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .map_err(db_err)?;
        let pending = rows.collect::<Result<Vec<_>, _>>().map_err(db_err)?;
        drop(stmt);
        for (id, thread, hash, baseline, created, status) in pending {
            let timed_out = DateTime::parse_from_rfc3339(&created)
                .map(|t| Utc::now().signed_duration_since(t) >= Duration::seconds(120))
                .unwrap_or(true);
            let session = observation.sessions.iter().find(|s| s.thread_id == thread);
            if status == "Accepted"
                && !timed_out
                && !session.is_some_and(|s| s.confirmation_issue.is_some())
            {
                continue;
            }
            let association = match (session, baseline) {
                (Some(session), Some(baseline)) => {
                    associate_desktop_input(session, baseline, &hash)
                }
                (_, None) => Err("发送前记录基线缺失，无法确认"),
                _ => Ok(None),
            };
            match association {
                Ok(Some(turn)) if turn.executed && !timed_out => {
                    tx.execute("UPDATE attempts SET status='Accepted',accepted_turn_id=?2,detail='Desktop input item and execution turn correlated' WHERE id=?1 AND status='Submitted'",params![id,turn.turn_id]).map_err(db_err)?;
                    Self::history_tx(
                        tx,
                        Some(&thread),
                        "turn_identified",
                        "已关联 Desktop 输入记录和执行 turn，正在确认记账",
                    )?;
                    continue;
                }
                _ => {}
            }
            let reason = match association {
                Err(reason) => Some(
                    session
                        .and_then(|s| s.confirmation_issue.as_deref())
                        .unwrap_or(reason),
                ),
                _ if timed_out => Some(
                    "Desktop 已接受请求，但两分钟内未取得唯一执行证据；不扣次数、不重发，需要核对",
                ),
                _ => None,
            };
            if let Some(reason) = reason {
                tx.execute("UPDATE attempts SET status='Unknown',accepted_turn_id=NULL,detail=?2 WHERE id=?1 AND status IN ('Submitted','Accepted')",params![id,reason]).map_err(db_err)?;
                tx.execute("UPDATE watches SET state='NeedsAttention',reason=?2,authorization=0,updated_at=?3 WHERE thread_id=?1 AND state='Resuming'",params![thread,reason,now()]).map_err(db_err)?;
                Self::history_tx(tx, Some(&thread), "delivery_unresolved", reason)?;
            }
        }
        Ok(())
    }

    fn confirm_attempts_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        observation: &Observation,
    ) -> Result<(), String> {
        self.resolve_submitted_tx(tx, observation)?;
        let mut stmt = tx.prepare("SELECT id,thread_id,prompt_hash,source_order,status,accepted_turn_id FROM attempts WHERE counted=0 AND status='Accepted' AND accepted_turn_id IS NOT NULL").map_err(db_err)?;
        let attempts: Result<Vec<(String, String, String, i64, String, String)>, _> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .map_err(db_err)?
            .collect();
        drop(stmt);
        for (id, thread_id, hash, source_order, attempt_status, accepted_turn_id) in
            attempts.map_err(db_err)?
        {
            let proven = observation
                .sessions
                .iter()
                .find(|s| s.thread_id == thread_id)
                .map(|s| {
                    !s.confirmation_incomplete
                        && s.confirmation_issue.is_none()
                        && s.turns.iter().any(|t| {
                            t.order > source_order as u64
                                && (t.prompt_hash.as_deref() == Some(&hash)
                                    || t.desktop_inputs.iter().any(|input| {
                                        input.prompt_hash == hash
                                            && input.source_thread_id == thread_id
                                    }))
                                && t.executed
                                && t.turn_id == accepted_turn_id
                        })
                })
                .unwrap_or(false);
            if !proven {
                continue;
            }
            let manual_intervention = observation
                .sessions
                .iter()
                .find(|s| s.thread_id == thread_id)
                .is_some_and(|s| {
                    s.turns
                        .iter()
                        .find(|t| t.turn_id == accepted_turn_id)
                        .is_some_and(|t| {
                            s.last_user_order > t.order
                                || s.turns
                                    .iter()
                                    .any(|later| later.order > t.order && later.has_user_input)
                        })
                });
            if manual_intervention {
                tx.execute("UPDATE watches SET state='NeedsAttention',reason='本次resume已执行；随后出现用户输入，后续自动发送已暂停',authorization=0,updated_at=?2 WHERE thread_id=?1 AND state='Resuming'", params![thread_id,now()]).map_err(db_err)?;
            }
            let watch: Option<(Option<u32>, String, bool, Option<String>)> = tx
                .query_row(
                    "SELECT remaining,state,authorization,reason FROM watches WHERE thread_id=?1",
                    params![thread_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i32>(2)? != 0, r.get(3)?)),
                )
                .optional()
                .map_err(db_err)?;
            let Some((remaining, current_state, current_authorization, current_reason)) = watch
            else {
                continue;
            };
            let (state, new_remaining, reason) = match remaining {
                Some(1) => ("Completed", Some(0), "Resume count exhausted"),
                Some(n) => ("Monitoring", Some(n - 1), "Execution proof observed"),
                None => ("Monitoring", None, "Execution proof observed"),
            };
            tx.execute("UPDATE attempts SET counted=1,status='Confirmed',detail='Observed matching execution evidence' WHERE id=?1 AND counted=0", params![id]).map_err(db_err)?;
            if remaining == Some(1) {
                tx.execute("UPDATE watches SET remaining=0,state='Completed',reason=?2,authorization=0,updated_at=?3 WHERE thread_id=?1", params![thread_id,reason,now()]).map_err(db_err)?;
            } else if current_state == "Resuming" && attempt_status == "Accepted" {
                tx.execute("UPDATE watches SET remaining=?2,state=?3,reason=?4,authorization=1,updated_at=?5 WHERE thread_id=?1", params![thread_id,new_remaining,state,reason,now()]).map_err(db_err)?;
            } else {
                tx.execute("UPDATE watches SET remaining=?2,state=?3,reason=?4,authorization=?5,updated_at=?6 WHERE thread_id=?1", params![thread_id,new_remaining,current_state,current_reason,current_authorization as i32,now()]).map_err(db_err)?;
            }
            Self::history_tx(
                tx,
                Some(&thread_id),
                "execution_confirmed",
                "Observed matching execution evidence; count updated",
            )?;
        }
        Ok(())
    }

    fn update_state_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        thread_id: &str,
        state: &str,
        reason: &str,
        authorized: bool,
    ) -> Result<(), String> {
        tx.execute("UPDATE watches SET state=?2,reason=?3,authorization=?4,updated_at=?5 WHERE thread_id=?1", params![thread_id,state,reason,authorized as i32,now()]).map_err(db_err)?;
        Ok(())
    }

    fn invalidate_source_tx(
        tx: &rusqlite::Transaction<'_>,
        thread_id: &str,
        event_id: &str,
        reason: &str,
    ) -> Result<(), String> {
        tx.execute(
            "INSERT OR IGNORE INTO invalidated_sources(thread_id,event_id,reason,invalidated_at) VALUES(?1,?2,?3,?4)",
            params![thread_id, event_id, reason, now()],
        )
        .map_err(db_err)?;
        Ok(())
    }

    fn source_invalidated_tx(
        tx: &rusqlite::Transaction<'_>,
        thread_id: &str,
        event_id: &str,
    ) -> Result<bool, String> {
        tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM invalidated_sources WHERE thread_id=?1 AND event_id=?2)",
            params![thread_id, event_id],
            |r| r.get(0),
        )
        .map_err(db_err)
    }

    fn history_tx(
        tx: &rusqlite::Transaction<'_>,
        thread_id: Option<&str>,
        kind: &str,
        message: &str,
    ) -> Result<(), String> {
        let display = message_from_text(message);
        tx.execute(
            "INSERT INTO history(thread_id,time,kind,message,message_code,message_params) VALUES(?1,?2,?3,?4,?5,?6)",
            params![thread_id, now(), kind, message, display.code, serde_json::to_string(&display.params).map_err(json_err)?],
        )
        .map_err(db_err)?;
        Ok(())
    }

    fn diagnostic_history_tx(
        tx: &rusqlite::Transaction<'_>,
        kind: &str,
        message: &str,
    ) -> Result<(), String> {
        let current = message_from_text(message);
        let last: Option<(String, Option<String>, Option<String>, String)> = tx
            .query_row(
                "SELECT kind,message_code,message_params,message FROM history ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(db_err)?;
        if last.as_ref().is_some_and(|entry| {
            entry.0 == kind
                && entry.1.is_some()
                && crate::i18n::identity(&Self::stored_message(
                    entry.1.clone(),
                    entry.2.clone(),
                    &entry.3,
                )) == crate::i18n::identity(&current)
        }) {
            return Ok(());
        }
        Self::history_tx(tx, None, kind, message)
    }

    fn read_watches(&self) -> Result<Vec<Value>, String> {
        let mut stmt = self.db.prepare("SELECT thread_id,prompt,remaining,state,reason,updated_at,reason_code,reason_params FROM watches ORDER BY updated_at DESC").map_err(db_err)?;
        let rows = stmt.query_map([], |r| {
            let raw = r.get::<_, Option<String>>(4)?;
            let code = r.get(6)?;
            let params = r.get(7)?;
            let display = raw.as_deref().map(|text| Self::stored_message(code, params, text));
            Ok(json!({"threadId":r.get::<_,String>(0)?,"prompt":r.get::<_,String>(1)?,"remaining":r.get::<_,Option<u32>>(2)?,"state":r.get::<_,String>(3)?,"reason":raw,"reasonMessage":display,"updatedAt":r.get::<_,String>(5)?}))
        }).map_err(db_err)?.collect::<Result<Vec<_>,_>>().map_err(db_err)?;
        Ok(rows)
    }

    fn stored_message(code: Option<String>, params: Option<String>, fallback: &str) -> Message {
        let Some(code) = code.filter(|code| !code.is_empty()) else {
            let mut legacy = crate::i18n::message("backend.external", fallback);
            legacy.params.insert(
                "fallback".into(),
                serde_json::Value::String(fallback.into()),
            );
            return legacy;
        };
        let Some(params) = params.and_then(|value| serde_json::from_str(&value).ok()) else {
            return crate::i18n::external_message(fallback);
        };
        let mut message = crate::i18n::message(&code, fallback);
        message.params = params;
        message
    }
    fn read_attempts(&self) -> Result<Vec<Value>, String> {
        let mut stmt = self.db.prepare("SELECT id,thread_id,status,counted,created_at,detail,detail_code,detail_params FROM attempts ORDER BY created_at DESC").map_err(db_err)?;
        let rows = stmt.query_map([], |r| {
            let raw: Option<String> = r.get(5)?;
            let code = r.get(6)?;
            let params = r.get(7)?;
            let display = raw.as_deref().map(|text| Self::stored_message(code, params, text));
            Ok(json!({"id":r.get::<_,String>(0)?,"threadId":r.get::<_,String>(1)?,"status":r.get::<_,String>(2)?,"counted":r.get::<_,i32>(3)? != 0,"createdAt":r.get::<_,String>(4)?,"detail":raw,"detailMessage":display}))
        }).map_err(db_err)?.collect::<Result<Vec<_>,_>>().map_err(db_err)?;
        Ok(rows)
    }
}

fn associate_desktop_input<'a>(
    session: &'a Session,
    baseline: u64,
    hash: &str,
) -> Result<Option<&'a crate::contracts::TurnEvidence>, &'static str> {
    if session.confirmation_issue.is_some() {
        return Err("执行证据未完整读取或日志身份变化；不扣次数、不重发，需要核对");
    }
    if session.confirmation_incomplete {
        return Ok(None);
    }
    let newer: Vec<_> = session
        .turns
        .iter()
        .filter(|t| t.order > baseline)
        .collect();
    if newer.iter().any(|turn| turn.invalid_desktop_input) {
        return Err("发送后出现 Desktop 输入身份不完整；不扣次数、不重发");
    }
    let candidates: Vec<_> = newer
        .iter()
        .copied()
        .filter(|turn| {
            turn.desktop_inputs.iter().any(|input| {
                input.order > baseline
                    && input.source_thread_id == session.thread_id
                    && input.prompt_hash == hash
            })
        })
        .collect();
    if candidates.len() > 1 {
        return Err("发送后出现多个匹配本次提交的执行turn，无法唯一关联；不扣次数、不重发");
    }
    let Some(turn) = candidates.first().copied() else {
        if newer
            .iter()
            .any(|t| t.has_user_input || !t.desktop_inputs.is_empty())
        {
            return Err("尚无匹配本次提交的输入，已出现手动输入或不匹配输入；不扣次数、不重发");
        }
        return Ok(None);
    };
    if turn.has_user_input || turn.invalid_desktop_input {
        return Err("发送后出现手动输入或 Desktop 输入身份不完整；不扣次数、不重发");
    }
    if turn.desktop_inputs.len() > 1 {
        return Err("执行 turn 包含多条 Desktop 输入，无法唯一关联；不扣次数、不重发");
    }
    let Some(input) = turn.desktop_inputs.first() else {
        return Ok(None);
    };
    if input.order <= baseline
        || input.source_thread_id != session.thread_id
        || input.prompt_hash != hash
    {
        return Err("Desktop 输入与本次提交的来源或原文不匹配；不扣次数、不重发");
    }
    Ok(Some(turn))
}

fn eligibility<'a>(
    session: &'a Session,
    live: &'a Option<Quota>,
) -> Option<(&'a crate::contracts::TerminalEvent, &'a Quota, &'a Quota)> {
    let event = session.latest.as_ref()?;
    let historic = event.quota.as_ref()?;
    let live = live.as_ref()?;
    if event.kind != "usage_limit_exceeded"
        || historic.bucket != "codex"
        || historic.five_hour_used < 100.0
        || historic.weekly_used >= 100.0
        || live.bucket != "codex"
        || live.scope != historic.scope
    {
        return None;
    }
    if session.turns.iter().any(|turn| turn.order > event.order) {
        return None;
    }
    Some((event, historic, live))
}

fn require_session<'a>(
    observation: &'a Observation,
    thread_id: &str,
) -> Result<&'a Session, String> {
    observation
        .sessions
        .iter()
        .find(|s| s.thread_id == thread_id)
        .ok_or_else(|| "Selected session is unavailable".into())
}
fn require_compatible(diagnostic: &Diagnostic) -> Result<(), String> {
    if diagnostic.compatible {
        Ok(())
    } else {
        Err(format!(
            "Desktop compatibility is not confirmed: {}",
            redact(&diagnostic.reason)
        ))
    }
}
fn require_fresh(observation: &Observation, poll: u64) -> Result<(), String> {
    let max_age = std::cmp::max(poll.saturating_mul(2), 120) as i64;
    for timestamp in std::iter::once(observation.diagnostic.checked_at.as_str())
        .chain(observation.quota.iter().map(|q| q.captured_at.as_str()))
    {
        let parsed = DateTime::parse_from_rfc3339(timestamp)
            .map_err(|_| "Observation timestamp is invalid".to_string())?
            .with_timezone(&Utc);
        if Utc::now() - parsed > Duration::seconds(max_age) {
            return Err("Observation is stale; no resume submission prepared".into());
        }
    }
    Ok(())
}
fn validate_config(c: &WatchConfig) -> Result<(), String> {
    if c.thread_id.trim().is_empty() || c.prompt.trim().is_empty() {
        return Err("threadId and prompt are required".into());
    }
    if c.prompt.as_bytes().len() > 64_000 {
        return Err("Prompt must be at most 64000 bytes".into());
    }
    if c.remaining.is_some_and(|n| n == 0 || n > 1_000_000) {
        return Err("remaining must be continuous or 1..1000000".into());
    }
    Ok(())
}
fn prompt_hash(prompt: &str) -> String {
    format!("{:x}", Sha256::digest(prompt.as_bytes()))
}
fn preview(prompt: &str) -> String {
    let mut p: String = prompt.chars().take(120).collect();
    if prompt.chars().count() > 120 {
        p.push('…');
    }
    p
}
fn redact(text: &str) -> String {
    text.chars().take(240).collect()
}
fn now() -> String {
    Utc::now().to_rfc3339()
}
fn db_err(e: rusqlite::Error) -> String {
    format!("SQLite: {e}")
}
fn json_err(e: serde_json::Error) -> String {
    format!("JSON: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{
        Diagnostic, Observation, Quota, Session, TerminalEvent, TurnEvidence, WatchConfig,
    };
    use tempfile::NamedTempFile;

    fn desktop_proof() -> Observation {
        let mut o = proof();
        o.sessions[0].latest = None;
        let turn = &mut o.sessions[0].turns[0];
        turn.prompt_hash = None;
        turn.desktop_inputs
            .push(crate::contracts::DesktopInputEvidence {
                item_id: "fco_test".into(),
                order: 3,
                source_thread_id: "t".into(),
                prompt_hash: prompt_hash("resume"),
            });
        o
    }

    fn submitted() -> (NamedTempFile, Engine, Submission) {
        let (f, e) = engine();
        e.observe(obs(0.0, 10.0)).unwrap();
        e.configure(config(), true).unwrap();
        let s = e.prepare().unwrap().unwrap();
        e.delivered(&s.attempt_id, Delivery::Submitted).unwrap();
        (f, e, s)
    }

    #[test]
    fn resume_then_manual_turn_counts_only_resume_and_revokes_future_authorization() {
        let (_f, e, _) = submitted();
        let mut o = desktop_proof();
        o.sessions[0].turns.push(TurnEvidence {
            turn_id: "manual".into(),
            order: 4,
            has_user_input: true,
            executed: true,
            ..Default::default()
        });
        o.sessions[0].last_user_order = 5;
        e.observe(o.clone()).unwrap();
        e.observe(o).unwrap();
        assert_eq!(e.read_watches().unwrap()[0]["remaining"], 1);
        assert_eq!(e.read_watches().unwrap()[0]["state"], "NeedsAttention");
        assert_eq!(e.read_attempts().unwrap()[0]["status"], "Confirmed");
        assert!(e.prepare().unwrap().is_none());
    }

    #[test]
    fn incomplete_scan_waits_and_read_limit_failure_stops_without_counting() {
        let (_f, e, _) = submitted();
        let mut o = desktop_proof();
        o.sessions[0].confirmation_incomplete = true;
        e.observe(o.clone()).unwrap();
        assert_eq!(e.read_watches().unwrap()[0]["remaining"], 2);
        assert!(e.confirmation_pending().unwrap());
        o.sessions[0].confirmation_issue = Some("执行证据超过16MiB确认读取上限".into());
        e.observe(o).unwrap();
        assert_eq!(e.read_attempts().unwrap()[0]["status"], "Unknown");
        assert_eq!(e.read_watches().unwrap()[0]["remaining"], 2);
        assert!(e.read_watches().unwrap()[0]["reason"]
            .as_str()
            .unwrap()
            .contains("16MiB"));
    }

    #[test]
    fn later_pending_manual_message_does_not_erase_execution_or_restart_paused_watch() {
        let (_f, e, _) = submitted();
        e.pause_all().unwrap();
        let mut o = desktop_proof();
        o.sessions[0].last_user_order = 4;
        e.observe(o).unwrap();
        assert_eq!(e.read_watches().unwrap()[0]["remaining"], 1);
        assert_eq!(e.read_watches().unwrap()[0]["state"], "PausedByUser");
        assert!(e.prepare().unwrap().is_none());
    }

    #[test]
    fn submitted_desktop_input_waits_for_execution_then_counts_once() {
        let (_f, e, s) = submitted();
        assert!(e.confirmation_pending().unwrap());
        let mut o = desktop_proof();
        o.sessions[0].turns[0].executed = false;
        e.observe(o.clone()).unwrap();
        assert_eq!(e.read_watches().unwrap()[0]["remaining"], 2);
        assert_eq!(e.read_watches().unwrap()[0]["state"], "Resuming");
        assert!(e.prepare().unwrap().is_none());
        o.sessions[0].turns[0].executed = true;
        e.observe(o.clone()).unwrap();
        e.observe(o).unwrap();
        // A duplicate callback must not erase confirmed identity or count twice.
        e.delivered(&s.attempt_id, Delivery::Unknown("late callback".into()))
            .unwrap();
        assert_eq!(e.read_watches().unwrap()[0]["remaining"], 1);
        assert_eq!(e.read_watches().unwrap()[0]["state"], "Monitoring");
        assert!(!e.confirmation_pending().unwrap());
        assert_eq!(e.read_attempts().unwrap()[0]["status"], "Confirmed");
    }

    #[test]
    fn submitted_rejects_manual_ambiguous_wrong_or_incomplete_identity() {
        for case in 0..7 {
            let (_f, e, _) = submitted();
            let mut o = desktop_proof();
            match case {
                0 => o.sessions[0].turns[0].has_user_input = true,
                1 => {
                    let input = o.sessions[0].turns[0].desktop_inputs[0].clone();
                    o.sessions[0].turns[0].desktop_inputs.push(input);
                }
                2 => {
                    let turn = o.sessions[0].turns[0].clone();
                    o.sessions[0].turns.push(turn);
                }
                3 => o.sessions[0].turns[0].desktop_inputs[0].source_thread_id = "other".into(),
                4 => o.sessions[0].turns[0].desktop_inputs[0].prompt_hash = prompt_hash("other"),
                5 => o.sessions[0].turns[0].invalid_desktop_input = true,
                _ => o.sessions[0].turns[0].desktop_inputs[0].order = 1,
            }
            e.observe(o).unwrap();
            assert_eq!(e.read_watches().unwrap()[0]["remaining"], 2, "case {case}");
            assert_eq!(
                e.read_watches().unwrap()[0]["state"],
                "NeedsAttention",
                "case {case}"
            );
            assert_eq!(e.read_attempts().unwrap()[0]["status"], "Unknown");
            e.observe(desktop_proof()).unwrap();
            assert_eq!(e.read_watches().unwrap()[0]["remaining"], 2);
            assert!(e.prepare().unwrap().is_none());
        }
    }

    #[test]
    fn submitted_timeout_never_late_confirms_or_resends() {
        let (_f, e, s) = submitted();
        let past = (Utc::now() - Duration::seconds(121)).to_rfc3339();
        e.db.execute(
            "UPDATE attempts SET created_at=?2 WHERE id=?1",
            params![s.attempt_id, past],
        )
        .unwrap();
        e.observe(obs(0.0, 10.0)).unwrap();
        assert_eq!(e.read_watches().unwrap()[0]["state"], "NeedsAttention");
        e.observe(desktop_proof()).unwrap();
        assert_eq!(e.read_watches().unwrap()[0]["remaining"], 2);
        assert!(e.prepare().unwrap().is_none());
        let (_f, e, s) = submitted();
        let past = (Utc::now() - Duration::seconds(121)).to_rfc3339();
        e.db.execute(
            "UPDATE attempts SET created_at=?2 WHERE id=?1",
            params![s.attempt_id, past],
        )
        .unwrap();
        e.observe(desktop_proof()).unwrap();
        assert_eq!(e.read_watches().unwrap()[0]["remaining"], 2);
        assert_eq!(e.read_watches().unwrap()[0]["state"], "NeedsAttention");
    }

    #[test]
    fn compatibility_loss_ends_pending_confirmation_without_late_count() {
        let (_f, e, _) = submitted();
        let mut o = obs(0.0, 10.0);
        o.diagnostic.compatible = false;
        e.observe(o).unwrap();
        e.observe(desktop_proof()).unwrap();
        assert_eq!(e.read_watches().unwrap()[0]["remaining"], 2);
        assert_eq!(e.read_attempts().unwrap()[0]["status"], "Unknown");
    }

    #[test]
    fn accepted_explicit_turn_has_same_deadline_and_never_late_counts() {
        for proof_at_deadline in [false, true] {
            let (_f, e) = engine();
            e.observe(obs(0.0, 10.0)).unwrap();
            e.configure(config(), true).unwrap();
            let s = e.prepare().unwrap().unwrap();
            e.delivered(
                &s.attempt_id,
                Delivery::Accepted {
                    turn_id: "new".into(),
                },
            )
            .unwrap();
            assert!(e.confirmation_pending().unwrap());
            let past = (Utc::now() - Duration::seconds(121)).to_rfc3339();
            e.db.execute(
                "UPDATE attempts SET created_at=?2 WHERE id=?1",
                params![s.attempt_id, past],
            )
            .unwrap();
            e.observe(if proof_at_deadline {
                proof()
            } else {
                obs(0.0, 10.0)
            })
            .unwrap();
            e.observe(proof()).unwrap();
            assert_eq!(e.read_watches().unwrap()[0]["remaining"], 2);
            assert_eq!(e.read_watches().unwrap()[0]["state"], "NeedsAttention");
            assert_eq!(e.read_attempts().unwrap()[0]["status"], "Unknown");
            assert!(!e.confirmation_pending().unwrap());
            assert!(e.prepare().unwrap().is_none());
        }
    }

    #[test]
    fn migration_keeps_legacy_unknown_without_baseline_unconfirmed() {
        let f = NamedTempFile::new().unwrap();
        let db = Connection::open(f.path()).unwrap();
        db.execute_batch("CREATE TABLE attempts (id TEXT PRIMARY KEY,thread_id TEXT NOT NULL,prompt_hash TEXT NOT NULL,source_event_id TEXT NOT NULL,source_order INTEGER NOT NULL,status TEXT NOT NULL,counted INTEGER NOT NULL DEFAULT 0,created_at TEXT NOT NULL,detail TEXT NULL,accepted_turn_id TEXT NULL);").unwrap();
        db.execute("INSERT INTO attempts(id,thread_id,prompt_hash,source_event_id,source_order,status,created_at) VALUES('old','t',?1,'old-event',1,'Unknown',?2)",params![prompt_hash("resume"),now()]).unwrap();
        drop(db);
        let e = Engine::open(f.path()).unwrap();
        e.observe(desktop_proof()).unwrap();
        assert_eq!(e.read_attempts().unwrap()[0]["status"], "Unknown");
        assert_eq!(e.read_attempts().unwrap()[0]["counted"], false);
        let baseline: Option<i64> =
            e.db.query_row(
                "SELECT baseline_order FROM attempts WHERE id='old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(baseline.is_none());
    }

    #[test]
    fn submitted_restart_and_revocation_do_not_restore_authority() {
        for action in ["pause", "stop"] {
            let (_f, e, _) = submitted();
            e.action("t", action, false).unwrap();
            e.observe(desktop_proof()).unwrap();
            assert_eq!(e.read_watches().unwrap()[0]["remaining"], 1);
            assert_eq!(
                e.read_watches().unwrap()[0]["state"],
                if action == "pause" {
                    "PausedByUser"
                } else {
                    "Stopped"
                }
            );
            assert!(e.prepare().unwrap().is_none());
        }
        let (f, e, _) = submitted();
        drop(e);
        let e = Engine::open(f.path()).unwrap();
        e.observe(desktop_proof()).unwrap();
        assert_eq!(e.read_watches().unwrap()[0]["remaining"], 2);
        assert_eq!(e.read_watches().unwrap()[0]["state"], "PausedAfterRestart");
        assert!(e.prepare().unwrap().is_none());
    }

    fn quota(used: f64, weekly: f64) -> Quota {
        Quota {
            scope: "scope".into(),
            bucket: "codex".into(),
            captured_at: now(),
            five_hour_used: used,
            weekly_used: weekly,
            reset_at: None,
        }
    }
    fn obs(live: f64, weekly: f64) -> Observation {
        Observation {
            sessions: vec![Session {
                thread_id: "t".into(),
                title: "t".into(),
                cwd: "c".into(),
                updated_at: now(),
                source: "desktop".into(),
                status: "idle".into(),
                latest: Some(TerminalEvent {
                    event_id: "e".into(),
                    turn_id: "turn".into(),
                    order: 1,
                    timestamp: now(),
                    kind: "usage_limit_exceeded".into(),
                    quota: Some(quota(100.0, 10.0)),
                }),
                turns: vec![],
                observed_order: 1,
                ..Default::default()
            }],
            quota: Some(quota(live, weekly)),
            quota_stale: false,
            diagnostic: Diagnostic {
                compatible: true,
                reason: "ok".into(),
                reason_message: Some(message_from_text("ok")),
                stage: "probe".into(),
                checked_at: now(),
                desktop_version: None,
                runtime_version: None,
            },
        }
    }
    fn engine() -> (NamedTempFile, Engine) {
        let f = NamedTempFile::new().unwrap();
        let e = Engine::open(f.path()).unwrap();
        (f, e)
    }
    fn config() -> WatchConfig {
        WatchConfig {
            thread_id: "t".into(),
            prompt: "resume".into(),
            remaining: Some(2),
        }
    }
    fn proof() -> Observation {
        proof_with_turn("new")
    }
    fn proof_with_turn(turn_id: &str) -> Observation {
        let mut o = obs(0.0, 10.0);
        o.sessions[0].turns.push(TurnEvidence {
            turn_id: turn_id.into(),
            order: 2,
            started_at: now(),
            user_message_at: Some(now()),
            prompt_hash: Some(prompt_hash("resume")),
            executed: true,
            failed: false,
            ..Default::default()
        });
        o
    }
    #[test]
    fn waits_then_prepares_only_after_quota() {
        let (_f, e) = engine();
        e.observe(obs(100.0, 10.0)).unwrap();
        e.configure(config(), false).unwrap();
        assert_eq!(e.snapshot().unwrap()["watches"][0]["state"], "WaitingQuota");
        e.observe(obs(0.0, 10.0)).unwrap();
        assert!(e.prepare().unwrap().is_some());
    }
    #[test]
    fn quota_failure_retains_authorization_but_never_prepares_from_cache() {
        let (_f, e) = engine();
        e.observe(obs(100.0, 10.0)).unwrap();
        e.configure(config(), false).unwrap();
        let mut stale = obs(0.0, 10.0);
        stale.quota_stale = true;
        let captured = stale.quota.as_ref().unwrap().captured_at.clone();
        e.observe(stale.clone()).unwrap();
        e.observe(stale).unwrap();
        assert_eq!(e.read_watches().unwrap()[0]["state"], "WaitingQuota");
        assert_eq!(
            e.snapshot().unwrap()["observation"]["quota"]["capturedAt"],
            captured
        );
        assert!(e.prepare().unwrap().is_none());
        e.observe(obs(0.0, 10.0)).unwrap();
        assert!(e.prepare().unwrap().is_some());
    }
    #[test]
    fn stale_quota_keeps_ready_watch_blocked_and_invalidates_newer_input() {
        let (_f, e) = engine();
        e.observe(obs(0.0, 10.0)).unwrap();
        e.configure(config(), true).unwrap();
        let mut stale = obs(0.0, 10.0);
        stale.quota_stale = true;
        e.observe(stale.clone()).unwrap();
        assert_eq!(e.read_watches().unwrap()[0]["state"], "ReadyToResume");
        assert!(e.prepare().unwrap().is_none());
        stale.sessions[0].latest = None;
        e.observe(stale).unwrap();
        e.observe(obs(0.0, 10.0)).unwrap();
        assert!(e.prepare().unwrap().is_none());
    }
    #[test]
    fn temporary_desktop_read_failure_skips_without_revoking_authorization() {
        let (_f, e) = engine();
        e.observe(obs(0.0, 10.0)).unwrap();
        e.configure(config(), true).unwrap();
        let mut skipped = obs(0.0, 10.0);
        skipped.quota_stale = true;
        skipped.diagnostic.reason = "Desktop 读取暂时失败，跳过本轮；保留授权，暂不发送".into();
        skipped.diagnostic.stage = "quota_retry".into();
        e.observe(skipped.clone()).unwrap();
        assert!(e.prepare().unwrap().is_none());
        assert_eq!(e.read_watches().unwrap()[0]["state"], "ReadyToResume");
        // No cached quota/list is also a skipped read, not a hard failure.
        skipped.quota = None;
        skipped.sessions.clear();
        e.observe(skipped).unwrap();
        assert!(e.prepare().unwrap().is_none());
        assert_eq!(e.read_watches().unwrap()[0]["state"], "ReadyToResume");
        e.observe(obs(0.0, 10.0)).unwrap();
        assert!(e.prepare().unwrap().is_some());
    }
    #[test]
    fn pending_confirmation_continues_when_quota_is_stale() {
        let (_f, e, _) = submitted();
        let mut o = desktop_proof();
        o.quota_stale = true;
        e.observe(o.clone()).unwrap();
        e.observe(o).unwrap();
        assert_eq!(e.read_watches().unwrap()[0]["remaining"], 1);
        assert!(e.prepare().unwrap().is_none());
    }
    #[test]
    fn quota_retry_does_not_reauthorize_paused_or_hard_failed_watches() {
        let (_f, e) = engine();
        e.observe(obs(0.0, 10.0)).unwrap();
        e.configure(config(), true).unwrap();
        e.pause_all().unwrap();
        let mut stale = obs(0.0, 10.0);
        stale.quota_stale = true;
        e.observe(stale.clone()).unwrap();
        e.observe(obs(0.0, 10.0)).unwrap();
        assert!(e.prepare().unwrap().is_none());
        stale.diagnostic.compatible = false;
        e.observe(stale).unwrap();
        e.observe(obs(0.0, 10.0)).unwrap();
        assert_eq!(e.read_watches().unwrap()[0]["state"], "NeedsAttention");
        assert!(e.prepare().unwrap().is_none());
    }
    #[test]
    fn weekly_and_newer_turn_do_not_send() {
        let (_f, e) = engine();
        e.observe(obs(100.0, 100.0)).unwrap();
        e.configure(config(), false).unwrap();
        e.observe(obs(0.0, 100.0)).unwrap();
        assert!(e.prepare().unwrap().is_none());
        e.observe(obs(0.0, 10.0)).unwrap();
        assert!(e.prepare().unwrap().is_none());
        let mut o = obs(0.0, 10.0);
        o.sessions[0].turns.push(TurnEvidence {
            turn_id: "later".into(),
            order: 2,
            started_at: now(),
            user_message_at: None,
            prompt_hash: None,
            executed: true,
            failed: false,
            ..Default::default()
        });
        e.observe(o).unwrap();
        assert!(e.prepare().unwrap().is_none());
    }
    #[test]
    fn normal_and_stale_observations_do_not_send() {
        let (_f, e) = engine();
        let mut normal = obs(0.0, 10.0);
        normal.sessions[0].latest.as_mut().unwrap().kind = "completed".into();
        e.observe(normal).unwrap();
        e.configure(config(), false).unwrap();
        assert!(e.prepare().unwrap().is_none());
        let mut stale = obs(0.0, 10.0);
        stale.diagnostic.checked_at = (Utc::now() - Duration::seconds(121)).to_rfc3339();
        e.observe(stale).unwrap();
        e.action("t", "enable", true).unwrap();
        assert!(e.prepare().is_err());
    }
    #[test]
    fn unknown_delivery_never_retries_and_count_needs_proof() {
        let (_f, e) = engine();
        e.observe(obs(100.0, 10.0)).unwrap();
        e.configure(config(), false).unwrap();
        e.observe(obs(0.0, 10.0)).unwrap();
        let s = e.prepare().unwrap().unwrap();
        e.delivered(&s.attempt_id, Delivery::Unknown("timeout secret".into()))
            .unwrap();
        e.observe(proof()).unwrap();
        assert!(e.prepare().unwrap().is_none());
        assert_eq!(e.snapshot().unwrap()["watches"][0]["remaining"], 2);
    }
    #[test]
    fn execution_proof_counts_once_and_restart_pauses() {
        let (f, e) = engine();
        e.observe(obs(100.0, 10.0)).unwrap();
        e.configure(config(), false).unwrap();
        e.observe(obs(0.0, 10.0)).unwrap();
        let s = e.prepare().unwrap().unwrap();
        e.delivered(
            &s.attempt_id,
            Delivery::Accepted {
                turn_id: "new".into(),
            },
        )
        .unwrap();
        let mut o = obs(0.0, 10.0);
        o.sessions[0].turns.push(TurnEvidence {
            turn_id: "new".into(),
            order: 2,
            started_at: now(),
            user_message_at: Some(now()),
            prompt_hash: Some(prompt_hash("resume")),
            executed: true,
            failed: false,
            ..Default::default()
        });
        e.observe(o.clone()).unwrap();
        e.observe(o).unwrap();
        assert_eq!(e.snapshot().unwrap()["watches"][0]["remaining"], 1);
        drop(e);
        let reopened = Engine::open(f.path()).unwrap();
        assert_eq!(
            reopened.snapshot().unwrap()["watches"][0]["state"],
            "PausedAfterRestart"
        );
    }
    #[test]
    fn edit_paused_and_cancel_never_send() {
        let (_f, e) = engine();
        e.observe(obs(100.0, 10.0)).unwrap();
        e.configure(config(), false).unwrap();
        e.action("t", "pause", false).unwrap();
        e.configure(
            WatchConfig {
                prompt: "edited".into(),
                ..config()
            },
            false,
        )
        .unwrap();
        e.observe(obs(0.0, 10.0)).unwrap();
        assert!(e.prepare().unwrap().is_none());
        e.action("t", "stop", false).unwrap();
        assert!(e.prepare().unwrap().is_none());
    }
    #[test]
    fn old_session_activity_with_fresh_poll_can_resume() {
        let (_f, e) = engine();
        e.observe(obs(100.0, 10.0)).unwrap();
        e.configure(config(), false).unwrap();
        let mut available = obs(0.0, 10.0);
        available.sessions[0].updated_at = (Utc::now() - Duration::hours(5)).to_rfc3339();
        e.observe(available).unwrap();
        assert!(e.prepare().unwrap().is_some());
    }
    #[test]
    fn delayed_proof_counts_without_reviving_revoked_watches() {
        for action in ["pause", "stop"] {
            let (_f, e) = engine();
            e.observe(obs(100.0, 10.0)).unwrap();
            e.configure(config(), false).unwrap();
            e.observe(obs(0.0, 10.0)).unwrap();
            let s = e.prepare().unwrap().unwrap();
            e.delivered(
                &s.attempt_id,
                Delivery::Accepted {
                    turn_id: "new".into(),
                },
            )
            .unwrap();
            e.action("t", action, false).unwrap();
            e.observe(proof()).unwrap();
            let watch = &e.snapshot().unwrap()["watches"][0];
            assert_eq!(
                watch["state"],
                if action == "pause" {
                    "PausedByUser"
                } else {
                    "Stopped"
                }
            );
            assert_eq!(watch["remaining"], 1);
            assert!(e.prepare().unwrap().is_none());
        }
        let (file, e) = engine();
        e.observe(obs(100.0, 10.0)).unwrap();
        e.configure(config(), false).unwrap();
        e.observe(obs(0.0, 10.0)).unwrap();
        let s = e.prepare().unwrap().unwrap();
        e.delivered(
            &s.attempt_id,
            Delivery::Accepted {
                turn_id: "new".into(),
            },
        )
        .unwrap();
        drop(e);
        let reopened = Engine::open(file.path()).unwrap();
        reopened.observe(proof()).unwrap();
        let watch = &reopened.snapshot().unwrap()["watches"][0];
        assert_eq!(watch["state"], "PausedAfterRestart");
        assert_eq!(watch["remaining"], 2);
    }
    #[test]
    fn unknown_delivery_proof_does_not_restore_authority() {
        let (_f, e) = engine();
        e.observe(obs(100.0, 10.0)).unwrap();
        e.configure(config(), false).unwrap();
        e.observe(obs(0.0, 10.0)).unwrap();
        let s = e.prepare().unwrap().unwrap();
        e.delivered(&s.attempt_id, Delivery::Unknown("timeout".into()))
            .unwrap();
        e.observe(proof()).unwrap();
        let watch = &e.snapshot().unwrap()["watches"][0];
        assert_eq!(watch["state"], "NeedsAttention");
        assert_eq!(watch["remaining"], 2);
        assert!(e.prepare().unwrap().is_none());
    }
    #[test]
    fn invalidated_latest_event_never_regenerates() {
        let (_f, e) = engine();
        e.observe(obs(100.0, 10.0)).unwrap();
        e.configure(config(), false).unwrap();
        let mut newer = obs(0.0, 10.0);
        newer.sessions[0].latest.as_mut().unwrap().event_id = "normal".into();
        newer.sessions[0].latest.as_mut().unwrap().kind = "completed".into();
        e.observe(newer).unwrap();
        e.observe(obs(0.0, 10.0)).unwrap();
        assert!(e.prepare().unwrap().is_none());
    }
    #[test]
    fn accepted_turn_id_rejects_manual_same_prompt_turn() {
        let (_f, e) = engine();
        e.observe(obs(100.0, 10.0)).unwrap();
        e.configure(config(), false).unwrap();
        e.observe(obs(0.0, 10.0)).unwrap();
        let s = e.prepare().unwrap().unwrap();
        e.delivered(
            &s.attempt_id,
            Delivery::Accepted {
                turn_id: "accepted".into(),
            },
        )
        .unwrap();
        e.observe(proof_with_turn("manual")).unwrap();
        assert_eq!(e.snapshot().unwrap()["watches"][0]["remaining"], 2);
    }
    #[test]
    fn deleting_watch_removes_management_but_retains_attempt_and_history() {
        let (_f, e) = engine();
        e.observe(obs(100.0, 10.0)).unwrap();
        e.configure(config(), false).unwrap();
        e.observe(obs(0.0, 10.0)).unwrap();
        let submission = e.prepare().unwrap().unwrap();
        e.action("t", "delete", false).unwrap();

        let snapshot = e.snapshot().unwrap();
        assert!(snapshot["watches"].as_array().unwrap().is_empty());
        assert_eq!(snapshot["attempts"].as_array().unwrap().len(), 1);
        assert!(snapshot["history"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "watch_deleted"));
        assert!(e.prepare().unwrap().is_none());
        drop(submission);
    }
    #[test]
    fn clearing_history_does_not_remove_watches_or_attempts() {
        let (_f, e) = engine();
        e.observe(obs(100.0, 10.0)).unwrap();
        e.configure(config(), false).unwrap();
        e.observe(obs(0.0, 10.0)).unwrap();
        let _submission = e.prepare().unwrap().unwrap();
        assert!(!e.snapshot().unwrap()["history"]
            .as_array()
            .unwrap()
            .is_empty());

        e.clear_history().unwrap();
        let snapshot = e.snapshot().unwrap();
        assert!(snapshot["history"].as_array().unwrap().is_empty());
        assert_eq!(snapshot["watches"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["attempts"].as_array().unwrap().len(), 1);
    }
    #[test]
    fn old_settings_default_to_system_language_without_rewriting_prompt() {
        let (_f, e) = engine();
        e.db.execute(
            "UPDATE settings SET value=?1 WHERE id=1",
            params![r#"{"defaultPrompt":"legacy","pollSeconds":60,"runtimePath":"","notifications":true,"autoStart":false}"#],
        ).unwrap();
        let settings = e.settings().unwrap();
        assert_eq!(settings.language, "system");
        assert_eq!(settings.default_prompt, "legacy");
    }
    #[test]
    fn new_history_persists_message_identity_and_old_null_metadata_is_safe() {
        let (_f, e) = engine();
        e.observe(obs(100.0, 10.0)).unwrap();
        e.configure(config(), false).unwrap();
        e.db.execute("INSERT INTO history(thread_id,time,kind,message) VALUES(NULL,'t','legacy','Paused by user')", []).unwrap();
        e.pause_all().unwrap();
        let snapshot = e.snapshot().unwrap();
        let history = snapshot["history"].as_array().unwrap();
        assert!(history.iter().any(|item| item["kind"] == "pause_all"
            && item["displayMessage"]["code"] == "watch.allPaused"));
        assert!(history.iter().any(|item| item["kind"] == "legacy"
            && item["displayMessage"]["code"] == "backend.external"
            && item["displayMessage"]["fallback"] == "Paused by user"));
        let stored: Option<String> =
            e.db.query_row(
                "SELECT message_code FROM history WHERE kind='pause_all' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_deref(), Some("watch.allPaused"));
    }
    #[test]
    fn direct_watch_transition_persists_metadata_via_the_shared_reason_mapper() {
        let (_f, e) = engine();
        e.observe(obs(100.0, 10.0)).unwrap();
        e.configure(config(), false).unwrap();
        e.action("t", "pause", false).unwrap();
        let stored: (Option<String>, Option<String>) =
            e.db.query_row(
                "SELECT reason_code,reason_params FROM watches WHERE thread_id='t'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored.0.as_deref(), Some("watch.pausedByUser"));
        assert_eq!(stored.1.as_deref(), Some("{}"));
    }

    #[test]
    fn language_switch_preserves_prompt_authorization_attempts_and_history() {
        let (_f, e, _) = submitted();
        let before = e.snapshot().unwrap();
        let authorization: i32 =
            e.db.query_row(
                "SELECT authorization FROM watches WHERE thread_id='t'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        for language in ["en", "zh-CN", "system", "unsupported"] {
            e.set_language(language).unwrap();
            let after = e.snapshot().unwrap();
            for key in ["watches", "attempts", "history", "observation"] {
                assert_eq!(before[key], after[key], "{key}");
            }
            assert_eq!(
                before["settings"]["defaultPrompt"],
                after["settings"]["defaultPrompt"]
            );
            assert_eq!(
                authorization,
                e.db.query_row(
                    "SELECT authorization FROM watches WHERE thread_id='t'",
                    [],
                    |r| r.get::<_, i32>(0)
                )
                .unwrap()
            );
        }
        assert_eq!(e.settings().unwrap().language, "zh-CN");
    }

    #[test]
    fn legacy_schema_upgrades_without_reverse_translating_history_and_language_persists() {
        let f = NamedTempFile::new().unwrap();
        let db = Connection::open(f.path()).unwrap();
        db.execute_batch("CREATE TABLE history (id INTEGER PRIMARY KEY AUTOINCREMENT,thread_id TEXT NULL,time TEXT NOT NULL,kind TEXT NOT NULL,message TEXT NOT NULL); INSERT INTO history(thread_id,time,kind,message) VALUES(NULL,'old','legacy','Paused by user');").unwrap();
        drop(db);
        let e = Engine::open(f.path()).unwrap();
        e.set_language("en").unwrap();
        let h = e.snapshot().unwrap()["history"][0].clone();
        let message: Message = serde_json::from_value(h["displayMessage"].clone()).unwrap();
        assert_eq!(crate::i18n::render(&message, "zh-CN"), "Paused by user");
        assert_eq!(h["message"], "Paused by user");
        drop(e);
        let reopened = Engine::open(f.path()).unwrap();
        assert_eq!(reopened.settings().unwrap().language, "en");
        assert_eq!(reopened.snapshot().unwrap()["history"][0], h);
    }

    #[test]
    fn attempt_diagnostics_are_structured_without_changing_counting() {
        let (_f, e, _) = submitted();
        let mut observation = obs(20.0, 10.0);
        observation.diagnostic.compatible = false;
        observation.diagnostic.reason = "Desktop 管道无法连接".into();
        e.observe(observation).unwrap();
        let attempt = e.read_attempts().unwrap()[0].clone();
        assert_eq!(attempt["status"], "Unknown");
        assert_eq!(attempt["counted"], false);
        assert_eq!(
            attempt["detailMessage"]["code"],
            "delivery.compatibilityLost"
        );
        assert_eq!(e.read_watches().unwrap()[0]["remaining"], 2);
        // Simulate an older retained row with no metadata: preserve verbatim.
        e.db.execute(
            "UPDATE attempts SET detail_code=NULL,detail_params=NULL",
            [],
        )
        .unwrap();
        let old = e.read_attempts().unwrap()[0].clone();
        assert_eq!(old["detail"], attempt["detail"]);
        assert_eq!(old["detailMessage"]["code"], "backend.external");
    }
    #[test]
    fn upgrade_preserves_terminal_attempt_audit_text_and_metadata() {
        for status in ["Unknown", "Rejected", "Confirmed"] {
            let (f, e, _) = submitted();
            e.db.execute(
                "UPDATE attempts SET status=?1,detail='历史故障原文'",
                params![status],
            )
            .unwrap();
            e.db.execute(
                "UPDATE attempts SET detail_code=NULL,detail_params=NULL",
                [],
            )
            .unwrap();
            let before = e.read_attempts().unwrap();
            drop(e);
            let reopened = Engine::open(f.path()).unwrap();
            assert_eq!(reopened.read_attempts().unwrap(), before, "{status}");
            assert!(reopened.prepare().unwrap().is_none());
        }
    }
}
