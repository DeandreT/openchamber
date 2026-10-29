import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { z } from 'zod';

import { renderTransportFixtures, TRANSPORT_FIXTURE_PATH } from './transportFixtures';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..', '..', '..');

describe('native transport fixtures', () => {
  // Regenerate with `bun run native:transport-fixtures` after changing the
  // pinned client, the projection or a recorded wire sample.
  test('the checked-in fixture matches the official client and the UI projection', async () => {
    expect(readFileSync(resolve(repoRoot, TRANSPORT_FIXTURE_PATH), 'utf8')).toBe(await renderTransportFixtures());
  });

  test('creation captures a root identity and exact lookup has no directory header', async () => {
    const fixture = z.object({
      scopedDirectory: z.string(),
      requests: z.array(z.object({
        name: z.string(), method: z.string(), path: z.string(), body: z.string().nullable(),
        directoryHeader: z.string().nullable(), query: z.array(z.tuple([z.string(), z.string()])),
      })),
      creation: z.object({ created: z.unknown(), observed: z.unknown() }),
    }).parse(JSON.parse(await renderTransportFixtures()));
    const create = fixture.requests.find((request) => request.name === 'create root session');
    const defaults = fixture.requests.find((request) => request.name === 'create root session with defaults');
    const lookup = fixture.requests.find((request) => request.name === 'observe exact session');
    if (!create?.body || !defaults?.body || !lookup) throw new Error('missing session creation fixtures');
    expect(JSON.parse(create.body)).toEqual({
      id: 'ses_e9b49c443ffefXcWWPhCOga6nH',
      location: { directory: fixture.scopedDirectory },
      model: { providerID: 'fixture', id: 'text-model', variant: 'high' },
      agent: 'build',
    });
    expect(JSON.parse(defaults.body)).toEqual({
      id: 'ses_e9b49c443ffefXcWWPhCOga6nH', location: { directory: fixture.scopedDirectory },
    });
    expect(lookup.method).toBe('GET');
    expect(lookup.path).toBe('/api/session/ses_e9b49c443ffefXcWWPhCOga6nH');
    expect(lookup.directoryHeader).toBeNull();
    expect(lookup.query).toEqual([]);
    expect(fixture.creation.observed).toEqual(fixture.creation.created);
  });

  test('blocking requests preserve global ownership and explicit mutation shapes', async () => {
    const fixture = z.object({
      client: z.literal('@opencode/client@2.2.0'),
      scopedDirectory: z.string(),
      blockingRequests: z.object({ requests: z.array(z.object({
        name: z.string(), method: z.string(), path: z.string(), directoryHeader: z.string().nullable(),
        body: z.unknown(), query: z.array(z.tuple([z.string(),z.string()])),
      })) }),
    }).parse(JSON.parse(await renderTransportFixtures()));
    const requests = fixture.blockingRequests.requests;
    const cancel = requests.find((request) => request.name === 'global form cancel');
    expect(cancel).toMatchObject({ method: 'DELETE', path: '/api/session/global/form/frm_native_fixture',
      directoryHeader: encodeURIComponent(fixture.scopedDirectory), body: null, query: [] });
    expect(requests.find((request) => request.name === 'permission always')?.body).toEqual({ decision: 'always' });
    expect(requests.find((request) => request.name === 'form cancel feedback')?.query).toEqual([['message','No answer available']]);
    expect(requests.filter((request) => request.method === 'POST')).toHaveLength(6);
  });
});
