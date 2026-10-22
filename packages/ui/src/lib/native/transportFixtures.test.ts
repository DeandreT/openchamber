import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { renderTransportFixtures, TRANSPORT_FIXTURE_PATH } from './transportFixtures';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..', '..', '..');

describe('native transport fixtures', () => {
  // Regenerate with `bun run native:transport-fixtures` after changing the
  // pinned client, the projection or a recorded wire sample.
  test('the checked-in fixture matches the official client and the UI projection', async () => {
    expect(readFileSync(resolve(repoRoot, TRANSPORT_FIXTURE_PATH), 'utf8')).toBe(await renderTransportFixtures());
  });
});
