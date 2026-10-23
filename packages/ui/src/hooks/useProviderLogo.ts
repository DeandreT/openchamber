import { useState, useCallback, useEffect } from 'react';
import { buildProviderLogoCandidates } from '@/lib/providerLogoCandidates';

type LogoSource = 'local' | 'remote' | 'none';

interface UseProviderLogoReturn {
    src: string | null;
    onError: () => void;
    hasLogo: boolean;
}

// Vite rewrites `import.meta.glob` at build time into the matching modules. A runtime without
// that transform (the bun test runner) throws on the call instead, so keep it failable and let
// the hook fall back to the remote logo URL it already supports.
const loadLocalLogoModules = (): Record<string, string> => {
    try {
        return import.meta.glob<string>('../assets/provider-logos/*.svg', {
            eager: true,
            import: 'default',
        });
    } catch {
        return {};
    }
};

const localLogoModules = loadLocalLogoModules();

const LOCAL_PROVIDER_LOGO_MAP = new Map<string, string>();
const PRELOADED_LOGO_SRCS = new Set<string>();

const resolveProviderLogoSrc = (providerId: string | null | undefined): string | null => {
    const candidates = buildProviderLogoCandidates(providerId);
    const localResolvedId = candidates.find((candidate) => LOCAL_PROVIDER_LOGO_MAP.has(candidate)) ?? null;
    const localLogoSrc = localResolvedId ? LOCAL_PROVIDER_LOGO_MAP.get(localResolvedId) ?? null : null;
    if (localLogoSrc) {
        return localLogoSrc;
    }

    const remoteResolvedId = candidates[0] ?? null;
    return remoteResolvedId ? `https://models.dev/logos/${remoteResolvedId}.svg` : null;
};

const preloadProviderLogo = (providerId: string | null | undefined): void => {
    if (typeof Image === 'undefined') return;
    const src = resolveProviderLogoSrc(providerId);
    if (!src || PRELOADED_LOGO_SRCS.has(src)) return;

    PRELOADED_LOGO_SRCS.add(src);
    const image = new Image();
    image.decoding = 'async';
    image.onerror = () => {
        PRELOADED_LOGO_SRCS.delete(src);
    };
    image.src = src;
    void image.decode?.().catch(() => undefined);
};

export const preloadProviderLogos = (providerIds: readonly (string | null | undefined)[]): void => {
    for (const providerId of providerIds) {
        preloadProviderLogo(providerId);
    }
};

for (const [path, url] of Object.entries(localLogoModules)) {
    const match = path.match(/provider-logos\/([^/]+)\.svg$/i);
    if (match?.[1] && url) {
        LOCAL_PROVIDER_LOGO_MAP.set(match[1].toLowerCase(), url);
    }
}

export function useProviderLogo(providerId: string | null | undefined): UseProviderLogoReturn {
    const candidates = buildProviderLogoCandidates(providerId);
    const localResolvedId = candidates.find((candidate) => LOCAL_PROVIDER_LOGO_MAP.has(candidate)) ?? null;
    const remoteResolvedId = candidates[0] ?? null;
    const hasLocalLogo = Boolean(localResolvedId);
    const localLogoSrc = localResolvedId ? LOCAL_PROVIDER_LOGO_MAP.get(localResolvedId) ?? null : null;

    const [source, setSource] = useState<LogoSource>(hasLocalLogo ? 'local' : 'remote');

    useEffect(() => {
        setSource(hasLocalLogo ? 'local' : 'remote');
    }, [hasLocalLogo, localResolvedId, remoteResolvedId]);

    const handleError = useCallback(() => {
        setSource((current) => (current === 'local' && hasLocalLogo ? 'remote' : 'none'));
    }, [hasLocalLogo]);

    if (!localResolvedId && !remoteResolvedId) {
        return { src: null, onError: handleError, hasLogo: false };
    }

    if (source === 'local' && localLogoSrc) {
        return {
            src: localLogoSrc,
            onError: handleError,
            hasLogo: true,
        };
    }

    if (source === 'remote' && remoteResolvedId) {
        return {
            src: `https://models.dev/logos/${remoteResolvedId}.svg`,
            onError: handleError,
            hasLogo: true,
        };
    }

    return { src: null, onError: handleError, hasLogo: false };
}
