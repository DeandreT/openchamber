import { findCatalogModel, type Agent, type Model, type Provider } from '@/lib/opencode/model';
import { parseModelIdentifier, parseModelSelection } from '@/lib/modelIdentifier';
import { listModelVariantIds } from '@/lib/modelVariants';
import { isAutoModel } from '@/lib/routing/autoModel';
import type { ModelRef } from '@/lib/settings/parsers';
import { isAgentHidden } from './useAgentsStore';
import { isPrimaryMode } from '@/components/chat/mobileControlsUtils';

type ProviderWithModelList = Provider & { models: Model[] };

/** Offerable v2 providers with enabled models in their original catalog order. */
export const groupProviderCatalog = (providers: Provider[], models: Model[]): ProviderWithModelList[] => {
    const modelsByProvider = new Map<string, Model[]>();
    for (const model of models) {
        if (!model.enabled) continue;
        const bucket = modelsByProvider.get(model.providerID);
        if (bucket) bucket.push(model);
        else modelsByProvider.set(model.providerID, [model]);
    }
    return providers
        .filter((provider) => provider.activation !== 'disabled' && modelsByProvider.has(provider.id))
        .map((provider) => ({ ...provider, models: modelsByProvider.get(provider.id) ?? [] }));
};

/** Finds exact catalog keys before falling back to provider API model names. */
export const findProviderModel = (providers: ProviderWithModelList[], providerId: string, modelId: string): Model | undefined => (
    findCatalogModel(providers.find((provider) => provider.id === providerId)?.models, modelId)
);

/** Whether a model exposes the requested variant. */
export const modelHasVariant = (model: Model | undefined, variant: string | null | undefined): boolean => (
    variant != null && listModelVariantIds(model?.variants).includes(variant)
);

/** Auto exists only when routing is ready; ordinary models use the active catalog. */
export const catalogHasProviderModel = (providers: ProviderWithModelList[], providerId: string, modelId: string, autoReady: boolean): boolean => (
    isAutoModel(providerId, modelId) ? autoReady : findProviderModel(providers, providerId, modelId) !== undefined
);

/** Preferences used only after configured and pinned model choices are exhausted. */
export type FallbackModelPrefs = { hiddenModels: readonly ModelRef[]; lastSelectedModel: string | undefined };

const isModelHidden = (hiddenModels: readonly ModelRef[], providerId: string, modelId: string): boolean => hiddenModels.some(
    (hidden) => hidden.providerID === providerId && hidden.modelID === modelId,
);

/** A remembered composer model survives catalog gaps unless hidden or confirmed stale Auto. */
export const resolveRememberedModel = ({ hiddenModels, lastSelectedModel }: FallbackModelPrefs, staleAuto: boolean): { providerId: string; modelId: string } | null => {
    const parsed = lastSelectedModel ? parseModelIdentifier(lastSelectedModel) : null;
    if (!parsed || isModelHidden(hiddenModels, parsed.providerId, parsed.modelId)) return null;
    return staleAuto && isAutoModel(parsed.providerId, parsed.modelId) ? null : parsed;
};

/** Big Pickle, then the first visible model; an entirely hidden catalog keeps the unfiltered pick. */
export const resolveFallbackModel = (providers: ProviderWithModelList[], hiddenModels: readonly ModelRef[]): { providerId: string; modelId: string } | null => {
    const isVisible = (providerId: string, modelId: string) => !isModelHidden(hiddenModels, providerId, modelId);
    const hasBigPickle = findProviderModel(providers, 'opencode', 'big-pickle') !== undefined;
    if (hasBigPickle && isVisible('opencode', 'big-pickle')) return { providerId: 'opencode', modelId: 'big-pickle' };
    for (const provider of providers) {
        const visibleModel = provider.models.find((model) => isVisible(provider.id, model.modelID));
        if (visibleModel) return { providerId: provider.id, modelId: visibleModel.modelID };
    }
    if (hasBigPickle) return { providerId: 'opencode', modelId: 'big-pickle' };
    const firstProvider = providers[0];
    const firstModel = firstProvider?.models[0];
    return firstProvider && firstModel ? { providerId: firstProvider.id, modelId: firstModel.modelID } : null;
};

type DefaultAgentModelSelection = {
    agentName: string | undefined;
    providerId?: string;
    modelId?: string;
    variant?: string;
};

/** Startup and fresh-draft defaults with routing readiness captured by the caller. */
export const resolveDefaultAgentModelSelection = ({
    agents, providers, projectDefaultAgent, projectDefaultModel, projectDefaultVariant,
    settingsDefaultAgent, settingsDefaultModel, settingsDefaultVariant,
    opencodeDefaultAgent, opencodeDefaultModel, allowFallback = true, fallbackPrefs,
    autoReady, staleAuto,
}: {
    agents: Agent[]; providers: ProviderWithModelList[];
    projectDefaultAgent?: string; projectDefaultModel?: string; projectDefaultVariant?: string;
    settingsDefaultAgent?: string; settingsDefaultModel?: string; settingsDefaultVariant?: string;
    opencodeDefaultAgent?: string; opencodeDefaultModel?: string; allowFallback?: boolean;
    fallbackPrefs: FallbackModelPrefs; autoReady: boolean; staleAuto: boolean;
}): DefaultAgentModelSelection => {
    const resolveVariant = (providerId: string, modelId: string, variant?: string): string | undefined => (
        variant && modelHasVariant(findProviderModel(providers, providerId, modelId), variant) ? variant : undefined
    );
    const hasModel = (providerId: string, modelId: string) => catalogHasProviderModel(providers, providerId, modelId, autoReady);
    const primaryAgents = agents.filter((agent) => isPrimaryMode(agent.mode) && !isAgentHidden(agent));
    let resolvedAgent: Agent | undefined;
    if (projectDefaultAgent) resolvedAgent = agents.find((agent) => agent.name === projectDefaultAgent);
    if (!resolvedAgent && settingsDefaultAgent) resolvedAgent = agents.find((agent) => agent.name === settingsDefaultAgent);
    if (!resolvedAgent && opencodeDefaultAgent) {
        const candidate = agents.find((agent) => agent.name === opencodeDefaultAgent);
        if (candidate && isPrimaryMode(candidate.mode) && !isAgentHidden(candidate)) resolvedAgent = candidate;
    }
    if (!resolvedAgent) resolvedAgent = primaryAgents.find((agent) => agent.name === 'build') || primaryAgents[0] || agents[0];

    let providerId: string | undefined;
    let modelId: string | undefined;
    let variant: string | undefined;
    const effectiveDefaultModel = projectDefaultModel || settingsDefaultModel;
    if (effectiveDefaultModel) {
        const parsed = parseModelIdentifier(effectiveDefaultModel);
        if (parsed && !(staleAuto && isAutoModel(parsed.providerId, parsed.modelId))) {
            providerId = parsed.providerId;
            modelId = parsed.modelId;
            const configuredVariant = projectDefaultModel ? projectDefaultVariant : settingsDefaultVariant;
            variant = hasModel(providerId, modelId) ? resolveVariant(providerId, modelId, configuredVariant) : configuredVariant;
        }
    }
    if (providerId || !allowFallback) return { agentName: resolvedAgent?.name ?? projectDefaultAgent ?? settingsDefaultAgent, providerId, modelId, variant };
    if (!providerId && resolvedAgent?.model?.providerID && resolvedAgent.model?.id) {
        providerId = resolvedAgent.model.providerID;
        modelId = resolvedAgent.model.id;
        variant = hasModel(providerId, modelId) ? resolveVariant(providerId, modelId, resolvedAgent.model.variant) : resolvedAgent.model.variant;
    }
    if (!providerId && opencodeDefaultModel) {
        const parsed = parseModelSelection(opencodeDefaultModel);
        if (parsed) {
            providerId = parsed.providerID;
            modelId = parsed.modelID;
            variant = hasModel(providerId, modelId) ? resolveVariant(providerId, modelId, parsed.variant) : parsed.variant;
        }
    }
    if (!providerId) {
        const fallback = resolveRememberedModel(fallbackPrefs, staleAuto) ?? resolveFallbackModel(providers, fallbackPrefs.hiddenModels);
        providerId = fallback?.providerId;
        modelId = fallback?.modelId;
    }
    return { agentName: resolvedAgent?.name, providerId, modelId, variant };
};
