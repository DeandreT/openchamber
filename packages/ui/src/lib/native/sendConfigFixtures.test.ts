import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { renderSendConfigFixtures, SEND_CONFIG_FIXTURE_PATH } from './sendConfigFixtures';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..', '..', '..');

describe('native send configuration fixtures', () => {
  test('the checked-in fixture matches the production TypeScript', () => {
    const checkedIn = readFileSync(resolve(repoRoot, SEND_CONFIG_FIXTURE_PATH), 'utf8');
    expect(checkedIn).toBe(renderSendConfigFixtures());
  });
});
