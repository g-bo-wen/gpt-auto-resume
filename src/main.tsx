import React, { useCallback, useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { invoke, isTauri } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import {
  defaults,
  emptySnapshot,
  immediatePossible,
  label,
  resetTime,
  waitingQuotaCount,
  watchBadgeTone,
  when,
  type Session,
  type Settings,
  type Snapshot,
  type Watch,
} from './model';
import './style.css';
import i18n, {
  displayMessage,
  errorMessage,
  uiMessage,
  numberText,
  resolveLanguage,
  type Message,
  type LanguagePreference,
} from './i18n';
import { useTranslation } from 'react-i18next';

type Page = 'sessions' | 'active' | 'history' | 'settings';
type Editor = {
  threadId: string;
  title: string;
  prompt: string;
  remaining: number | null;
  allowImmediate: boolean;
  existing: boolean;
};
async function native<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauri()) throw uiMessage('ui.nativeOnly');
  return invoke<T>(command, args);
}
function App() {
  const { t } = useTranslation();
  const [snapshot, setSnapshot] = useState<Snapshot>(emptySnapshot),
    [page, setPage] = useState<Page>('sessions');
  const [error, setError] = useState<Message | string>(''),
    [notice, setNotice] = useState<Message | string>(''),
    [busy, setBusy] = useState(false),
    [search, setSearch] = useState('');
  const [editor, setEditor] = useState<Editor | null>(null),
    [settings, setSettings] = useState<Settings>(defaults);
  const [enable, setEnable] = useState<Watch | null>(null);
  const read = useCallback(async () => {
    try {
      const s = await native<Snapshot>('snapshot');
      setSnapshot(s);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);
  useEffect(() => {
    if (!isTauri()) return;
    void read();
    let cleanup: (() => void) | undefined;
    let disposed = false;
    const cleanups: Array<() => void> = [];
    cleanup = () => cleanups.forEach((f) => f());
    void listen('state-changed', () => void read()).then((f) => {
      if (disposed) f();
      else cleanups.push(f);
    });
    void listen<Message | string>('backend-error', (e) => setError(e.payload)).then((f) => {
      if (disposed) f();
      else cleanups.push(f);
    });
    return () => {
      disposed = true;
      cleanup?.();
    };
  }, [read]);
  const language = resolveLanguage(
    snapshot.settings.language,
    snapshot.resolvedLanguage ?? navigator.language,
  );
  useEffect(() => {
    void i18n.changeLanguage(language);
    document.documentElement.lang = language;
  }, [language]);
  const message = (value: Message | string | null | undefined) => displayMessage(value, language);
  const settingsVersion = JSON.stringify({ ...snapshot.settings, language: undefined });
  useEffect(() => setSettings(snapshot.settings), [settingsVersion]);
  const work = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    setError('');
    setNotice('');
    try {
      await fn();
      await read();
      return true;
    } catch (e) {
      setError(errorMessage(e));
      return false;
    } finally {
      setBusy(false);
    }
  };
  const sessions = snapshot.observation?.sessions ?? [],
    diag = snapshot.observation?.diagnostic,
    quota = snapshot.observation?.quota;
  const watches = snapshot.watches,
    active = watches.filter((w) => !['Stopped', 'Completed'].includes(w.state)),
    attention = active.filter((w) => w.state === 'NeedsAttention').length;
  const waitingQuota = waitingQuotaCount(watches);
  const openEditor = (s: Session) => {
    const w = watches.find((w) => w.threadId === s.threadId);
    setEditor({
      threadId: s.threadId,
      title: s.title,
      prompt: w?.prompt ?? snapshot.settings.defaultPrompt,
      remaining: w ? w.remaining : 1,
      allowImmediate: false,
      existing: !!w,
    });
  };
  const act = (w: Watch, action: string) =>
    work(() => native('watch_action', { threadId: w.threadId, action, allowImmediate: false }));
  const filtered = sessions.filter((s) =>
    `${s.title} ${s.cwd}`.toLowerCase().includes(search.toLowerCase()),
  );
  return (
    <div className="shell">
      <aside>
        <div className="brand">
          <span className="brand-icon">↻</span>
          <div>
            Codex<span>Auto Resume</span>
          </div>
        </div>
        <div className="nav-label">{t('ui.workspace')}</div>
        <nav aria-label={t('ui.navigation')}>
          {(
            [
              ['sessions', t('ui.sessions'), '◫'],
              ['active', t('ui.active'), '↻'],
              ['history', t('ui.history'), '◷'],
              ['settings', t('ui.settings'), '⚙'],
            ] as const
          ).map(([key, title, icon]) => (
            <button
              key={key}
              className={page === key ? 'selected' : ''}
              onClick={() => setPage(key)}
            >
              <span>{icon}</span>
              {title}
              {key === 'active' && <b>{waitingQuota}</b>}
            </button>
          ))}
        </nav>
        <div className="aside-bottom">
          <div className="quota-reset">
            <span>{t('ui.quotaReset')}</span>
            <strong>{resetTime(quota?.resetAt, language)}</strong>
          </div>
          <div className="connection">
            <i className={diag?.compatible ? 'good' : 'bad'} />
            {diag?.compatible ? t('ui.connected') : t('ui.sendingStopped')}
          </div>
          <small>Windows Native · v0.1.0</small>
          <button className="quiet" disabled={busy} onClick={() => void work(() => native('quit'))}>
            {t('ui.quit')}
          </button>
        </div>
      </aside>
      <main>
        <header>
          <div>
            <div className="eyebrow">CODEX · 5H RECOVERY</div>
            <h1>
              {
                {
                  sessions: t('ui.sessions'),
                  active: t('ui.active'),
                  history: t('ui.history'),
                  settings: t('ui.settings'),
                }[page]
              }
            </h1>
          </div>
          <button
            disabled={busy}
            onClick={() =>
              void work(async () => {
                const s = await native<Snapshot>('refresh');
                setSnapshot(s);
              })
            }
          >
            {t('ui.refresh')}
          </button>
        </header>
        <div
          className={`compatibility ${diag?.compatible ? 'connected' : 'blocked'}`}
          role="status"
        >
          <span className="status-symbol">{diag?.compatible ? '✓' : '!'}</span>
          <div>
            <strong>{diag?.compatible ? t('ui.connected') : t('ui.incompatible')}</strong>
            {snapshot.observation?.quotaStale && (
              <small>{t('ui.lastQuota', { time: when(quota?.capturedAt, language) })}</small>
            )}
            {(!diag?.compatible || snapshot.observation?.quotaStale) && (
              <p>
                {(diag?.reasonMessage ? message(diag.reasonMessage) : diag?.reason) ??
                  (isTauri() ? t('ui.reading') : t('ui.preview'))}
              </p>
            )}
            <small>
              {t('ui.checked', {
                time: when(diag?.checkedAt, language),
                desktop: diag?.desktopVersion ?? t('ui.unknown'),
                runtime: diag?.runtimeVersion ?? t('ui.unknown'),
              })}
            </small>
          </div>
          {!diag?.compatible && (
            <button onClick={() => setPage('settings')}>{t('ui.diagnostics')}</button>
          )}
        </div>
        {error && (
          <div className="alert" role="alert">
            {message(error)}
            <button aria-label={t('ui.closeError')} onClick={() => setError('')}>
              ×
            </button>
          </div>
        )}
        {notice && (
          <div className="notice" role="status">
            {message(notice)}
          </div>
        )}
        <div className="deferred">{t('ui.pendingAcceptance')}</div>
        {page === 'sessions' && (
          <>
            <section className="metrics">
              <div>
                <span>{t('ui.managed')}</span>
                <strong>
                  {numberText(active.length, language)}
                  <small>{t('ui.watchUnit')}</small>
                </strong>
              </div>
              <div>
                <span>{t('ui.fiveHourUsed')}</span>
                <strong>{quota ? `${numberText(quota.fiveHourUsed, language)}%` : '—'}</strong>
              </div>
              <div>
                <span>{t('ui.weeklyUsed')}</span>
                <strong>{quota ? `${numberText(quota.weeklyUsed, language)}%` : '—'}</strong>
              </div>
              <div>
                <span>{t('ui.attention')}</span>
                <strong className={attention ? 'warning' : ''}>{attention}</strong>
              </div>
            </section>
            <div className="section-top">
              <div>
                <h2>{t('ui.localSessions')}</h2>
                <p>{t('ui.latestOnly')}</p>
              </div>
              <input
                aria-label={t('ui.search')}
                placeholder={t('ui.searchPlaceholder')}
                value={search}
                onChange={(e) => setSearch(e.target.value)}
              />
            </div>
            <section className="cards">
              {filtered.map((s) => {
                const w = watches.find((w) => w.threadId === s.threadId);
                return (
                  <article className="session-card" key={s.threadId}>
                    <div className="session-heading">
                      <div className="project-icon">⌘</div>
                      <div>
                        <h3>{s.title}</h3>
                        <p className="path" title={s.cwd}>
                          {s.cwd}
                        </p>
                      </div>
                      {w && <span className="badge">↻ Auto Resume</span>}
                    </div>
                    <div className="session-meta">
                      <span>Codex: {label(s.status, language)}</span>
                      <span>{when(s.updatedAt, language)}</span>
                    </div>
                    {w && (
                      <div className="watch-line">
                        <strong>{label(w.state, language)}</strong>
                        <span>
                          {w.remaining === null
                            ? t('ui.continuous')
                            : t('ui.remaining', { count: w.remaining })}
                        </span>
                        {w.reason && <p>{message(w.reasonMessage ?? w.reason)}</p>}
                      </div>
                    )}
                    <div className="card-footer">
                      <small>{s.source}</small>
                      <button
                        className="primary"
                        disabled={busy || !diag?.compatible}
                        onClick={() => openEditor(s)}
                      >
                        {w ? t('ui.editConfig') : t('ui.startResume')}
                      </button>
                    </div>
                  </article>
                );
              })}
            </section>
            {!filtered.length && <Empty title={t('ui.noSessions')} text={t('ui.noSessionsHelp')} />}
          </>
        )}
        {page === 'active' && (
          <>
            <div className="section-top">
              <div>
                <h2>{t('ui.watchManagement')}</h2>
                <p>{t('ui.backgroundHelp')}</p>
              </div>
              <button
                disabled={busy || !active.length}
                onClick={() => void work(() => native('pause_all'))}
              >
                {t('ui.pauseAll')}
              </button>
            </div>
            <section className="cards">
              {watches.map((w) => {
                const s = sessions.find((s) => s.threadId === w.threadId);
                return (
                  <article className="session-card" key={w.threadId}>
                    <div className="session-heading">
                      <h3>{s?.title ?? t('ui.unavailableSession')}</h3>
                      <span className={`badge ${watchBadgeTone(w.state)}`}>
                        {label(w.state, language)}
                      </span>
                    </div>
                    <p className="reason">
                      {message(w.reasonMessage ?? w.reason) || t('ui.waitingFuture')}
                    </p>
                    <div className="prompt-preview">{w.prompt}</div>
                    <div className="card-footer">
                      <span>
                        {w.remaining === null
                          ? t('ui.continuousMonitor')
                          : t('ui.countRemaining', { count: w.remaining })}
                      </span>
                      <div className="actions">
                        {s && (
                          <button disabled={busy} onClick={() => openEditor(s)}>
                            {t('ui.edit')}
                          </button>
                        )}
                        {['Monitoring', 'WaitingQuota', 'ReadyToResume', 'Resuming'].includes(
                          w.state,
                        ) ? (
                          <button disabled={busy} onClick={() => void act(w, 'pause')}>
                            {t('ui.pause')}
                          </button>
                        ) : (
                          (w.state !== 'Completed' || w.remaining !== 0) && (
                            <button
                              disabled={busy || !diag?.compatible}
                              onClick={() => setEnable(w)}
                            >
                              {t('ui.enable')}
                            </button>
                          )
                        )}
                        {w.state === 'NeedsAttention' && (
                          <button disabled={busy} onClick={() => void act(w, 'reconcile')}>
                            {t('ui.reconcile')}
                          </button>
                        )}
                        {!['Stopped', 'Completed'].includes(w.state) && (
                          <button
                            className="danger"
                            disabled={busy}
                            onClick={() => void act(w, 'stop')}
                          >
                            {t('ui.stop')}
                          </button>
                        )}
                        <button
                          className="danger"
                          disabled={busy}
                          onClick={() => {
                            if (window.confirm(t('ui.deleteConfirm'))) {
                              void act(w, 'delete');
                            }
                          }}
                        >
                          {t('ui.delete')}
                        </button>
                      </div>
                    </div>
                  </article>
                );
              })}
            </section>
            {!watches.length && <Empty title={t('ui.noWatches')} text={t('ui.noWatchesHelp')} />}
          </>
        )}
        {page === 'history' && (
          <>
            <div className="section-top">
              <div>
                <h2>{t('ui.events')}</h2>
                <p>{t('ui.eventsHelp')}</p>
              </div>
              <div className="actions">
                <button
                  disabled={busy || !snapshot.history.length}
                  onClick={() => {
                    if (window.confirm(t('ui.clearConfirm'))) {
                      void work(() => native('clear_history'));
                    }
                  }}
                >
                  {t('ui.clearHistory')}
                </button>
                <button
                  disabled={busy}
                  onClick={() =>
                    void work(async () => {
                      const path = await native<string>('export_diagnostics');
                      setNotice(uiMessage('ui.exported', { path }));
                    })
                  }
                >
                  {t('ui.export')}
                </button>
              </div>
            </div>
            <div className="history-table">
              <table>
                <thead>
                  <tr>
                    <th>{t('ui.time')}</th>
                    <th>{t('ui.event')}</th>
                    <th>{t('ui.sessionDetails')}</th>
                  </tr>
                </thead>
                <tbody>
                  {snapshot.history.map((h) => (
                    <tr key={h.id}>
                      <td>{when(h.time, language)}</td>
                      <td>
                        <span className="event-kind">
                          {t('ui.event.' + h.kind, { defaultValue: h.kind })}
                        </span>
                      </td>
                      <td>
                        <strong>
                          {sessions.find((s) => s.threadId === h.threadId)?.title ??
                            (h.threadId ? t('ui.sessionEvent') : t('ui.appDiagnostic'))}
                        </strong>
                        <p>{message(h.displayMessage ?? h.message)}</p>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
              {!snapshot.history.length && (
                <Empty title={t('ui.noHistory')} text={t('ui.noHistoryHelp')} />
              )}
            </div>
          </>
        )}
        {page === 'settings' && (
          <>
            <form
              className="settings-panel"
              onSubmit={(e) => {
                e.preventDefault();
                void work(() =>
                  native('save_settings', {
                    settings: { ...settings, language: snapshot.settings.language },
                  }),
                ).then((ok) => {
                  if (ok) setNotice(uiMessage('ui.saved'));
                });
              }}
            >
              <h2>{t('ui.recoverySettings')}</h2>
              <label>
                {t('ui.language')}
                <select
                  value={snapshot.settings.language ?? 'system'}
                  disabled={busy}
                  onChange={(e) => {
                    const language = e.target.value as LanguagePreference;
                    if (!isTauri()) {
                      setSnapshot((s) => ({ ...s, settings: { ...s.settings, language } }));
                      return;
                    }
                    void work(() => native('set_language', { language }));
                  }}
                >
                  <option value="system">{t('ui.systemLanguage')}</option>
                  <option value="zh-CN">简体中文</option>
                  <option value="en">English</option>
                </select>
                <small>{t(isTauri() ? 'ui.languageHelp' : 'ui.languagePreviewHelp')}</small>
              </label>
              <label>
                {t('ui.defaultPrompt')}
                <textarea
                  required
                  maxLength={64000}
                  rows={4}
                  value={settings.defaultPrompt}
                  onChange={(e) => setSettings({ ...settings, defaultPrompt: e.target.value })}
                />
                <small>{t('ui.defaultPromptHelp')}</small>
              </label>
              <div className="form-row">
                <label>
                  {t('ui.pollSeconds')}
                  <input
                    type="number"
                    min={15}
                    max={3600}
                    required
                    value={settings.pollSeconds}
                    onChange={(e) =>
                      setSettings({ ...settings, pollSeconds: Number(e.target.value) })
                    }
                  />
                </label>
                <label>
                  {t('ui.runtimePath')}
                  <input
                    value={settings.runtimePath}
                    placeholder={t('ui.runtimePlaceholder')}
                    onChange={(e) => setSettings({ ...settings, runtimePath: e.target.value })}
                  />
                </label>
              </div>
              <label className="check">
                <input
                  type="checkbox"
                  checked={settings.notifications}
                  onChange={(e) => setSettings({ ...settings, notifications: e.target.checked })}
                />
                {t('ui.notifications')}
              </label>
              <label className="check">
                <input
                  type="checkbox"
                  checked={settings.autoStart}
                  onChange={(e) => setSettings({ ...settings, autoStart: e.target.checked })}
                />
                {t('ui.autoStart')}
              </label>
              <p className="hint">{t('ui.autoStartHelp')}</p>
              <button className="primary" disabled={busy}>
                {t('ui.saveSettings')}
              </button>
            </form>
            <section className="settings-panel">
              <h2>{t('ui.compatibility')}</h2>
              <dl>
                <dt>{t('ui.currentState')}</dt>
                <dd>{diag?.compatible ? t('ui.ready') : t('ui.stopped')}</dd>
                <dt>{t('ui.reason')}</dt>
                <dd>
                  {(diag?.reasonMessage ? message(diag.reasonMessage) : diag?.reason) ??
                    t('ui.notChecked')}
                </dd>
                <dt>{t('ui.stage')}</dt>
                <dd>
                  {diag?.stage ? t('ui.stage.' + diag.stage, { defaultValue: diag.stage }) : '—'}
                </dd>
                <dt>{t('ui.versions')}</dt>
                <dd>
                  Desktop {diag?.desktopVersion ?? t('ui.unknown')} / Runtime{' '}
                  {diag?.runtimeVersion ?? t('ui.unknown')}
                </dd>
              </dl>
              <p className="hint">{t('ui.compatibilityHelp')}</p>
              <button disabled={busy} onClick={() => void work(() => native('open_data_folder'))}>
                {t('ui.openData')}
              </button>
            </section>
          </>
        )}
      </main>
      {editor && (
        <div className="overlay">
          <section
            className="dialog"
            role="dialog"
            aria-modal="true"
            aria-labelledby="editor-title"
          >
            <div className="dialog-top">
              <h2 id="editor-title">{editor.existing ? t('ui.editWatch') : t('ui.startResume')}</h2>
              <button
                aria-label={t('ui.closeConfig')}
                onClick={() => setEditor(null)}
                disabled={busy}
              >
                ×
              </button>
            </div>
            <p>{editor.title}</p>
            <form
              onSubmit={(e) => {
                e.preventDefault();
                void work(() =>
                  native('configure', {
                    config: {
                      threadId: editor.threadId,
                      prompt: editor.prompt,
                      remaining: editor.remaining,
                    },
                    allowImmediate: editor.allowImmediate,
                  }),
                ).then((ok) => {
                  if (ok) setEditor(null);
                });
              }}
            >
              <label>
                {t('ui.resumePrompt')}
                <textarea
                  required
                  maxLength={64000}
                  rows={6}
                  value={editor.prompt}
                  onChange={(e) => setEditor({ ...editor, prompt: e.target.value })}
                />
              </label>
              <div className="mode-options">
                {[1, 3, 5, null].map((n) => (
                  <button
                    type="button"
                    key={String(n)}
                    className={editor.remaining === n ? 'chosen' : ''}
                    onClick={() => setEditor({ ...editor, remaining: n })}
                  >
                    {n === null ? t('ui.continuous') : t('ui.count', { count: n })}
                  </button>
                ))}
                <label>
                  {t('ui.customCount')}
                  <input
                    type="number"
                    min={1}
                    max={1000000}
                    value={editor.remaining ?? ''}
                    placeholder="N"
                    onChange={(e) =>
                      setEditor({
                        ...editor,
                        remaining: e.target.value ? Number(e.target.value) : null,
                      })
                    }
                  />
                </label>
              </div>
              <p className="hint">{t('ui.countHelp')}</p>
              {immediatePossible(
                sessions.find((s) => s.threadId === editor.threadId),
                quota,
                snapshot.observation?.quotaStale,
              ) && (
                <label className="check confirm-check">
                  <input
                    type="checkbox"
                    required
                    checked={editor.allowImmediate}
                    onChange={(e) => setEditor({ ...editor, allowImmediate: e.target.checked })}
                  />
                  {t('ui.immediateConfirm')}
                </label>
              )}
              <footer>
                <button type="button" disabled={busy} onClick={() => setEditor(null)}>
                  {t('ui.cancel')}
                </button>
                <button className="primary" disabled={busy}>
                  {editor.existing ? t('ui.saveConfig') : t('ui.enableWatch')}
                </button>
              </footer>
            </form>
          </section>
        </div>
      )}
      {enable && (
        <div className="overlay">
          <section
            className="dialog"
            role="dialog"
            aria-modal="true"
            aria-labelledby="enable-title"
          >
            <h2 id="enable-title">{t('ui.reenableWatch')}</h2>
            <p>{t('ui.reenableHelp')}</p>
            <div className="prompt-preview">{enable.prompt}</div>
            <p className="hint">{t('ui.unknownHelp')}</p>
            <footer>
              <button disabled={busy} onClick={() => setEnable(null)}>
                {t('ui.cancel')}
              </button>
              <button
                className="primary"
                disabled={busy}
                onClick={() =>
                  void work(() =>
                    native('watch_action', {
                      threadId: enable.threadId,
                      action: 'enable',
                      allowImmediate: true,
                    }),
                  ).then((ok) => {
                    if (ok) setEnable(null);
                  })
                }
              >
                {t('ui.confirmEnable')}
              </button>
            </footer>
          </section>
        </div>
      )}
    </div>
  );
}
function Empty({ title, text }: { title: string; text: string }) {
  return (
    <div className="empty">
      <span>◫</span>
      <h3>{title}</h3>
      <p>{text}</p>
    </div>
  );
}
createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
