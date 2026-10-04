mod contracts;
mod desktop;
mod engine;
mod i18n;
mod sessions;

use contracts::{Observation, Quota, Settings, WatchConfig};
use desktop::{hash, now, Desktop};
use engine::Engine;
use i18n::{message_from_text, normalize_preference, Message};
use serde_json::{json, Value};
use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
    time::Duration,
};
use tauri::{Emitter, Manager, State};
use tauri_plugin_notification::NotificationExt;

fn quota_refresh_due(checked: Option<std::time::Instant>, force: bool) -> bool {
    force || checked.is_none_or(|checked| checked.elapsed() >= Duration::from_secs(300))
}

#[cfg(test)]
mod polling_tests {
    use super::*;
    #[test]
    fn quota_polling_waits_five_minutes_but_forced_checks_are_immediate() {
        let now = std::time::Instant::now();
        assert!(quota_refresh_due(None, false));
        assert!(!quota_refresh_due(Some(now), false));
        assert!(!quota_refresh_due(
            Some(now - Duration::from_secs(299)),
            false
        ));
        assert!(quota_refresh_due(
            Some(now - Duration::from_secs(300)),
            false
        ));
        assert!(quota_refresh_due(Some(now), true));
    }
}

struct Backend {
    engine: Engine,
    desktop: Desktop,
    home: PathBuf,
    data: PathBuf,
    last_diagnostic: String,
    logged_history: i64,
    fault: Option<String>,
    quota_cache: Option<(Quota, std::collections::HashMap<String, String>)>,
    quota_checked: Option<std::time::Instant>,
    quota_failed: bool,
    evidence_cache: sessions::EvidenceCache,
}
struct AppState {
    backend: Mutex<Backend>,
    quitting: AtomicBool,
    revision: AtomicU64,
}
impl Backend {
    fn view(&self) -> Result<Value, String> {
        let mut s = self.engine.snapshot()?;
        if let Some(reason) = &self.fault {
            s["observation"]["diagnostic"] =
                serde_json::to_value(self.desktop.diagnostic(false, reason.clone(), "supervisor"))
                    .map_err(|_| "诊断投影失败")?;
        }
        Ok(s)
    }
    fn clear_fault(&mut self) -> Result<(), String> {
        if let Some(reason) = &self.fault {
            let value = self.engine.snapshot()?["observation"].clone();
            if !value.is_null() {
                let mut o: Observation =
                    serde_json::from_value(value).map_err(|_| "恢复前状态无法核实")?;
                o.diagnostic = self.desktop.diagnostic(false, reason.clone(), "supervisor");
                self.engine.observe(o)?;
            } else {
                self.engine.pause_all()?;
            }
            self.fault = None;
        }
        Ok(())
    }
    fn refresh(&mut self) -> Result<(), String> {
        self.refresh_with_quota(true)
    }
    fn refresh_with_quota(&mut self, force: bool) -> Result<(), String> {
        let settings = self.engine.settings()?;
        let parsed = sessions::discover(&self.home);
        let context = parsed.first().map(|(s, _)| s.thread_id.clone());
        let mut sessions = parsed
            .iter()
            .take(20)
            .map(|(s, _)| s.clone())
            .collect::<Vec<_>>();
        let mut quota_stale = false;
        let result = (|| -> Result<Option<Quota>, String> {
            self.desktop.connect(&settings.runtime_path)?;
            let context = context.as_ref().ok_or("没有可核实的 Desktop 本地会话")?;
            let due = quota_refresh_due(self.quota_checked, force);
            let quota_result = if due {
                self.quota_checked = Some(std::time::Instant::now());
                let result = self.desktop.quota(context);
                self.quota_failed = result.is_err();
                result
            } else if let Some((quota, _)) = &self.quota_cache {
                Ok(quota.clone())
            } else {
                quota_stale = true;
                return Ok(None);
            };
            let (quota, titles) = match quota_result {
                Ok(quota) => {
                    quota_stale = self.quota_failed;
                    match self.desktop.list(context) {
                        Ok(listing) => (quota, sessions::desktop_titles(&listing)),
                        Err(_) if self.desktop.read_temporarily_unavailable() => {
                            quota_stale = true;
                            let Some(cached) = self.quota_cache.clone() else {
                                return Ok(None);
                            };
                            cached
                        }
                        Err(reason) => return Err(reason),
                    }
                }
                Err(_) if self.desktop.quota_temporarily_unavailable() => {
                    quota_stale = true;
                    let Some(cached) = self.quota_cache.clone() else {
                        return Ok(None);
                    };
                    cached
                }
                Err(reason) => return Err(reason),
            };
            if titles.is_empty() {
                return Err("Desktop 会话列表格式无法识别，已停止自动恢复".into());
            }
            // Latest events are re-read after all read-only Desktop round trips.
            sessions = crate::sessions::discover_with_evidence(
                &self.home,
                &self.engine.pending_baselines()?,
                &mut self.evidence_cache,
            )
            .into_iter()
            .filter(|(s, scope)| scope == &quota.scope && titles.contains_key(&s.thread_id))
            .map(|(mut s, _)| {
                s.title = titles[&s.thread_id].clone();
                s
            })
            .collect();
            if !quota_stale {
                self.quota_cache = Some((quota.clone(), titles));
            }
            Ok(Some(quota))
        })();
        let (quota, mut diagnostic) = match result {
            Ok(q) => (
                q,
                self.desktop.diagnostic(
                    true,
                    if quota_stale {
                        "Desktop 读取暂时失败，跳过本轮；保留授权，暂不发送".into()
                    } else {
                        "Desktop 已连接".into()
                    },
                    if quota_stale { "quota_retry" } else { "ready" },
                ),
            ),
            Err(reason) => {
                self.quota_cache = None;
                (None, self.desktop.diagnostic(false, reason, "discovery"))
            }
        };
        self.log_desktop_failure();
        if let Some(reason) = &self.fault {
            diagnostic.compatible = false;
            diagnostic.reason = reason.clone();
            diagnostic.reason_message = Some(message_from_text(reason));
            diagnostic.stage = "supervisor".into();
        }
        let diagnostic_identity = diagnostic
            .reason_message
            .as_ref()
            .map(i18n::identity)
            .unwrap_or_else(|| diagnostic.reason.clone());
        let changed =
            self.last_diagnostic != format!("{}:{diagnostic_identity}", diagnostic.compatible);
        if changed {
            self.last_diagnostic = format!("{}:{diagnostic_identity}", diagnostic.compatible);
            self.log(json!({"time":now(),"kind":"compatibility","compatible":diagnostic.compatible,"reason":diagnostic.reason,
                "stage":diagnostic.stage,"desktopVersion":diagnostic.desktop_version,"runtimeVersion":diagnostic.runtime_version}));
        }
        self.engine.observe(Observation {
            sessions,
            quota,
            quota_stale,
            diagnostic,
        })?;
        self.log_history()?;
        Ok(())
    }
    fn tick(&mut self, revision: &AtomicU64) -> Result<(), String> {
        let generation = revision.load(Ordering::SeqCst);
        self.refresh_with_quota(false)?;
        let snapshot = self.engine.snapshot()?;
        if !snapshot["observation"]["diagnostic"]["compatible"]
            .as_bool()
            .unwrap_or(false)
        {
            return Ok(());
        }
        if snapshot["observation"]["quotaStale"] == true {
            return Ok(());
        }
        // The supervisor is the only production caller of the send effect.
        // Watch authorization is never restored at application startup.
        let candidate = snapshot["watches"]
            .as_array()
            .and_then(|w| w.iter().find(|w| w["state"] == "ReadyToResume"))
            .and_then(|w| w["threadId"].as_str())
            .map(str::to_owned);
        if let Some(thread) = candidate {
            if let Err(reason) = self.desktop.check_thread(&thread) {
                self.log_desktop_failure();
                let previous = snapshot["observation"].clone();
                let mut o: Observation =
                    serde_json::from_value(previous).map_err(|_| "观测格式错误")?;
                o.diagnostic = self.desktop.diagnostic(false, reason, "preflight");
                self.engine.observe(o)?;
                self.log_history()?;
                return Ok(());
            }
            // Re-read original latest events AFTER the contextual IPC round-trip.
            // Another client still cannot be locked atomically; no exactly-once claim.
            self.refresh()?;
            if self.engine.snapshot()?["observation"]["quotaStale"] == true {
                return Ok(());
            }
        } else {
            return Ok(());
        }
        if generation != revision.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(submission) = self.engine.prepare()? {
            let delivery = if generation == revision.load(Ordering::SeqCst) {
                self.desktop.send(&submission, &self.home, || {
                    generation == revision.load(Ordering::SeqCst)
                })
            } else {
                contracts::Delivery::Rejected("提交前收到暂停、停止或配置变更，未发送".into())
            };
            self.engine.delivered(&submission.attempt_id, delivery)?;
            self.log_desktop_failure();
            self.log_history()?;
        }
        Ok(())
    }
    fn log_desktop_failure(&self) {
        if let Some(failure) = self.desktop.take_failure() {
            self.log(json!({"time":now(),"kind":"desktop_tool_failure","failure":failure}));
        }
    }

    fn log(&self, value: Value) {
        let path = self.data.join("diagnostics.jsonl");
        // Rotate bounded diagnostic files, entirely inside our own data directory.
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 2 * 1024 * 1024) {
            let previous = self.data.join("diagnostics.previous.jsonl");
            let _ = std::fs::remove_file(&previous);
            let _ = std::fs::rename(&path, &previous);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{value}");
        }
    }
    fn log_history(&mut self) -> Result<(), String> {
        let value = self.engine.snapshot()?;
        if let Some(items) = value["history"].as_array() {
            for h in items
                .iter()
                .rev()
                .filter(|v| v["id"].as_i64().unwrap_or(0) > self.logged_history)
            {
                self.log(
                    json!({"time":h["time"],"kind":h["kind"],"message":h["message"],
                    "threadCorrelation":h["threadId"].as_str().map(|s|hash(s)[..12].to_owned())}),
                );
            }
            if let Some(id) = items.first().and_then(|h| h["id"].as_i64()) {
                self.logged_history = id;
            }
        }
        Ok(())
    }
}

#[tauri::command]
async fn snapshot(app: tauri::AppHandle) -> Result<Value, Message> {
    let value = tauri::async_runtime::spawn_blocking(move || {
        app.state::<AppState>()
            .backend
            .lock()
            .map_err(|_| "应用状态不可用".to_string())?
            .view()
    })
    .await
    .map_err(|_| "后台读取失败".to_string())?;
    Ok(value?)
}
#[tauri::command]
async fn refresh(app: tauri::AppHandle) -> Result<Value, Message> {
    let value = tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let mut b = state.backend.lock().map_err(|_| "应用状态不可用")?;
        b.clear_fault()?;
        b.refresh()?;
        b.view()
    })
    .await
    .map_err(|_| "只读刷新失败".to_string())?;
    Ok(value?)
}
#[tauri::command]
async fn configure(
    app: tauri::AppHandle,
    config: WatchConfig,
    allow_immediate: bool,
) -> Result<(), Message> {
    app.state::<AppState>()
        .revision
        .fetch_add(1, Ordering::SeqCst);
    let result = tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let mut b = state.backend.lock().map_err(|_| "应用状态不可用")?;
        b.refresh()?;
        b.engine.configure(config, allow_immediate)?;
        b.log_history()
    })
    .await
    .map_err(|_| "保存 Watch 失败".to_string())?;
    Ok(result?)
}
#[tauri::command]
async fn watch_action(
    app: tauri::AppHandle,
    thread_id: String,
    action: String,
    allow_immediate: bool,
) -> Result<(), Message> {
    if action == "pause" || action == "stop" || action == "delete" {
        app.state::<AppState>()
            .revision
            .fetch_add(1, Ordering::SeqCst);
    }
    let result = tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let mut b = state.backend.lock().map_err(|_| "应用状态不可用")?;
        if action == "enable" || action == "reconcile" {
            b.clear_fault()?;
            b.refresh()?;
        }
        b.engine.action(&thread_id, &action, allow_immediate)?;
        b.log_history()
    })
    .await
    .map_err(|_| "Watch 操作失败".to_string())?;
    Ok(result?)
}
#[tauri::command]
async fn clear_history(app: tauri::AppHandle) -> Result<(), Message> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let result = state
            .backend
            .lock()
            .map_err(|_| "应用状态不可用".to_string())?
            .engine
            .clear_history();
        result
    })
    .await
    .map_err(|_| "清理事件历史失败".to_string())?;
    Ok(result?)
}
#[tauri::command]
fn pause_all(state: State<AppState>) -> Result<(), Message> {
    state.revision.fetch_add(1, Ordering::SeqCst);
    state
        .backend
        .lock()
        .map_err(|_| "应用状态不可用")?
        .engine
        .pause_all()?;
    Ok(())
}
#[tauri::command]
fn save_settings(
    app: tauri::AppHandle,
    state: State<AppState>,
    mut settings: Settings,
) -> Result<(), Message> {
    state.revision.fetch_add(1, Ordering::SeqCst);
    if !(15..=3600).contains(&settings.poll_seconds)
        || settings.default_prompt.trim().is_empty()
        || settings.default_prompt.len() > 64000
    {
        return Err("检查间隔应为 15–3600 秒，默认消息不能为空且不超过 64KB".into());
    }
    settings.language = normalize_preference(&settings.language).into();
    let b = state.backend.lock().map_err(|_| "应用状态不可用")?;
    let previous = b.engine.settings()?;
    if previous.auto_start != settings.auto_start {
        set_auto_start(settings.auto_start)?;
    }
    if let Err(e) = b.engine.save_settings(settings) {
        let _ = set_auto_start(previous.auto_start);
        return Err(e.into());
    }
    let language = b.engine.settings()?.language;
    let attention = needs_attention(b.view().ok().as_ref());
    drop(b);
    refresh_tray_text(&app, &language, attention);
    Ok(())
}

/// Language changes are presentation-only: no revision is advanced and no
/// watch, authorization, prompt, or auto-start setting is touched.
#[tauri::command]
fn set_language(
    app: tauri::AppHandle,
    state: State<AppState>,
    language: String,
) -> Result<(), Message> {
    let b = state.backend.lock().map_err(|_| "应用状态不可用")?;
    b.engine.set_language(normalize_preference(&language))?;
    let attention = needs_attention(b.view().ok().as_ref());
    drop(b);
    refresh_tray_text(&app, normalize_preference(&language), attention);
    Ok(())
}

fn needs_attention(view: Option<&Value>) -> bool {
    view.is_none_or(|view| {
        view["observation"]["diagnostic"]["compatible"] != true
            || view["watches"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["state"] == "NeedsAttention"))
    })
}
fn refresh_tray_text(app: &tauri::AppHandle, language: &str, attention: bool) {
    if let Some(tray) = app.tray_by_id("watch-status") {
        let key = if attention {
            "tray.attention"
        } else {
            "tray.listening"
        };
        let _ = tray.set_tooltip(Some(&i18n::localized(key, language)));
        let menu = (|| -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
            let show = tauri::menu::MenuItem::with_id(
                app,
                "show",
                i18n::localized("tray.show", language),
                true,
                None::<&str>,
            )?;
            let pause = tauri::menu::MenuItem::with_id(
                app,
                "pause",
                i18n::localized("tray.pauseAll", language),
                true,
                None::<&str>,
            )?;
            let exit = tauri::menu::MenuItem::with_id(
                app,
                "quit",
                i18n::localized("tray.quit", language),
                true,
                None::<&str>,
            )?;
            tauri::menu::Menu::with_items(app, &[&show, &pause, &exit])
        })();
        if let Ok(menu) = menu {
            let _ = tray.set_menu(Some(menu));
        }
    }
}
fn set_auto_start(enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let exe = std::env::current_exe().map_err(|_| "无法解析自启动路径")?;
        let mut cmd = std::process::Command::new("reg.exe");
        cmd.creation_flags(0x08000000);
        let key = "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run";
        if enabled {
            cmd.args([
                "add",
                key,
                "/v",
                "CodexAutoResume",
                "/t",
                "REG_SZ",
                "/d",
                &format!("\"{}\"", exe.display()),
                "/f",
            ]);
        } else {
            cmd.args(["delete", key, "/v", "CodexAutoResume", "/f"]);
        }
        let output = cmd.output().map_err(|_| "自启动设置失败")?;
        if !output.status.success() {
            return Err("自启动设置失败，未保存设置".into());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Err("仅支持 Windows Native".into())
    }
}
#[tauri::command]
fn export_diagnostics(state: State<AppState>) -> Result<String, String> {
    let b = state.backend.lock().map_err(|_| "应用状态不可用")?;
    let s = b.engine.snapshot()?;
    let value = json!({"exportedAt":now(),"diagnostic":s["observation"]["diagnostic"],"watchCounts":s["watches"].as_array().map(Vec::len),"note":"真实发送效果测试由用户延期"});
    let path = b.data.join("diagnostics-export.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&value).map_err(|_| "诊断导出失败")?,
    )
    .map_err(|_| "诊断文件写入失败")?;
    Ok(path.to_string_lossy().into_owned())
}
#[tauri::command]
fn open_data_folder(state: State<AppState>) -> Result<(), String> {
    let b = state.backend.lock().map_err(|_| "应用状态不可用")?;
    std::process::Command::new("explorer.exe")
        .arg(&b.data)
        .spawn()
        .map_err(|_| "无法打开数据目录")?;
    Ok(())
}
#[tauri::command]
fn quit(app: tauri::AppHandle, state: State<AppState>) -> Result<(), String> {
    state.revision.fetch_add(1, Ordering::SeqCst);
    state
        .backend
        .lock()
        .map_err(|_| "应用状态不可用")?
        .engine
        .pause_all()?;
    state.quitting.store(true, Ordering::SeqCst);
    app.exit(0);
    Ok(())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            snapshot,
            refresh,
            configure,
            watch_action,
            clear_history,
            pause_all,
            save_settings,
            set_language,
            export_diagnostics,
            open_data_folder,
            quit
        ])
        .setup(|app| {
            let data = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data)?;
            // Desktop's default store is explicitly used; inherited CODEX_HOME is not trusted.
            let home = app.path().home_dir()?.join(".codex");
            let engine =
                Engine::open(&data.join("auto-resume.sqlite")).map_err(std::io::Error::other)?;
            let language = engine
                .settings()
                .map(|settings| settings.language)
                .unwrap_or_else(|_| "zh-CN".into());
            let logged_history = engine
                .snapshot()
                .ok()
                .and_then(|s| {
                    s["history"]
                        .as_array()
                        .and_then(|h| h.first())
                        .and_then(|h| h["id"].as_i64())
                })
                .unwrap_or(0);
            app.manage(AppState {
                backend: Mutex::new(Backend {
                    engine,
                    desktop: Desktop::default(),
                    home,
                    data,
                    last_diagnostic: String::new(),
                    logged_history,
                    fault: None,
                    quota_cache: None,
                    quota_checked: None,
                    quota_failed: false,
                    evidence_cache: sessions::EvidenceCache::default(),
                }),
                quitting: AtomicBool::new(false),
                revision: AtomicU64::new(0),
            });
            let show = tauri::menu::MenuItem::with_id(
                app,
                "show",
                i18n::localized("tray.show", &language),
                true,
                None::<&str>,
            )?;
            let pause = tauri::menu::MenuItem::with_id(
                app,
                "pause",
                i18n::localized("tray.pauseAll", &language),
                true,
                None::<&str>,
            )?;
            let exit = tauri::menu::MenuItem::with_id(
                app,
                "quit",
                i18n::localized("tray.quit", &language),
                true,
                None::<&str>,
            )?;
            let menu = tauri::menu::Menu::with_items(app, &[&show, &pause, &exit])?;
            let mut rgba = vec![0u8; 32 * 32 * 4];
            for px in rgba.chunks_mut(4) {
                px.copy_from_slice(&[32, 112, 100, 255]);
            }
            tauri::tray::TrayIconBuilder::with_id("watch-status")
                .icon(tauri::image::Image::new_owned(rgba, 32, 32))
                .tooltip(i18n::localized("tray.restartPaused", &language))
                .menu(&menu)
                .on_menu_event(|app, e| match e.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "pause" => {
                        let state = app.state::<AppState>();
                        state.revision.fetch_add(1, Ordering::SeqCst);
                        if let Ok(b) = state.backend.lock() {
                            let _ = b.engine.pause_all();
                        }
                        let _ = app.emit("state-changed", ());
                    }
                    "quit" => {
                        let state = app.state::<AppState>();
                        state.revision.fetch_add(1, Ordering::SeqCst);
                        if let Ok(b) = state.backend.lock() {
                            if b.engine.pause_all().is_err() {
                                return;
                            }
                        }
                        state.quitting.store(true, Ordering::SeqCst);
                        app.exit(0);
                    }
                    _ => {}
                })
                .build(app)?;
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let mut last_attention = String::new();
                loop {
                    let state = handle.state::<AppState>();
                    if state.quitting.load(Ordering::SeqCst) {
                        break;
                    }
                    let seconds = if let Ok(mut b) = state.backend.lock() {
                        if let Err(e) = b.tick(&state.revision) {
                            b.fault = Some(format!("后台故障，已停止发送：{e}"));
                            let _ = handle.emit("backend-error", message_from_text(&e));
                            if let Some(tray) = handle.tray_by_id("watch-status") {
                                let _ = tray.set_tooltip(Some(&i18n::localized(
                                    "tray.supervisorFault",
                                    &b.engine
                                        .settings()
                                        .map(|s| s.language)
                                        .unwrap_or_else(|_| "zh-CN".into()),
                                )));
                            }
                            b.log(json!({"time":now(),"kind":"supervisor_error","reason":e}));
                        }
                        if let Ok(s) = b.view() {
                            let diag = &s["observation"]["diagnostic"];
                            let attention = s["watches"]
                                .as_array()
                                .is_some_and(|w| w.iter().any(|v| v["state"] == "NeedsAttention"));
                            let compatible = diag["compatible"] == true;
                            let language = b
                                .engine
                                .settings()
                                .map(|settings| settings.language)
                                .unwrap_or_else(|_| "zh-CN".into());
                            if let Some(tray) = handle.tray_by_id("watch-status") {
                                let tooltip = if compatible && !attention {
                                    i18n::localized("tray.listening", &language)
                                } else {
                                    i18n::localized("tray.attention", &language)
                                };
                                let _ = tray.set_tooltip(Some(&tooltip));
                            }
                            let reason_message = if !compatible {
                                serde_json::from_value(diag["reasonMessage"].clone())
                                    .unwrap_or_else(|_| {
                                        message_from_text(
                                            diag["reason"].as_str().unwrap_or("接口未知"),
                                        )
                                    })
                            } else if attention {
                                i18n::message(
                                    "backend.attentionRequired",
                                    "部分 Watch 需要处理，自动发送已停止",
                                )
                            } else {
                                i18n::message("", "")
                            };
                            let reason = i18n::render(&reason_message, &language);
                            let identity = i18n::identity(&reason_message);
                            if !reason.is_empty()
                                && last_attention != identity
                                && b.engine.settings().is_ok_and(|s| s.notifications)
                            {
                                let _ = handle
                                    .notification()
                                    .builder()
                                    .title(&i18n::localized(
                                        "notification.attentionTitle",
                                        &language,
                                    ))
                                    .body(&reason)
                                    .show();
                            }
                            last_attention = identity;
                        }
                        let _ = handle.emit("state-changed", ());
                        if b.engine.confirmation_pending().unwrap_or(false) {
                            5
                        } else {
                            b.engine.settings().map(|s| s.poll_seconds).unwrap_or(60)
                        }
                    } else {
                        60
                    };
                    // Interruptible lifecycle wait; no polling of Desktop during sleep.
                    for _ in 0..seconds {
                        if state.quitting.load(Ordering::SeqCst) {
                            return;
                        }
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if !window.state::<AppState>().quitting.load(Ordering::SeqCst) {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("无法启动 Codex Auto Resume");
}

/// Read-only integration check: never opens the app database or the send effect.
pub fn observe_only() {
    let mut desktop = Desktop::default();
    let result = (|| -> Result<Value, String> {
        let home = PathBuf::from(
            std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map_err(|_| "用户主目录不可用")?,
        )
        .join(".codex");
        let found = sessions::discover(&home);
        desktop.connect("")?;
        let context = found.first().ok_or("本地 Desktop 会话不可用")?;
        let quota = desktop.quota(&context.0.thread_id)?;
        let list = desktop.list(&context.0.thread_id)?;
        let titles = sessions::desktop_titles(&list);
        let matched = found
            .iter()
            .filter(|(s, scope)| scope == &quota.scope && titles.contains_key(&s.thread_id))
            .count();
        Ok(
            json!({"readOnly":true,"localDesktopSessions":found.len(),"desktopListedSessions":titles.len(),"accountMatchedSessions":matched,
            "fiveHourUsed":quota.five_hour_used,"weeklyUsed":quota.weekly_used,"listKeys":list.as_object().map(|m|m.keys().collect::<Vec<_>>()),
            "diagnostic":desktop.diagnostic(true,"只读发现通过；没有发送消息".into(),"readonly")}),
        )
    })();
    match result {
        Ok(value) => println!("{value}"),
        Err(reason) => {
            println!(
                "{}",
                json!({"readOnly":true,"diagnostic":desktop.diagnostic(false,reason,"readonly")})
            );
            std::process::exit(1);
        }
    }
}
