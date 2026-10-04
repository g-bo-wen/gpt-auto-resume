import i18n, { type Language, type LanguagePreference, type Message } from './i18n';
export type Settings = {
  language: LanguagePreference;
  defaultPrompt: string;
  pollSeconds: number;
  runtimePath: string;
  notifications: boolean;
  autoStart: boolean;
};
export type Quota = {
  scope: string;
  bucket: string;
  capturedAt: string;
  fiveHourUsed: number;
  weeklyUsed: number;
  resetAt: number | null;
};
export type Session = {
  threadId: string;
  title: string;
  cwd: string;
  updatedAt: string;
  source: string;
  status: string;
  latest: null | { kind: string; quota: Quota | null };
};
export type Watch = {
  threadId: string;
  prompt: string;
  remaining: number | null;
  state: string;
  reason: string | null;
  reasonMessage?: Message | null;
  updatedAt: string;
};
export type Diagnostic = {
  compatible: boolean;
  reason: string;
  reasonMessage?: Message | null;
  stage: string;
  checkedAt: string;
  desktopVersion: string | null;
  runtimeVersion: string | null;
};
export type History = {
  id: number;
  threadId: string | null;
  time: string;
  kind: string;
  message: string;
  displayMessage?: Message | null;
};
export type Snapshot = {
  resolvedLanguage?: Language;
  watches: Watch[];
  history: History[];
  settings: Settings;
  observation: null | {
    sessions: Session[];
    quota: Quota | null;
    quotaStale?: boolean;
    diagnostic: Diagnostic;
  };
  attempts: unknown[];
};
export const defaults: Settings = {
  language: 'system',
  defaultPrompt: '请继续完成之前的任务。',
  pollSeconds: 60,
  runtimePath: '',
  notifications: true,
  autoStart: false,
};
export const emptySnapshot: Snapshot = {
  watches: [],
  history: [],
  settings: defaults,
  observation: null,
  attempts: [],
};
export function label(state: string, language: Language = 'zh-CN') {
  return i18n.t('ui.state.' + state, { lng: language, defaultValue: state });
}
export function when(value: string | null | undefined, language: Language = 'zh-CN') {
  if (!value) return '—';
  const d = new Date(value);
  return Number.isNaN(d.valueOf()) ? '—' : d.toLocaleString(language, { hour12: false });
}
export function resetTime(value: number | null | undefined, language: Language = 'zh-CN') {
  if (value == null) return '—';
  const d = new Date(value * 1000);
  return Number.isNaN(d.valueOf()) ? '—' : d.toLocaleString(language, { hour12: false });
}
export function watchBadgeTone(state: string) {
  if (state === 'PausedAfterRestart') return 'restart-paused';
  if (state === 'Stopped') return 'stopped';
  if (state === 'NeedsAttention') return 'attention';
  return '';
}
export function waitingQuotaCount(watches: Watch[]) {
  return watches.filter((watch) => watch.state === 'WaitingQuota').length;
}
export function immediatePossible(
  session: Session | undefined,
  quota: Quota | null | undefined,
  quotaStale = false,
) {
  const old = session?.latest?.quota;
  return (
    !quotaStale &&
    session?.latest?.kind === 'usage_limit_exceeded' &&
    !!old &&
    !!quota &&
    old.bucket === 'codex' &&
    old.scope === quota.scope &&
    old.fiveHourUsed >= 100 &&
    old.weeklyUsed < 100 &&
    quota.fiveHourUsed < 100 &&
    quota.weeklyUsed < 100
  );
}
