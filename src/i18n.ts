import i18next from 'i18next';
import { initReactI18next } from 'react-i18next';
import ui from './locales/ui.json';
import backend from './locales/backend.json';

export type Language = 'zh-CN' | 'en';
export type LanguagePreference = 'system' | Language;
export type Message = {
  code: string;
  params: Record<string, string | number | Message>;
  fallback: string;
};
export function resolveLanguage(preference: string = 'system', system = 'zh-CN'): Language {
  if (preference === 'en' || preference === 'zh-CN') return preference;
  if (preference !== 'system') return 'zh-CN';
  return /^en(?:-|$)/i.test(system) ? 'en' : 'zh-CN';
}

void i18next.use(initReactI18next).init({
  resources: {
    'zh-CN': { translation: { ...ui['zh-CN'], ...backend['zh-CN'] } },
    en: { translation: { ...ui.en, ...backend.en } },
  },
  lng: 'zh-CN',
  fallbackLng: 'zh-CN',
  supportedLngs: ['zh-CN', 'en'],
  keySeparator: false,
  interpolation: { escapeValue: false },
  initAsync: false,
});

export function displayMessage(
  value: Message | string | null | undefined,
  language: Language,
): string {
  if (!value) return '';
  // Legacy records and third-party strings are intentionally kept verbatim.
  if (typeof value === 'string') return value;
  return resolveMessage(value, language, 0) ?? value.fallback ?? value.code;
}
function resolveMessage(value: Message, language: Language, depth: number): string | null {
  if (!value || typeof value.code !== 'string' || depth >= 8) return null;
  const template: unknown = i18next.getResource(language, 'translation', value.code);
  if (typeof template !== 'string') return value.fallback || value.code;
  const params: Record<string, string | number> = {};
  for (const match of template.matchAll(/{{\s*([^}]+?)\s*}}/g)) {
    const key = match[1];
    const param = value.params?.[key];
    if (typeof param === 'string' || typeof param === 'number') params[key] = param;
    else if (param && typeof param === 'object' && !Array.isArray(param)) {
      const rendered = resolveMessage(param, language, depth + 1);
      if (rendered === null) return null;
      params[key] = rendered;
    } else return null;
  }
  return i18next.t(value.code, { ...params, lng: language });
}
export function errorMessage(error: unknown): Message | string {
  if (error && typeof error === 'object' && 'code' in error && 'fallback' in error) {
    return error as Message;
  }
  return String(error);
}
export function uiMessage(code: string, params: Message['params'] = {}): Message {
  return { code, params, fallback: i18next.t(code, { ...params, lng: 'zh-CN' }) };
}
export function numberText(value: number, language: Language) {
  return new Intl.NumberFormat(language, { maximumFractionDigits: 2 }).format(value);
}
export default i18next;
