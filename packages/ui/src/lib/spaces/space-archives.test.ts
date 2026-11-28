import { afterEach, expect, test } from 'bun:test';

import { isSpaceArchiveDirectory, refreshSpaceArchives, useSpaceArchivesStore } from './space-archives';

const dispatch = { spaceId: 'a1b2c3d4e5f6', name: 'Dispatch', directory: '/archive/a1b2c3d4e5f6' };
const packing = { spaceId: 'b2c3d4e5f6a7', name: 'Packing', directory: '/archive/b2c3d4e5f6a7' };
const originalFetch = globalThis.fetch;
const read = (response: Response) => {
  globalThis.fetch = Object.assign(async () => response, originalFetch);
  return refreshSpaceArchives();
};

afterEach(() => {
  globalThis.fetch = originalFetch;
  useSpaceArchivesStore.getState().resetForRuntimeSwitch();
});

test('a partial archive list retains known owners and applies the readable records', async () => {
  await read(Response.json({ archives: [dispatch, packing] }));
  const renamed = { ...packing, name: 'Packing records' };
  await read(Response.json({ archives: [renamed], complete: false }));
  const owners = useSpaceArchivesStore.getState().byDirectory;
  expect(owners?.get(dispatch.directory)).toEqual(dispatch);
  expect(owners?.get(packing.directory)).toEqual(renamed);

  const failed = Response.json({ code: 'space_archives_unavailable', message: 'Unavailable' }, { status: 503 });
  await expect(read(failed)).rejects.toThrow('Unavailable');
  await expect(read(Response.json({ archives: [], complete: 'false' }))).rejects.toThrow();
  expect(useSpaceArchivesStore.getState().byDirectory).toBe(owners);
  await read(Response.json({ archives: [renamed] }));
  expect(isSpaceArchiveDirectory(dispatch.directory)).toBe(false);
  expect(isSpaceArchiveDirectory(packing.directory)).toBe(true);
  await read(Response.json({ archives: [] }));
  expect(useSpaceArchivesStore.getState().byDirectory?.size).toBe(0);
});

test('a fresh runtime only admits owners confirmed there, even with a partial read in flight', async () => {
  await read(Response.json({ archives: [dispatch] }));
  const previousRuntime = read(Response.json({ archives: [dispatch], complete: false }));
  useSpaceArchivesStore.getState().resetForRuntimeSwitch();
  await previousRuntime;
  expect(useSpaceArchivesStore.getState().byDirectory).toBeNull();

  await read(Response.json({ archives: [packing], complete: false }));
  expect(isSpaceArchiveDirectory(dispatch.directory)).toBe(false);
  expect(isSpaceArchiveDirectory(packing.directory)).toBe(true);
});
