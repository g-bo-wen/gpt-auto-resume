import { describe, expect, it } from 'vitest';
import ui from './locales/ui.json';
import backend from './locales/backend.json';
import i18n, { displayMessage, errorMessage, resolveLanguage, uiMessage } from './i18n';
import { defaults, immediatePossible, label, resetTime, type Quota, type Session } from './model';

const placeholders = (text: string) =>
  [...text.matchAll(/{{\s*([^}]+?)\s*}}/g)].map((m) => m[1]).sort();
describe('language resources and display contract', () => {
  it('requires both languages to cover every key and interpolation parameter', () => {
    for (const catalog of [ui, backend]) {
      expect(Object.keys(catalog.en).sort()).toEqual(Object.keys(catalog['zh-CN']).sort());
      for (const key of Object.keys(catalog.en)) {
        const en = (catalog.en as Record<string, string>)[key];
        const zh = (catalog['zh-CN'] as Record<string, string>)[key];
        expect(en.trim(), key).not.toBe('');
        expect(zh.trim(), key).not.toBe('');
        expect(placeholders(en), key).toEqual(placeholders(zh));
      }
    }
    expect(Object.keys(ui.en).filter((key) => key in backend.en)).toEqual([]);
  });
  it('resolves explicit settings, system variants, and unsupported settings predictably', () => {
    expect(resolveLanguage('en', 'zh-CN')).toBe('en');
    expect(resolveLanguage('zh-CN', 'en-US')).toBe('zh-CN');
    expect(resolveLanguage('system', 'en-GB')).toBe('en');
    expect(resolveLanguage('system', 'fr-FR')).toBe('zh-CN');
    expect(resolveLanguage('unknown', 'en-US')).toBe('zh-CN');
    expect(resolveLanguage(undefined, 'en-US')).toBe('en');
  });
  it('rerenders structured notices without rewriting user or legacy content', () => {
    const notice = uiMessage('ui.exported', { path: 'C:/日志/exports.json' });
    expect(displayMessage(notice, 'en')).toBe('Diagnostics exported: C:/日志/exports.json');
    expect(displayMessage(notice, 'zh-CN')).toBe('诊断已导出：C:/日志/exports.json');
    expect(displayMessage('历史原文 Keep exactly', 'en')).toBe('历史原文 Keep exactly');
    expect(defaults.defaultPrompt).toBe('请继续完成之前的任务。');
    expect(displayMessage({ code: 'unrecognized', params: {}, fallback: '保留故障' }, 'en')).toBe(
      '保留故障',
    );
    expect(errorMessage(notice)).toBe(notice);
  });
  it('formats states and local-time dates in the selected language', () => {
    expect(label('PausedAfterRestart', 'en')).toBe('Paused after restart');
    expect(label('PausedAfterRestart', 'zh-CN')).toBe('重启后已暂停');
    expect(label('FutureState', 'en')).toBe('FutureState');
    expect(resetTime(1790985600, 'en')).toBe(
      new Date(1790985600000).toLocaleString('en', { hour12: false }),
    );
  });
  it('keeps nested fault details visible and translated', () => {
    const fault = {
      code: 'backend.supervisorFault',
      fallback: '后台故障',
      params: { detail: { code: 'watch.pausedByUser', params: {}, fallback: 'Paused by user' } },
    };
    expect(displayMessage(fault, 'en')).toContain('Paused by user');
    expect(displayMessage(fault, 'en')).not.toContain('{{');
  });
  it('falls back to complete audit text for missing or malformed parameters', () => {
    const fault = {
      code: 'desktop.compatibilityUnconfirmed',
      params: {},
      fallback: '完整原因 Account identity could not be verified',
    };
    expect(displayMessage(fault, 'en')).toBe(fault.fallback);
    expect(displayMessage({ ...fault, params: { detail: {} as never } }, 'en')).toBe(
      fault.fallback,
    );
  });
  it('switching display language never changes resume eligibility or mutates input', async () => {
    const quota: Quota = {
      scope: 'fixture',
      bucket: 'codex',
      capturedAt: '',
      fiveHourUsed: 20,
      weeklyUsed: 30,
      resetAt: null,
    };
    const session: Session = {
      threadId: 'fixture',
      title: '原文',
      cwd: 'C:/fixture',
      updatedAt: '',
      source: 'desktop',
      status: 'usage_limit_exceeded',
      latest: { kind: 'usage_limit_exceeded', quota: { ...quota, fiveHourUsed: 100 } },
    };
    const before = JSON.stringify({ quota, session });
    for (const language of ['en', 'zh-CN']) {
      await i18n.changeLanguage(language);
      expect(immediatePossible(session, quota)).toBe(true);
      expect(immediatePossible(session, quota, true)).toBe(false);
    }
    expect(JSON.stringify({ quota, session })).toBe(before);
  });
});
