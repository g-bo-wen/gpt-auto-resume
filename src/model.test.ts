import { describe, it, expect } from 'vitest';
import {
  immediatePossible,
  resetTime,
  waitingQuotaCount,
  watchBadgeTone,
  type Quota,
  type Session,
  type Watch,
} from './model';
const q: Quota = {
  scope: 'account-scope',
  bucket: 'codex',
  capturedAt: '2026-10-03T00:00:00Z',
  fiveHourUsed: 20,
  weeklyUsed: 30,
  resetAt: null,
};
const s: Session = {
  threadId: 'fixture',
  title: 'fixture',
  cwd: 'C:/fixture',
  updatedAt: '2026-10-03T00:00:00Z',
  source: 'fixture',
  status: 'usage_limit_exceeded',
  latest: { kind: 'usage_limit_exceeded', quota: { ...q, fiveHourUsed: 100 } },
};
describe('immediate-send confirmation projection', () => {
  it('never treats cached quota as permission to send', () =>
    expect(immediatePossible(s, q, true)).toBe(false));
  it('requires original 5h interruption and restored applicable quota', () =>
    expect(immediatePossible(s, q)).toBe(true));
  it('never presents availability alone as resume eligibility', () => {
    expect(immediatePossible({ ...s, latest: null }, q)).toBe(false);
    expect(immediatePossible({ ...s, latest: { kind: 'completed', quota: q } }, q)).toBe(false);
  });
  it('does not borrow another account or weekly quota', () => {
    expect(immediatePossible(s, { ...q, scope: 'other' })).toBe(false);
    expect(immediatePossible(s, { ...q, weeklyUsed: 100 })).toBe(false);
    expect(
      immediatePossible(
        {
          ...s,
          latest: {
            kind: 'usage_limit_exceeded',
            quota: { ...q, fiveHourUsed: 100, weeklyUsed: 100 },
          },
        },
        q,
      ),
    ).toBe(false);
  });
});

describe('quota reset projection', () => {
  it('formats the Desktop Unix-seconds reset timestamp in local time', () =>
    expect(resetTime(1790985600)).toBe(
      new Date(1790985600 * 1000).toLocaleString('zh-CN', { hour12: false }),
    ));
  it('shows an unavailable marker when reset time is absent', () => {
    expect(resetTime(null)).toBe('—');
  });
});

describe('watch list projections', () => {
  const watch = (state: string): Watch => ({
    threadId: state,
    prompt: 'resume',
    remaining: 1,
    state,
    reason: null,
    updatedAt: '2026-10-03T00:00:00Z',
  });

  it('counts only watches waiting for the 5h quota', () =>
    expect(waitingQuotaCount([watch('WaitingQuota'), watch('Monitoring'), watch('Stopped')])).toBe(
      1,
    ));
  it('projects the requested status tones', () => {
    expect(watchBadgeTone('PausedAfterRestart')).toBe('restart-paused');
    expect(watchBadgeTone('Stopped')).toBe('stopped');
    expect(watchBadgeTone('Monitoring')).toBe('');
  });
});
