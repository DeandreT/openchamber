/** Differential send-configuration fixtures, generated with `bun run native:send-config-fixtures`. */
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import type { Agent, Model, Provider } from '@/lib/opencode/model';
import { projectAgent, configModelIdentifier } from '@/lib/opencode/projection';
import { listModelVariantIds } from '@/lib/modelVariants';
import { getProviderModelDisplayName } from '@/lib/modelDisplay';
import { agentLabel } from '@/lib/agentLabel';
import { createAgentColorResolver } from '@/lib/agentColors';
import { agentColorPalette } from '@/lib/agentColorPalette';
import { getDefaultTheme, getThemeById } from '@/lib/theme/themes';
import { filterVisibleAgents, isAgentHidden } from '@/stores/useAgentsStore';
import { groupProviderCatalog, resolveDefaultAgentModelSelection } from '@/stores/configModelSelection';
import { isStaleAutoSelection, useConfigStore } from '@/stores/useConfigStore';
import { selectAutoReady, useRoutingStore } from '@/stores/useRoutingStore';
import { useUIStore } from '@/stores/useUIStore';

export const SEND_CONFIG_FIXTURE_PATH = 'native/crates/core/tests/fixtures/send_config.json';
const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..', '..', '..');

type Choice = { providerId: string; modelId: string; variant?: string };
type FixtureModel = { id: string; providerModelId?: string; name?: string; variants?: string[] };
type FixtureProvider = { id: string; name?: string; models: FixtureModel[] };
type FixtureAgent = {
  name: string; displayName?: string; mode?: 'primary' | 'subagent' | 'all';
  hidden?: boolean; optionsHidden?: boolean; model?: Choice; color?: string;
};
type Defaults = {
  projectDefaultAgent?: string; projectDefaultModel?: string; projectDefaultVariant?: string;
  settingsDefaultAgent?: string; settingsDefaultModel?: string; settingsDefaultVariant?: string;
  opencodeDefaultAgent?: string; opencodeDefaultModel?: string;
  lastSelectedModel?: string; hiddenModels?: [string, string][]; allowFallback?: boolean;
  autoRouting?: 'unknown' | 'unavailable' | 'ready' | 'notReady';
};
type SelectionInput = { agents: FixtureAgent[]; providers: FixtureProvider[]; defaults: Defaults; settingsLoaded?: boolean };

const toModel = (providerId: string, model: FixtureModel): Model => ({
  id: model.id, modelID: model.providerModelId ?? model.id, providerID: providerId,
  name: model.name ?? '', variants: (model.variants ?? []).map((id) => ({ id })),
  capabilities: { tools: true, input: ['text'], output: ['text'] },
  time: { released: 0 }, cost: [{ input: 0, output: 0, cache: { read: 0, write: 0 } }],
  status: 'active', enabled: true, limit: { context: 0, output: 0 },
});
const toProvider = (provider: FixtureProvider) => ({
  id: provider.id, name: provider.name ?? '', package: provider.id, activation: 'enabled' as const,
  models: provider.models.map((model) => toModel(provider.id, model)),
});
const toAgent = (agent: FixtureAgent): Agent & { options: { hidden: boolean } } => ({
  ...projectAgent({
    id: agent.name, name: agent.displayName ?? '', mode: agent.mode ?? 'primary',
    hidden: agent.hidden ?? false, color: agent.color,
    model: agent.model ? { providerID: agent.model.providerId, id: agent.model.modelId, variant: agent.model.variant } : undefined,
    request: { settings: {}, headers: {}, body: {} }, permissions: [],
  }),
  options: { hidden: agent.optionsHidden ?? false },
});

const providers: FixtureProvider[] = [
  { id: 'p', name: 'Provider', models: [{ id: 'catalog', name: ' Catalog model ', variants: ['high', 'low'] }, { id: 'other' }] },
  { id: 'opencode', models: [{ id: 'big-pickle', name: 'Big Pickle' }] },
];
const agents: FixtureAgent[] = [
  { name: 'plan', mode: 'primary' }, { name: 'build', mode: 'primary' },
  { name: 'helper', mode: 'subagent' }, { name: 'secret', mode: 'primary', hidden: true },
];
const scenario = (name: string, defaults: Defaults = {}, overrides: Partial<SelectionInput> = {}) => ({
  name, input: { agents, providers, defaults: { allowFallback: true, ...defaults }, ...overrides },
});
const selectionScenarios = [
  scenario('project defaults beat settings', { projectDefaultAgent: 'helper', projectDefaultModel: 'p/catalog', projectDefaultVariant: 'low', settingsDefaultAgent: 'plan', settingsDefaultModel: 'p/other', settingsDefaultVariant: 'high' }),
  scenario('project model does not inherit settings effort', { projectDefaultModel: 'p/catalog', settingsDefaultVariant: 'high' }),
  scenario('settings defaults', { settingsDefaultAgent: 'plan', settingsDefaultModel: 'p/catalog', settingsDefaultVariant: 'high' }),
  scenario('configured hidden agent and model remain honored', { settingsDefaultAgent: 'secret', settingsDefaultModel: 'p/catalog', hiddenModels: [['p', 'catalog']] }),
  scenario('OpenCode visible primary default', { opencodeDefaultAgent: 'plan' }),
  scenario('OpenCode hidden agent rejected', { opencodeDefaultAgent: 'secret' }),
  scenario('OpenCode subagent rejected', { opencodeDefaultAgent: 'helper' }),
  scenario('options hidden agent rejected', { opencodeDefaultAgent: 'legacy' }, { agents: [{ name: 'legacy', optionsHidden: true }, { name: 'all', mode: 'all' }] }),
  scenario('build fallback'),
  scenario('first primary fallback', {}, { agents: [{ name: 'helper', mode: 'subagent' }, { name: 'primary' }, { name: 'all', mode: 'all' }] }),
  scenario('first agent fallback includes hidden', {}, { agents: [{ name: 'hidden', hidden: true }, { name: 'helper', mode: 'subagent' }] }),
  scenario('unknown configured agent retained before fallback', { settingsDefaultAgent: 'not-loaded', allowFallback: false }, { agents: [] }),
  scenario('unknown configured agent absent after fallback', { settingsDefaultAgent: 'not-loaded' }, { agents: [] }),
  scenario('empty project agent retains nullish precedence', { projectDefaultAgent: '', settingsDefaultAgent: 'missing', allowFallback: false }, { agents: [] }),
  scenario('agent pin supported effort', { opencodeDefaultModel: 'p/other' }, { agents: [{ name: 'build', model: { providerId: 'p', modelId: 'catalog', variant: 'high' } }] }),
  scenario('agent pin missing catalog keeps effort', {}, { agents: [{ name: 'build', model: { providerId: 'late', modelId: 'nested/model', variant: 'high' } }] }),
  scenario('agent pin unsupported effort drops', {}, { agents: [{ name: 'build', model: { providerId: 'p', modelId: 'catalog', variant: 'xhigh' } }] }),
  scenario('OpenCode config model with effort', { opencodeDefaultModel: configModelIdentifier({ providerID: 'p', model: 'catalog', variant: 'low' }) }),
  scenario('OpenCode config missing model keeps effort', { opencodeDefaultModel: configModelIdentifier(' late/nested/model#xhigh ') }),
  scenario('OpenCode config unsupported effort drops', { opencodeDefaultModel: 'p/other#high' }),
  scenario('OpenCode malformed effort drops', { opencodeDefaultModel: 'p/catalog#low#extra' }),
  scenario('OpenCode rejects hash in provider', { opencodeDefaultModel: 'p#x/catalog' }),
  scenario('fallback disabled before pin and config', { allowFallback: false, opencodeDefaultModel: 'p/catalog' }, { agents: [{ name: 'build', model: { providerId: 'p', modelId: 'other' } }] }),
  scenario('fallback disabled preserves explicit model', { allowFallback: false, settingsDefaultModel: 'late/model', settingsDefaultVariant: 'high' }),
  scenario('unloaded settings preserve Auto despite unavailable feature', { allowFallback: false, settingsDefaultModel: 'openchamber/auto', settingsDefaultVariant: 'high', autoRouting: 'unavailable' }, { settingsLoaded: false }),
  scenario('unloaded settings preserve Auto despite unready routing', { allowFallback: false, settingsDefaultModel: 'openchamber/auto', settingsDefaultVariant: 'high', autoRouting: 'notReady' }, { settingsLoaded: false }),
  scenario('last selected visible through catalog gap', { lastSelectedModel: 'late/nested/model' }),
  scenario('last selected hidden skipped', { lastSelectedModel: 'p/catalog', hiddenModels: [['p', 'catalog']] }),
  scenario('last selected stale Auto skipped', { lastSelectedModel: 'openchamber/auto', autoRouting: 'notReady' }),
  scenario('Big Pickle preferred'),
  scenario('hidden Big Pickle skipped', { hiddenModels: [['opencode', 'big-pickle']] }),
  scenario('first visible model', { hiddenModels: [['p', 'catalog']] }, { providers: [providers[0]] }),
  scenario('all hidden retain Big Pickle', { hiddenModels: [['p', 'catalog'], ['p', 'other'], ['opencode', 'big-pickle']] }),
  scenario('all hidden retain first', { hiddenModels: [['p', 'catalog'], ['p', 'other']] }, { providers: [providers[0]] }),
  scenario('empty first provider prevents unfiltered fallback', { hiddenModels: [['p', 'catalog']] }, { providers: [{ id: 'empty', models: [] }, { id: 'p', models: [{ id: 'catalog' }] }] }),
  scenario('empty catalog', {}, { providers: [], agents: [] }),
  scenario('configured slash ID and whitespace preserved', { settingsDefaultModel: ' p/nested/model ', settingsDefaultVariant: ' high ' }),
  scenario('identifier does not parse effort suffix', { settingsDefaultModel: 'p/catalog#high' }),
  ...['', 'noslash', '/model', 'p/', 'p//', 'p/#x'].map((model) => scenario(`identifier ${JSON.stringify(model)}`, { settingsDefaultModel: model })),
  ...(['unknown', 'unavailable', 'ready', 'notReady'] as const).flatMap((autoRouting) => [
    scenario(`Auto configured ${autoRouting}`, { settingsDefaultModel: 'openchamber/auto', settingsDefaultVariant: 'high', autoRouting }),
    scenario(`Auto remembered ${autoRouting}`, { lastSelectedModel: 'openchamber/auto', autoRouting }),
    // The reference only checks staleness for settings and remembered choices.
    scenario(`Auto agent pin ${autoRouting}`, { autoRouting }, { agents: [{ name: 'build', model: { providerId: 'openchamber', modelId: 'auto', variant: 'high' } }] }),
    scenario(`Auto OpenCode config ${autoRouting}`, { opencodeDefaultModel: 'openchamber/auto#high', autoRouting }),
  ]),
  scenario('derived catalog key validates exact variant', { settingsDefaultModel: 'p/base', settingsDefaultVariant: 'high' }, { providers: [{ id: 'p', models: [{ id: 'fast', providerModelId: 'base', variants: ['low'] }, { id: 'base', variants: ['high'] }] }] }),
  scenario('provider API alias validates effort', { settingsDefaultModel: 'p/api-name', settingsDefaultVariant: 'high' }, { providers: [{ id: 'p', models: [{ id: 'alias', providerModelId: 'api-name', variants: ['high'] }] }] }),
  scenario('fallback uses provider API model name', {}, { providers: [{ id: 'p', models: [{ id: 'fast', providerModelId: 'base' }] }] }),
];

const runSelection = ({ agents: entries, providers: catalog, defaults, settingsLoaded = true }: SelectionInput) => {
  const routing = defaults.autoRouting ?? 'unknown';
  useConfigStore.setState({ settingsDefaultsLoaded: settingsLoaded });
  useUIStore.setState({ routingFeatureAvailable: routing !== 'unavailable' });
  useRoutingStore.setState({ loaded: routing !== 'unknown', available: routing === 'ready', autoReady: routing === 'ready' });
  const selected = resolveDefaultAgentModelSelection({
    ...defaults, agents: entries.map(toAgent), providers: catalog.map(toProvider),
    autoReady: selectAutoReady(useRoutingStore.getState()),
    staleAuto: isStaleAutoSelection('openchamber', 'auto'),
    fallbackPrefs: {
      lastSelectedModel: defaults.lastSelectedModel,
      hiddenModels: (defaults.hiddenModels ?? []).map(([providerID, modelID]) => ({ providerID, modelID })),
    },
  });
  return { agentName: selected.agentName ?? null, model: selected.providerId && selected.modelId ? {
    providerId: selected.providerId, modelId: selected.modelId, variant: selected.variant ?? null,
  } : null };
};

const normalizeModel = (model: Model) => ({
  id: model.id, providerModelId: model.modelID === model.id ? null : model.modelID,
  name: model.name || null, variants: listModelVariantIds(model.variants),
});

/** Renders every scenario using the production TypeScript implementations. */
export const renderSendConfigFixtures = (): string => {
  const config = useConfigStore.getState();
  const ui = useUIStore.getState();
  const routing = useRoutingStore.getState();
  try {
    const groupingInputs = [
      { name: 'disabled providers and models, orphan models and empty providers', providers: [
        { id: 'p', name: 'Provider', enabled: true }, { id: 'disabled', enabled: false },
        { id: 'empty', enabled: true }, { id: 'only-disabled', enabled: true },
      ], models: [
        { id: 'one', providerId: 'p', enabled: true }, { id: 'off', providerId: 'p', enabled: false },
        { id: 'hidden-provider', providerId: 'disabled', enabled: true }, { id: 'orphan', providerId: 'absent', enabled: true },
        { id: 'off', providerId: 'only-disabled', enabled: false },
        { id: 'fast', providerId: 'p', providerModelId: 'one', name: 'Fast', variants: ['', 'high'], enabled: true },
      ] },
      { name: 'catalog ordering', providers: [{ id: 'z', enabled: true }, { id: 'a', enabled: true }], models: [
        { id: 'a1', providerId: 'a', enabled: true }, { id: 'z2', providerId: 'z', enabled: true }, { id: 'z1', providerId: 'z', enabled: true },
      ] },
      { name: 'empty catalog', providers: [], models: [] },
    ];
    const grouping = groupingInputs.map((input) => {
      const entries: Provider[] = input.providers.map((entry) => ({ id: entry.id, name: entry.name ?? '', package: entry.id, activation: entry.enabled ? 'enabled' : 'disabled' }));
      const models = input.models.map((entry) => ({ ...toModel(entry.providerId, entry), enabled: entry.enabled }));
      return { name: input.name, input, expected: groupProviderCatalog(entries, models).map((provider) => ({ id: provider.id, name: provider.name || null, models: provider.models.map(normalizeModel) })) };
    });
    const labelInputs = [
      { name: 'catalog name', providers, choice: { providerId: 'p', modelId: 'catalog' } },
      { name: 'long catalog name', providers: [{ id: 'p', models: [{ id: 'long', name: 'A catalog name that is deliberately longer than forty characters' }] }], choice: { providerId: 'p', modelId: 'long' } },
      { name: 'UTF-16 catalog truncation at a complete character', providers: [{ id: 'p', models: [{ id: 'unicode', name: `a${'😀'.repeat(19)}tail` }] }], choice: { providerId: 'p', modelId: 'unicode' } },
      { name: 'blank catalog name', providers: [{ id: 'p', models: [{ id: 'gpt-6-1', name: '\ufeff  ' }] }], choice: { providerId: 'p', modelId: 'gpt-6-1' } },
      { name: 'exact derived display key', providers: [{ id: 'p', models: [{ id: 'fast', providerModelId: 'base', name: 'Fast' }, { id: 'base', name: 'Normal' }] }], choice: { providerId: 'p', modelId: 'base' } },
      ...['gpt-6-1-sol', '~vendor/deepseekV3-0324:free', 'org/claude-opus-4-6-2026-02-05', 'qwen-3-5-235b-non-reasoning:thinking', 'glm-4-7', 'lfm-2-5', 'gpt-oss-120b', 'qwen3.5-30b', 'o4-mini', 'r2d2-7b', 'ernie-api-ai-vl-vlm', 'llama-8x7b-4.5b-32n', 'minimax-m2-1', 'a-very-long-model-identifier-that-will-need-to-be-truncated', '', '  org/model_with   spaces ', 'emoji-😀-agent', '𐐨model-𐐀name', 'foo:custom:other'].map((id) => ({ name: `humanized ${JSON.stringify(id)}`, providers: [], choice: { providerId: 'absent', modelId: id } })),
    ];
    const modelLabels = labelInputs.map((input) => ({ name: input.name, input, expected: getProviderModelDisplayName(input.providers.map(toProvider).find((provider) => provider.id === input.choice.providerId), input.choice.modelId, { maxLength: 40 }) }));
    const agentInputs: FixtureAgent[] = [
      { name: 'build', displayName: '  Builder  ' }, { name: 'plan', displayName: '   ' },
      { name: 'custom-agent' }, { name: '' }, { name: 'ßagent' }, { name: '😀agent' },
      { name: '𐐨agent' }, { name: 'hidden', hidden: true }, { name: 'legacy', optionsHidden: true },
    ];
    const agentLabels = agentInputs.map((input) => ({ input, expected: agentLabel(toAgent(input)), hidden: isAgentHidden(toAgent(input)) }));
    const roster: FixtureAgent[] = [
      { name: 'build' }, { name: 'architect' }, { name: 'plan', color: ' #123AbC ' },
      { name: 'helper', mode: 'subagent' }, { name: 'hidden', hidden: true, color: '#112233' },
      ...Array.from({ length: 12 }, (_, index) => ({ name: `agent-${index}` })),
      { name: '😀agent' }, { name: 'migrated', color: '#AAAAAA' }, { name: 'invalid', color: '#abc' },
    ];
    const colorInputs = [getDefaultTheme(true), getDefaultTheme(false), getThemeById('monokai-dark') ?? getDefaultTheme(true)].flatMap((theme) => [
      { theme, agents: roster },
      { theme, agents: [{ name: 'build', color: '#112233' }, { name: 'plan', color: 'primary' }] },
    ]);
    const sparse = structuredClone(getDefaultTheme(true));
    for (const key of ['keyword', 'type', 'function', 'number', 'string', 'operator', 'variable'] as const) sparse.colors.syntax.base[key] = '#dddddd';
    colorInputs.push({ theme: sparse, agents: roster });
    const colors = colorInputs.map(({ theme, agents: entries }, index) => {
      const palette = agentColorPalette(theme);
      const color = createAgentColorResolver(theme, filterVisibleAgents(entries.map(toAgent)));
      return {
        name: `${theme.metadata.id} palette ${index}`, agents: entries, syntaxRoles: 7,
        palette: palette.map(({ index, colorful }) => ({ index, colorful })),
        expected: [undefined, '', 'build', ...entries.map((agent) => agent.name), 'missing'].map((name) => {
          const resolved = color(name).color;
          const role = palette.find((entry) => entry.color === resolved);
          return { name: name ?? null, color: resolved.startsWith('#') ? { kind: 'configured', value: resolved } : resolved === 'var(--status-success)' ? { kind: 'success' } : { kind: 'syntax', index: role?.index, of: 7 } };
        }),
      };
    });
    const selections = selectionScenarios.map((entry) => ({ ...entry, expected: runSelection(entry.input) }));
    return `${JSON.stringify({ selections, grouping, modelLabels, agentLabels, colors }, null, 2)}\n`;
  } finally {
    useConfigStore.setState({ settingsDefaultsLoaded: config.settingsDefaultsLoaded });
    useUIStore.setState({ routingFeatureAvailable: ui.routingFeatureAvailable });
    useRoutingStore.setState({ loaded: routing.loaded, available: routing.available, autoReady: routing.autoReady });
  }
};

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  const target = resolve(repoRoot, SEND_CONFIG_FIXTURE_PATH);
  mkdirSync(dirname(target), { recursive: true });
  writeFileSync(target, renderSendConfigFixtures(), 'utf8');
  console.log(`wrote ${target}`);
}
