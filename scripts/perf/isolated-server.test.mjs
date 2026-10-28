import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { existsSync, readFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import test from 'node:test';
import { reservePort } from './cdp.mjs';
import { isolatedOpencodeConfig, isolatedServerEnv, startFixture, startInteractionServer, withInteractionServer } from './isolated-server.mjs';
import { INTERACTION_SHELL_COMMAND } from './fixture-provider.mjs';

const interactionHost = process.platform === 'linux' && existsSync('/tmp/opencode') && !existsSync('/etc/opencode')
  && ['/', '/tmp', '/tmp/opencode'].every((directory) =>
    ['opencode.json', 'opencode.jsonc', '.opencode', 'AGENTS.md', 'CLAUDE.md'].every((name) => !existsSync(join(directory, name))));

test('isolated server replaces every home and data location without inheriting credentials', () => {
  const state = join('scratch', 'state');
  const env = isolatedServerEnv({
    state, dist: 'built-ui', config: { model: 'perf/stream-300cps' }, opencodeBinary: 'bundled-opencode',
    inherited: {
      Path: 'native-tools', SystemRoot: 'Windows', COMSPEC: 'cmd.exe', TEMP: 'scratch-temp',
      HOME: 'real-home', USERPROFILE: 'real-profile', APPDATA: 'real-appdata', LOCALAPPDATA: 'real-local',
      XDG_CONFIG_HOME: 'real-config', XDG_DATA_HOME: 'real-data', XDG_CACHE_HOME: 'real-cache',
      OPENCODE_CONFIG: 'real-config-file', OPENCODE_PASSWORD: 'test-secret', OPENCHAMBER_AGENT_TOKEN: 'test-token',
      OPENAI_API_KEY: 'test-provider-key', OPENCODE_DB: 'real-database', NODE_OPTIONS: '--require=personal-hook',
    },
  });
  assert.equal(env.Path, 'native-tools');
  assert.equal(env.SystemRoot, 'Windows');
  assert.equal(env.COMSPEC, 'cmd.exe');
  assert.equal(env.TEMP, 'scratch-temp');
  assert.equal(env.HOME, join(state, 'home'));
  assert.equal(env.USERPROFILE, env.HOME);
  assert.equal(env.APPDATA, join(env.HOME, 'AppData', 'Roaming'));
  assert.equal(env.LOCALAPPDATA, join(env.HOME, 'AppData', 'Local'));
  assert.equal(env.XDG_CONFIG_HOME, join(state, 'config'));
  assert.equal(env.XDG_DATA_HOME, join(state, 'data'));
  assert.equal(env.XDG_CACHE_HOME, join(state, 'cache'));
  assert.equal(env.XDG_STATE_HOME, join(state, 'xdg-state'));
  assert.equal(env.OPENCHAMBER_DATA_DIR, join(state, 'oc-data'));
  assert.equal(env.OPENCHAMBER_DIST_DIR, 'built-ui');
  assert.equal(env.OPENCODE_BINARY, 'bundled-opencode');
  assert.deepEqual(JSON.parse(env.OPENCODE_CONFIG_CONTENT), { model: 'perf/stream-300cps' });
  for (const key of ['OPENCODE_CONFIG', 'OPENCODE_PASSWORD', 'OPENCHAMBER_AGENT_TOKEN', 'OPENAI_API_KEY', 'OPENCODE_DB', 'NODE_OPTIONS']) {
    assert.equal(env[key], undefined, key);
  }
});

test('a native child resolves its home inside the isolated state', () => {
  const state = join(process.cwd(), 'tmp', 'perf-env-test');
  const env = isolatedServerEnv({ state, dist: 'built-ui', config: {} });
  const home = execFileSync(process.execPath, ['--input-type=module', '-e', "import { homedir } from 'node:os'; process.stdout.write(homedir())"], {
    env, encoding: 'utf8', windowsHide: true,
  });
  assert.equal(home, join(state, 'home'));
});

test('fixture CLI starts and answers a deterministic completion', async () => {
  const port = await reservePort();
  const fixture = await startFixture({ port });
  try {
    assert.equal(fixture.reused, false);
    const response = await fetch(`http://127.0.0.1:${port}/v1/chat/completions`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ model: 'stream-300cps', stream: false, messages: [] }),
      signal: AbortSignal.timeout(5000),
    });
    assert.equal(response.status, 200);
    const result = await response.json();
    assert.equal(result.choices[0].message.content, 'Streaming capture');
  } finally {
    await fixture.stop();
  }
  await assert.rejects(fetch(`http://127.0.0.1:${port}/v1/models`, { signal: AbortSignal.timeout(1000) }));
});

test('interaction configuration uses only the local provider and exact shell permission', () => {
  const original = isolatedOpencodeConfig(1234);
  assert.equal(original.model, 'perf/stream-300cps');
  assert.equal(original.permissions, undefined);
  const config = isolatedOpencodeConfig(1234, { interactions: true });
  assert.deepEqual(config.permissions, [
    { action: '*', resource: '*', effect: 'deny' },
    { action: 'question', resource: '*', effect: 'allow' },
    { action: 'shell', resource: INTERACTION_SHELL_COMMAND, effect: 'ask' },
  ]);
  assert.equal(config.providers.perf.settings.baseURL, 'http://127.0.0.1:1234/v1');
  assert.deepEqual(config.policies, [
    { action: 'provider.use', resource: '*', effect: 'deny' },
    { action: 'provider.use', resource: 'perf', effect: 'allow' },
  ]);
  assert.ok(Object.values(config.agents).every((agent) => agent.model === 'perf/stream-1200cps'));
});

test('interaction launcher refuses a checkout root or an unspecified binary before launching', { skip: !interactionHost }, async () => {
  await assert.rejects(startInteractionServer({ root: join(process.cwd(), 'tmp/interaction'), dist: 'unused', opencodeBinary: process.execPath }), /directly under \/tmp\/opencode/);
  await assert.rejects(startInteractionServer({ root: '/tmp/opencode/interaction-no-binary', dist: 'unused' }), /explicit OpenCode binary/);
});

test('interaction CLI requires the binary flag even when OPENCODE_BINARY is inherited', () => {
  const result = spawnSync(process.execPath, ['scripts/perf/isolated-server.mjs', '--interactions'], {
    env: { PATH: process.env.PATH, OPENCODE_BINARY: process.execPath }, encoding: 'utf8', timeout: 5000,
  });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /requires an explicit --opencode-binary flag/);
});

test('interaction startup failure stops its owned provider and refuses root reuse', { skip: !interactionHost }, async (t) => {
  const root = `/tmp/opencode/interaction-failure-${process.pid}-${Date.now()}`;
  t.after(() => rmSync(root, { recursive: true, force: true }));
  await assert.rejects(startInteractionServer({ root, dist: join(root, 'missing-dist'), opencodeBinary: process.execPath }), /no main chunk/);
  const port = readFileSync(join(root, 'fixture.log'), 'utf8').match(/listening on http:\/\/127\.0\.0\.1:(\d+)/)?.[1];
  assert.ok(port, 'owned provider logged its bound port before startup failed');
  await assert.rejects(fetch(`http://127.0.0.1:${port}/health`, { signal: AbortSignal.timeout(1000) }));
  await assert.rejects(startInteractionServer({ root, dist: 'unused', opencodeBinary: process.execPath }), /EEXIST/);
});

test('scoped interaction startup failure removes its process signal listeners', { skip: !interactionHost }, async () => {
  const before = ['SIGINT', 'SIGTERM'].map((name) => process.listenerCount(name));
  await assert.rejects(withInteractionServer({ root: '/not-an-interaction-root', dist: 'unused' }, () => {
    assert.fail('invalid startup must not enter the callback');
  }), /directly under/);
  assert.deepEqual(['SIGINT', 'SIGTERM'].map((name) => process.listenerCount(name)), before);
});
