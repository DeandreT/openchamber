import { describe, expect, test } from 'bun:test';

import { getStreamingThrottleText } from '../../hooks/useStreamingTextThrottle';
import { commitStreamedText } from '../../lib/streamTextCommit';
import { resolveAssistantTextStreaming } from './assistantTextVisibility';

describe('resolveAssistantTextStreaming', () => {
    test('streams live text whose part is not sealed', () => {
        expect(resolveAssistantTextStreaming({
            streamPhase: 'streaming',
            isFinalized: false,
        })).toBe(true);
        expect(resolveAssistantTextStreaming({
            streamPhase: 'cooldown',
            isFinalized: false,
        })).toBe(true);
    });

    test('releases a sealed part even while the turn stays blocked (#3277)', () => {
        // A message blocked on a pending question keeps the stream phase
        // forever; the pre-question text part is sealed (time.end is set the
        // moment the model moves on to the question) and must render in full.
        expect(resolveAssistantTextStreaming({
            streamPhase: 'streaming',
            isFinalized: true,
        })).toBe(false);
        expect(resolveAssistantTextStreaming({
            streamPhase: 'cooldown',
            isFinalized: true,
        })).toBe(false);
    });
});

describe('assistant reply samples', () => {
    test('holds the unfinished block while streaming', () => {
        const text = 'line nine, fully written\n\nline ten, still held';
        expect(getStreamingThrottleText('', text, true, false, commitStreamedText)).toBe('line nine, fully written\n\n');
    });

    test('turning partial replies off keeps the visible prefix until the next block is ready', () => {
        const partial = 'An unfinished';
        const shown = getStreamingThrottleText('', partial, true, false);
        expect(shown).toBe(partial);
        const more = `${partial} paragraph`;
        expect(getStreamingThrottleText(shown, more, true, false, commitStreamedText)).toBe(partial);
        expect(getStreamingThrottleText(shown, `${more}.\n\nNext`, true, false, commitStreamedText)).toBe(`${more}.\n\n`);
    });

    test('reveals the full text once the part is not streaming', () => {
        const text = 'one\ntwo\nsealed tail without trailing newline';
        expect(getStreamingThrottleText('one', text, false, false, commitStreamedText)).toBe(text);
        expect(getStreamingThrottleText('one', text, false, false)).toBe(text);
    });
});
