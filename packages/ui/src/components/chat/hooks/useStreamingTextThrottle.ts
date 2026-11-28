import React from 'react';

interface UseStreamingTextThrottleInput {
    text: string;
    isStreaming: boolean;
    throttleMs?: number;
    identityKey?: string;
    allowTextReplacement?: boolean;
    /** Select the visible prefix when a sample is delivered, rather than on each incoming delta. */
    selectStreamingText?: (text: string) => string;
}

const DEFAULT_STREAMING_TEXT_THROTTLE_MS = 100;

export const getStreamingThrottleText = (
    current: string,
    next: string,
    isStreaming: boolean,
    allowTextReplacement: boolean,
    selectStreamingText?: (text: string) => string,
): string => {
    if (!isStreaming) return next;
    const visible = selectStreamingText ? selectStreamingText(next) : next;
    return !allowTextReplacement && current.length > visible.length ? current : visible;
};

const computeStreamingThrottleDelay = (lastEmitAt: number, now: number, throttleMs: number): number => {
    const elapsed = now - lastEmitAt;
    return Math.max(0, throttleMs - elapsed);
};

export const useStreamingTextThrottle = ({
    text,
    isStreaming,
    throttleMs = DEFAULT_STREAMING_TEXT_THROTTLE_MS,
    identityKey,
    allowTextReplacement = false,
    selectStreamingText,
}: UseStreamingTextThrottleInput): string => {
    const [sample, setSample] = React.useState(() => ({
        identityKey,
        text: getStreamingThrottleText('', text, isStreaming, allowTextReplacement, selectStreamingText),
    }));
    const lastEmit = React.useRef({ identityKey, at: 0 });

    React.useEffect(() => {
        const emit = () => {
            lastEmit.current = { identityKey, at: Date.now() };
            setSample((previous) => {
                const samePart = previous.identityKey === identityKey;
                const visible = getStreamingThrottleText(samePart ? previous.text : '', text, isStreaming, allowTextReplacement, selectStreamingText);
                return samePart && previous.text === visible ? previous : { identityKey, text: visible };
            });
        };
        const remaining = computeStreamingThrottleDelay(lastEmit.current.at, Date.now(), throttleMs);
        if (!isStreaming || lastEmit.current.identityKey !== identityKey || remaining === 0) {
            emit();
            return;
        }
        const timer = setTimeout(emit, remaining);
        return () => clearTimeout(timer);
    }, [allowTextReplacement, identityKey, isStreaming, selectStreamingText, text, throttleMs]);

    if (!isStreaming) return text;
    // A recycled row can render its new part before the effect delivers its first sample.
    if (sample.identityKey !== identityKey) return getStreamingThrottleText('', text, true, allowTextReplacement, selectStreamingText);
    return sample.text;
};
