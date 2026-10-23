import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  NATIVE_ICONS_PATH,
  NATIVE_LOCALES_PATH,
  NATIVE_PROVIDER_LOGOS_PATH,
  NATIVE_THEMES_PATH,
  renderNativeIcons,
  renderNativeLocales,
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

  test('the checked-in provider logos match the bundled logos and aliases', () => {
    expect(checkedIn(NATIVE_PROVIDER_LOGOS_PATH)).toBe(renderNativeProviderLogos());
  });
});
