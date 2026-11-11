import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { getDefaultMagicPromptTemplate } from '@/lib/magicPrompts';

import {
  NATIVE_DATE_FORMATS_PATH,
  NATIVE_HTML_RENDER_PATH,
  NATIVE_ICONS_PATH,
  NATIVE_LOCALES_PATH,
  NATIVE_MAGIC_PROMPT_SETTINGS_PATH,
  NATIVE_MAGIC_PROMPTS_PATH,
  NATIVE_MENU_LOCALES_PATH,
  NATIVE_PROVIDER_LOGOS_PATH,
  NATIVE_THEMES_PATH,
  renderNativeDateFormats,
  renderNativeHtmlRender,
  renderNativeIcons,
  renderNativeLocales,
  renderNativeMagicPromptSettings,
  renderNativeMagicPrompts,
  renderNativeMenuLocales,
  renderNativeProviderLogos,
  renderNativeThemes,
} from './nativeResources';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..', '..', '..');
const checkedIn = (relativePath: string): string => readFileSync(resolve(repoRoot, relativePath), 'utf8');

describe('native desktop resources', () => {
  // Regenerate with `bun run native:resources` after changing themes, keys or translations.
  test('the checked-in locales match the dictionaries', () => {
    expect(checkedIn(NATIVE_LOCALES_PATH)).toBe(renderNativeLocales());
  });

  test('the checked-in themes match the resolved built-in themes', () => {
    expect(checkedIn(NATIVE_THEMES_PATH)).toBe(renderNativeThemes());
  });

  test('the checked-in icons match the icon sprite', () => {
    expect(checkedIn(NATIVE_ICONS_PATH)).toBe(renderNativeIcons());
  });

  test('the checked-in date formats match the runtime\'s Intl data', () => {
    expect(checkedIn(NATIVE_DATE_FORMATS_PATH)).toBe(renderNativeDateFormats());
  });

  test('the checked-in magic prompts match the built-in templates', () => {
    expect(checkedIn(NATIVE_MAGIC_PROMPTS_PATH)).toBe(renderNativeMagicPrompts());
  });

  test('the checked-in magic prompt settings match the built-in definitions', () => {
    expect(checkedIn(NATIVE_MAGIC_PROMPT_SETTINGS_PATH)).toBe(renderNativeMagicPromptSettings());
  });

  test('conflict handoffs export both canonical prompt templates', () => {
    const rendered = renderNativeMagicPrompts();
    for (const id of ['git.conflict.resolve.visible', 'git.conflict.resolve.instructions'] as const) {
      expect(rendered).toContain(`${JSON.stringify(id)}: ${JSON.stringify(getDefaultMagicPromptTemplate(id))}`);
    }
  });

  test('the checked-in HTML page bootstrap matches the chat\'s', () => {
    expect(checkedIn(NATIVE_HTML_RENDER_PATH)).toBe(renderNativeHtmlRender());
  });

  test('the checked-in menu labels match the Electron menu dictionaries', () => {
    expect(checkedIn(NATIVE_MENU_LOCALES_PATH)).toBe(renderNativeMenuLocales());
  });

  test('the checked-in provider logos match the bundled logos and aliases', () => {
    expect(checkedIn(NATIVE_PROVIDER_LOGOS_PATH)).toBe(renderNativeProviderLogos());
  });
});
