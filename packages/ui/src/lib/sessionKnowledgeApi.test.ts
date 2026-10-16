import { afterEach, describe, expect, spyOn, test } from 'bun:test';
import { reportSessionKnowledgeDelivered, setSessionProjectContextPin } from './sessionKnowledgeApi';

const SPACE_DIRECTORY = '/spaces/a1b2c3d4e5f6/repo';

let request: ReturnType<typeof spyOn<typeof globalThis, 'fetch'>> | null = null;

const capturePaths = (): string[] => {
  const paths: string[] = [];
  request = spyOn(globalThis, 'fetch').mockImplementation(async (input) => {
    paths.push(new URL(input instanceof Request ? input.url : String(input), 'http://app.test').pathname);
    return Response.json({ recorded: true, pins: { notes: ['note-1'], plans: [] } });
  });
  return paths;
};

afterEach(() => {
  request?.mockRestore();
  request = null;
});

// The session lives in the space's OpenCode, so a report or pin that reaches
// the host's server finds no session and changes nothing.
describe('session knowledge writes for a session in an isolated space', () => {
  test('the delivery report reaches the server inside the space', async () => {
    const paths = capturePaths();
    await reportSessionKnowledgeDelivered(SPACE_DIRECTORY, 'session-1', 'signature-1');
    expect(paths).toEqual(['/api/spaces/a1b2c3d4e5f6/session-knowledge/delivered']);
  });

  test('a pin reaches the server inside the space', async () => {
    const paths = capturePaths();
    expect(await setSessionProjectContextPin(SPACE_DIRECTORY, 'session-1', 'note', 'note-1', true)).toEqual({ notes: ['note-1'], plans: [] });
    expect(paths).toEqual(['/api/spaces/a1b2c3d4e5f6/session-knowledge/pin']);
  });

  test('a host session stays on the host', async () => {
    const paths = capturePaths();
    await reportSessionKnowledgeDelivered('/home/me/repo', 'session-1', 'signature-1');
    await setSessionProjectContextPin('/home/me/repo', 'session-1', 'note', 'note-1', true);
    expect(paths).toEqual(['/api/session-knowledge/delivered', '/api/session-knowledge/pin']);
  });
});
