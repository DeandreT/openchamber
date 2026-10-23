/**
 * Generates the native desktop's theme, locale and icon resources from the
 * canonical sources: the built-in theme definitions, resolved exactly as the
 * web UI resolves them, the locale dictionaries and the icon sprite, for the
 * keys and icons the native views use. The output is checked in;
 * `nativeResources.test.ts` fails when it is stale.
 *
 *   bun run native:resources
 */
import { mkdirSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { z } from 'zod';

import { iconSpriteData } from '@/components/icon/sprite';
import { themes } from '@/lib/theme/themes';
import { getReadableThemeColors } from '@/lib/theme/readableColors';
import { agentColorPalette } from '@/lib/agentColorPalette';
import { PROVIDER_LOGO_ALIASES } from '@/lib/providerLogoCandidates';
import { getLocaleDirection, LOCALES, type Locale } from '@/lib/i18n/runtime';
import type { I18nDictionary } from '@/lib/i18n/store';
import { dict as en } from '@/lib/i18n/messages/en';
import { dict as de } from '@/lib/i18n/messages/de';
import { dict as fr } from '@/lib/i18n/messages/fr';
import { dict as nl } from '@/lib/i18n/messages/nl';
import { dict as zhCN } from '@/lib/i18n/messages/zh-CN';
import { dict as zhTW } from '@/lib/i18n/messages/zh-TW';
import { dict as uk } from '@/lib/i18n/messages/uk';
import { dict as es } from '@/lib/i18n/messages/es';
import { dict as ptBR } from '@/lib/i18n/messages/pt-BR';
import { dict as ko } from '@/lib/i18n/messages/ko';
import { dict as pl } from '@/lib/i18n/messages/pl';
import { dict as ja } from '@/lib/i18n/messages/ja';
import { dict as tr } from '@/lib/i18n/messages/tr';
import { dict as ar } from '@/lib/i18n/messages/ar';
import { dict as he } from '@/lib/i18n/messages/he';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '..', '..', '..', '..', '..');
const resourcesDirectory = 'native/crates/desktop/resources';

/** The keys the native views resolve, one per line, owned by the desktop crate. */
export const NATIVE_LOCALE_KEYS_PATH = `${resourcesDirectory}/locale-keys.txt`;
export const NATIVE_LOCALES_PATH = `${resourcesDirectory}/locales.json`;
export const NATIVE_THEMES_PATH = `${resourcesDirectory}/themes.json`;
/** The sprite icons the native views draw, one name per line. */
export const NATIVE_ICON_NAMES_PATH = `${resourcesDirectory}/icon-names.txt`;
export const NATIVE_ICONS_PATH = `${resourcesDirectory}/icons.json`;
export const NATIVE_PROVIDER_LOGOS_PATH = `${resourcesDirectory}/provider-logos.json`;
/** The provider logos the web bundles, one SVG per provider ID. */
const PROVIDER_LOGOS_DIRECTORY = 'packages/ui/src/assets/provider-logos';

const dictionaries = {
  en, de, fr, nl, 'zh-CN': zhCN, 'zh-TW': zhTW, uk, es, 'pt-BR': ptBR, ko, pl, ja, tr, ar, he,
} satisfies Record<Locale, I18nDictionary>;

const isDictionaryKey = (key: string): key is keyof I18nDictionary => Object.hasOwn(en, key);

const placeholders = (message: string): string[] =>
  [...message.matchAll(/\{(\w+)\}/g)].map((match) => match[1] ?? '').sort();

const readList = (relativePath: string): string[] =>
  readFileSync(resolve(repoRoot, relativePath), 'utf8')
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line.length > 0 && !line.startsWith('#'));

const isSpriteIcon = (name: string): name is keyof typeof iconSpriteData => Object.hasOwn(iconSpriteData, name);

/**
 * Exports every used key for every locale.
 *
 * Throws when a key is unknown, a locale lacks it, or a translation's
 * placeholders differ from English, so a broken export never reaches the app.
 */
export const renderNativeLocales = (): string => {
  const keys = readList(NATIVE_LOCALE_KEYS_PATH);
  const locales: Record<string, { direction: string; messages: Record<string, string> }> = {};
  for (const locale of LOCALES) {
    const dictionary = dictionaries[locale];
    const messages: Record<string, string> = {};
    for (const key of keys) {
      if (!isDictionaryKey(key)) throw new Error(`native locale key ${key} is not in the English dictionary`);
      const message = dictionary[key];
      if (!message) throw new Error(`locale ${locale} has no message for ${key}`);
      const expected = placeholders(en[key]).join(',');
      if (placeholders(message).join(',') !== expected) {
        throw new Error(`locale ${locale} changes the placeholders of ${key}`);
      }
      messages[key] = message;
    }
    locales[locale] = { direction: getLocaleDirection(locale), messages };
  }
  return `${JSON.stringify({ defaultLocale: 'en', locales }, null, 2)}\n`;
};

/** Resolved theme colors: named groups of color strings, nested at most a few levels. */
type ColorGroup = { [key: string]: string | ColorGroup | undefined };
const colorLeaf = z.string();
const colorGroup: z.ZodType<ColorGroup> = z.lazy(() => z.record(z.string(), z.union([colorLeaf, colorGroup, z.undefined()])));

/** Flattens a color group into dotted token paths; absent optional colors are skipped. */
const flattenColors = (group: ColorGroup, prefix: string, target: Record<string, string>): void => {
  for (const [key, value] of Object.entries(group)) {
    if (value === undefined) continue;
    const path = prefix ? `${prefix}.${key}` : key;
    const leaf = colorLeaf.safeParse(value);
    if (leaf.success) target[path] = leaf.data;
    else flattenColors(colorGroup.parse(value), path, target);
  }
};

/**
 * Exports every built-in theme's resolved colors, computed readable text
 * colors and agent color palette (syntax role indices in `agentColorPalette`'s
 * keyword, type, function, number, string, operator, variable order).
 */
export const renderNativeThemes = (): string => {
  const exported = themes.map((theme) => {
    const colors: Record<string, string> = {};
    flattenColors(colorGroup.parse(theme.colors), '', colors);
    flattenColors(colorGroup.parse(getReadableThemeColors(theme)), 'readable', colors);
    return {
      id: theme.metadata.id,
      name: theme.metadata.name,
      variant: theme.metadata.variant,
      colors: Object.fromEntries(Object.entries(colors).sort(([left], [right]) => (left < right ? -1 : 1))),
      agentPalette: agentColorPalette(theme).map(({ index, colorful }) => ({ index, colorful })),
    };
  });
  return `${JSON.stringify({ defaultLight: 'openchamber-light', defaultDark: 'openchamber-dark', themes: exported }, null, 2)}\n`;
};

/**
 * Exports the listed sprite glyphs as standalone SVG documents on the
 * sprite's shared 24x24 viewbox. Throws for a name the sprite lacks.
 */
export const renderNativeIcons = (): string => {
  const icons: Record<string, string> = {};
  for (const name of readList(NATIVE_ICON_NAMES_PATH)) {
    if (!isSpriteIcon(name)) throw new Error(`native icon ${name} is not in the icon sprite`);
    icons[name] = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">${iconSpriteData[name]}</svg>`;
  }
  return `${JSON.stringify(icons, null, 2)}\n`;
};

/**
 * Exports the bundled provider logos by provider ID, with the aliases that
 * map other provider IDs onto them.
 */
export const renderNativeProviderLogos = (): string => {
  const directory = resolve(repoRoot, PROVIDER_LOGOS_DIRECTORY);
  const logos: Record<string, string> = {};
  for (const file of readdirSync(directory).filter((name) => name.endsWith('.svg')).sort()) {
    logos[file.slice(0, -'.svg'.length).toLowerCase()] = readFileSync(resolve(directory, file), 'utf8');
  }
  return `${JSON.stringify({ aliases: Object.fromEntries(PROVIDER_LOGO_ALIASES), logos }, null, 2)}\n`;
};

const writeNativeResources = (): string[] => {
  const outputs: Array<[string, string]> = [
    [NATIVE_LOCALES_PATH, renderNativeLocales()],
    [NATIVE_THEMES_PATH, renderNativeThemes()],
    [NATIVE_ICONS_PATH, renderNativeIcons()],
    [NATIVE_PROVIDER_LOGOS_PATH, renderNativeProviderLogos()],
  ];
  return outputs.map(([relativePath, content]) => {
    const target = resolve(repoRoot, relativePath);
    mkdirSync(dirname(target), { recursive: true });
    writeFileSync(target, content, 'utf8');
    return target;
  });
};

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  for (const target of writeNativeResources()) console.log(`wrote ${target}`);
}
