import { describe, expect, test } from 'bun:test';

import { buildProviderLogoCandidates } from './providerLogoCandidates';

describe('provider logo aliases', () => {
    test('maps rotating exe.dev proxy provider IDs to the local exe.dev logo', () => {
        expect(buildProviderLogoCandidates('exe-a1b2c3')[0]).toBe('exe-dev');
    });

    test('aliases the copilot provider id to the bundled github-copilot logo', () => {
        expect(buildProviderLogoCandidates('copilot')[0]).toBe('github-copilot');
    });
});
