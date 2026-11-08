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
import { fileURLToPath, pathToFileURL } from 'node:url';
import { z } from 'zod';

import { iconSpriteData } from '@/components/icon/sprite';
import { themes } from '@/lib/theme/themes';
import { getReadableThemeColors } from '@/lib/theme/readableColors';
import { agentColorPalette } from '@/lib/agentColorPalette';
import { PROVIDER_LOGO_ALIASES } from '@/lib/providerLogoCandidates';
import { getLocaleDirection, LOCALES, type Locale } from '@/lib/i18n/runtime';
import { getDefaultMagicPromptTemplate } from '@/lib/magicPrompts';
import { MAGIC_PROMPT_COMMANDS } from '@/components/chat/composer/submit/slashCommands';
import { getIntlLocale } from '@/lib/i18n/intl';
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
export const NATIVE_DATE_FORMATS_PATH = `${resourcesDirectory}/date-formats.json`;
export const NATIVE_MAGIC_PROMPTS_PATH = `${resourcesDirectory}/magic-prompts.json`;
/** The Electron application menu's labels, which the native app menu shows. */
export const NATIVE_MENU_LOCALES_PATH = `${resourcesDirectory}/menu-locales.json`;
/** The Electron shell's own menu dictionaries, outside `@/lib/i18n`. */
const MENU_LOCALES_SOURCE = 'packages/electron/menu-locales.mjs';
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

type ClockFormat = { layout: string[] | null; hours: string[]; dayPeriods: string[] };
type LocaleDateFormat = {
  hour12ByDefault: boolean;
  months: string[];
  twelveHour: ClockFormat;
  twentyFourHour: ClockFormat;
  numbers: Record<string, string>;
};

/** Token counts Settings labels with `formatCompactNumber` (compaction steps and defaults). */
const NATIVE_COMPACT_NUMBERS = [5_000, 10_000, 15_000, 30_000, 50_000, 100_000];
/** Shares Settings labels as percentages (compaction buffer steps). */
const NATIVE_PERCENTS = [5, 10, 15, 20, 25, 30, 40, 50];

/**
 * How each locale writes a message's time, as `formatTimestampForDisplay`
 * asks `Intl` for it: the short month names and, for the 12- and 24-hour
 * clocks, the order of hour, minute and day period with the hour and day
 * period each hour of the day shows. The native client has no locale data
 * of its own, so it fills these in rather than formatting.
 */
export const renderNativeDateFormats = (): string => {
  const document: Record<string, LocaleDateFormat> = {};
  for (const locale of LOCALES) {
    const intl = getIntlLocale(locale);
    const clock = (hour12: boolean): ClockFormat => {
      const format = new Intl.DateTimeFormat(intl, { hour: 'numeric', minute: '2-digit', hour12 });
      let layout: string[] | null = null;
      const hours: string[] = [];
      const dayPeriods: string[] = [];
      for (let hour = 0; hour < 24; hour += 1) {
        const parts = format.formatToParts(new Date(2026, 0, 1, hour, 5));
        const hourLayout = parts.map((part) => (
          part.type === 'literal' ? part.value : `{${part.type}}`
        ));
        if (layout && hourLayout.join('\u0000') !== layout.join('\u0000')) {
          throw new Error(`${intl} writes ${hour}:05 in another layout`);
        }
        layout = hourLayout;
        const minute = parts.find((part) => part.type === 'minute')?.value;
        if (minute !== '05') throw new Error(`${intl} writes minute 5 as ${minute}`);
        hours.push(parts.find((part) => part.type === 'hour')?.value ?? '');
        dayPeriods.push(parts.find((part) => part.type === 'dayPeriod')?.value ?? '');
      }
      return { layout, hours, dayPeriods };
    };
    document[locale] = {
      hour12ByDefault: new Intl.DateTimeFormat(intl, { hour: 'numeric' }).resolvedOptions().hour12 === true,
      months: Array.from({ length: 12 }, (_, month) => new Date(2026, month, 1).toLocaleString(intl, { month: 'short' })),
      twelveHour: clock(true),
      twentyFourHour: clock(false),
      // `formatCompactNumber` and the compaction buffer's percent labels.
      numbers: Object.fromEntries([
        ...NATIVE_COMPACT_NUMBERS.map((value) => [String(value), new Intl.NumberFormat(intl, {
          notation: 'compact',
          compactDisplay: 'short',
          maximumFractionDigits: 1,
          minimumFractionDigits: 0,
        }).format(value)]),
        ...NATIVE_PERCENTS.map((percent) => [`${percent}%`, new Intl.NumberFormat(intl, {
          style: 'percent',
          maximumFractionDigits: 1,
        }).format(percent / 100)]),
      ]),
    };
  }
  return `${JSON.stringify(document, null, 2)}\n`;
};

/**
 * Prompts the native review flow renders besides the commands' own: the
 * handoff, the review session's first prompt and the messages an automatic
 * review run passes between the two sessions.
 */
const NATIVE_REVIEW_PROMPTS = [
  'session.review.instructions',
  'session.reviewHandoff.visible',
  'session.reviewHandoff.instructions',
  'session.reviewSession.visible',
  'session.reviewSessionWithoutHandoff.visible',
  'session.reviewFeedbackToImplementer.visible',
  'session.implementationResponseToReviewer.visible',
] as const;

/** Stands for the typed argument while a command's variables are exported. */
const ARGUMENT_SENTINEL = '\u0000argument\u0000';

/**
 * Exports the composer's prompt-pair commands with the built-in templates of
 * their prompts. A command's variables are functions of its argument; each is
 * exported as its value without an argument and with `{{argument}}` standing
 * for one, so the native client derives the same text.
 */
export const renderNativeMagicPrompts = (): string => {
  const templates: Record<string, string> = {};
  const commands = MAGIC_PROMPT_COMMANDS.map((command) => {
    templates[command.visiblePrompt] = getDefaultMagicPromptTemplate(command.visiblePrompt);
    templates[command.instructionsPrompt] = getDefaultMagicPromptTemplate(command.instructionsPrompt);
    const empty = command.buildVariables?.('') ?? {};
    const filled = command.buildVariables?.(ARGUMENT_SENTINEL) ?? {};
    const variables = (['visible', 'instructions'] as const).flatMap((side) => Object.keys(filled[side] ?? {}).sort().map((key) => {
      const withArgument = filled[side]?.[key] ?? '';
      if (withArgument.split(ARGUMENT_SENTINEL).length > 2) throw new Error(`${command.name} uses its argument twice in ${key}`);
      return {
        side,
        key,
        empty: empty[side]?.[key] ?? '',
        withArgument: withArgument.replace(ARGUMENT_SENTINEL, '{{argument}}'),
      };
    }));
    return {
      name: command.name,
      visible: command.visiblePrompt,
      instructions: command.instructionsPrompt,
      requiresSession: command.requires === 'session',
      errorToastKey: command.errorToastKey,
      variables,
    };
  });
  for (const id of NATIVE_REVIEW_PROMPTS) {
    templates[id] = getDefaultMagicPromptTemplate(id);
  }
  return `${JSON.stringify({ commands, templates }, null, 2)}\n`;
};

const menuDictionaries = z.record(z.string(), z.record(z.string(), z.string()));
const menuLocalesModule = z.object({ MENU_LOCALE_DICTIONARIES: menuDictionaries }).parse(
  await import(pathToFileURL(resolve(repoRoot, MENU_LOCALES_SOURCE)).href),
);

/**
 * Exports the Electron application menu's labels for every UI locale. Throws
 * when a locale has no menu dictionary or lacks a label English has, so the
 * native menu never falls back silently.
 */
export const renderNativeMenuLocales = (): string => {
  const dictionaries = menuLocalesModule.MENU_LOCALE_DICTIONARIES;
  const english = dictionaries.en;
  if (!english) throw new Error('the menu dictionaries have no English entry');
  const locales: Record<string, Record<string, string>> = {};
  for (const locale of LOCALES) {
    const dictionary = dictionaries[locale];
    if (!dictionary) throw new Error(`locale ${locale} has no menu dictionary`);
    const labels: Record<string, string> = {};
    for (const key of Object.keys(english).sort()) {
      const label = dictionary[key];
      if (!label) throw new Error(`locale ${locale} has no menu label for ${key}`);
      labels[key] = label;
    }
    locales[locale] = labels;
  }
  return `${JSON.stringify(locales, null, 2)}\n`;
};

const writeNativeResources = (): string[] => {
  const outputs: Array<[string, string]> = [
    [NATIVE_LOCALES_PATH, renderNativeLocales()],
    [NATIVE_THEMES_PATH, renderNativeThemes()],
    [NATIVE_ICONS_PATH, renderNativeIcons()],
    [NATIVE_PROVIDER_LOGOS_PATH, renderNativeProviderLogos()],
    [NATIVE_DATE_FORMATS_PATH, renderNativeDateFormats()],
    [NATIVE_MAGIC_PROMPTS_PATH, renderNativeMagicPrompts()],
    [NATIVE_MENU_LOCALES_PATH, renderNativeMenuLocales()],
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
