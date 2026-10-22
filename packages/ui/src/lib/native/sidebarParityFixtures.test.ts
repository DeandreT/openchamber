import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { renderSidebarParityFixtures, SIDEBAR_PARITY_FIXTURE_PATH } from './sidebarParityFixtures';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..', '..', '..');

describe('native sidebar parity fixtures', () => {
  test('the checked-in fixture matches the reference projection', async () => {
    const checkedIn = readFileSync(resolve(repoRoot, SIDEBAR_PARITY_FIXTURE_PATH), 'utf8');
    // Regenerate with `bun run native:fixtures` after changing the projection.
    expect(checkedIn).toBe(await renderSidebarParityFixtures());
  });
});
