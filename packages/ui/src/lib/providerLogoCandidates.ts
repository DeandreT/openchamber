/**
 * Which logo files may stand for a provider: its normalized ID, the part
 * before any `/` or `:`, and the aliases below, in lookup order. The native
 * desktop exports the aliases and ports the lookup, so both clients pick the
 * same logo.
 *
 * @public Read by the native resource export (`lib/native/nativeResources.ts`).
 */
export const PROVIDER_LOGO_ALIASES: ReadonlyMap<string, string> = new Map([
    ['codex', 'openai'],
    ['chatgpt', 'openai'],
    ['claude', 'anthropic'],
    ['cline-pass', 'cline'],
    ['gemini', 'google'],
    ['evroc-ai', 'evroc'],
    ['evrocai', 'evroc'],
    ['ollama-cloud', 'ollama'],
    ['wafer-ai', 'wafer.ai'],
    ['wafer', 'wafer.ai'],
    ['copilot', 'github-copilot'],
]);

const normalizeProviderId = (providerId: string | null | undefined) => {
    return (providerId ?? '')
        .toLowerCase()
        .trim()
        .replace(/^models\./, '')
        .replace(/^provider\./, '')
        .replace(/\s+/g, '-');
};

export const buildProviderLogoCandidates = (providerId: string | null | undefined): string[] => {
    const normalized = normalizeProviderId(providerId);
    if (!normalized) {
        return [];
    }

    const compact = normalized.replace(/[^a-z0-9_\-./:]/g, '');
    const primary = compact.split(/[/:]/)[0] || compact;
    const prefixAlias = compact.startsWith('exe-') ? 'exe-dev' : undefined;
    const candidates = [prefixAlias, PROVIDER_LOGO_ALIASES.get(compact), PROVIDER_LOGO_ALIASES.get(primary), compact, primary]
        .filter((value): value is string => Boolean(value && value.length > 0));

    return [...new Set(candidates)];
};
