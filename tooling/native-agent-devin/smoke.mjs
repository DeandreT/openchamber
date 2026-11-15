import { spawn, spawnSync } from 'node:child_process';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { createServer } from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomUUID } from 'node:crypto';
import assert from 'node:assert/strict';
import { watchSmokeChild } from './smoke-child.mjs';

const executable = process.argv[2];
if (!executable) throw new Error('Pass the installed OpenCode executable path.');
const root = await mkdtemp(path.join(process.env.TEMP, 'opencode', 'devin-plugin-'));
const directory = path.join(root, 'project');
await mkdir(directory);
const plugin = fileURLToPath(new URL('.', import.meta.url));
await writeFile(path.join(directory, 'opencode.json'), JSON.stringify({ plugins: [{ package: plugin, options: { mode: 'cloud' } }] }));
const env = Object.fromEntries(['PATH', 'SystemRoot', 'WINDIR', 'COMSPEC', 'PATHEXT', 'TEMP', 'TMP'].flatMap((key) => process.env[key] ? [[key, process.env[key]]] : []));
Object.assign(env, { HOME: path.join(root, 'home'), USERPROFILE: path.join(root, 'home'), APPDATA: path.join(root, 'appdata'), LOCALAPPDATA: path.join(root, 'local'), XDG_CONFIG_HOME: path.join(root, 'config'), XDG_DATA_HOME: path.join(root, 'data'), XDG_CACHE_HOME: path.join(root, 'cache'), XDG_STATE_HOME: path.join(root, 'state') });
for (const value of Object.values(env).filter((value) => value.startsWith(root))) await mkdir(value, { recursive: true });
env.OPENCODE_PASSWORD = randomUUID();
const version = spawnSync(executable, ['--version'], { env, encoding: 'utf8', windowsHide: true, timeout: 15_000 });
if (version.error) throw version.error;
assert.equal(version.status, 0, 'OpenCode version probe');
const runtimeVersion = version.stdout.trim();
const listener = createServer();
await new Promise((resolve) => listener.listen(0, 'localhost', resolve));
const port = listener.address().port;
await new Promise((resolve) => listener.close(resolve));
const child = spawn(executable, ['serve', '--hostname', 'localhost', '--port', String(port)], { cwd: directory, env, stdio: 'ignore', windowsHide: true });
const watched = watchSmokeChild(child);
console.log('isolatedRoot', root);
const headers = { 'x-opencode-directory': encodeURIComponent(directory), authorization: `Basic ${Buffer.from(`opencode:${env.OPENCODE_PASSWORD}`).toString('base64')}` };
const base = `http://localhost:${port}`;
const get = async (route) => {
  const response = await fetch(base + route, { headers });
  assert.equal(response.status, 200, route);
  return response.json();
};
let summary;
let failure;
try {
  let started = false;
  for (let count = 0; count < 120; count++) {
    try { const response = await fetch(base + '/openapi.json', { headers }); if (response.ok) { started = true; break; } } catch {}
    if (watched.outcome?.error) throw watched.outcome.error;
    if (watched.outcome) throw new Error('Isolated OpenCode exited during startup.');
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  assert(started, 'Isolated OpenCode startup');
  const sessions = await get('/api/session');
  assert.deepEqual(sessions.data, []);
  const providers = await get('/api/provider');
  assert.deepEqual(providers.data.find((provider) => provider.id === 'devin')?.settings.openchamber.nativeAgent, { protocolVersion: 1 });
  const plugins = await get('/api/plugin');
  assert.equal(plugins.data.find((plugin) => plugin.id === 'openchamber.devin')?.state.status, 'active');
  const api = await get('/openapi.json');
  assert(api.components.schemas['Rpc.Input'].properties.input);
  const rpc = async (method, requestID) => {
    const response = await fetch(base + `/api/rpc/openchamber.native-agent.devin.v1/${method}`, {
      method: 'POST', headers: { ...headers, 'content-type': 'application/json' },
      body: JSON.stringify({ input: { protocolVersion: 1, requestID, scope: { directory, providerID: 'devin', sessionID: null }, activationID: null } }),
    });
    if (!response.ok) console.log('rpcError', await response.text());
    assert.equal(response.status, 200, method);
    return response.json();
  };
  const initial = (await rpc('getState', 'read-smoke')).output;
  assert.equal(initial.state.authentication, 'required');
  assert.equal(initial.state.execution, null);
  assert.equal(initial.state.configComplete, false);
  assert.equal(initial.state.capabilities.prepareAfterSession, true);
  const prepared = (await rpc('prepare', 'prepare-smoke')).output;
  assert.equal(prepared.ok, true);
  assert.equal(prepared.state.status, 'unprepared');
  assert.equal(prepared.state.execution, null);
  assert.equal(prepared.state.configComplete, false);
  const configuredResponse = await fetch(base + '/api/rpc/openchamber.native-agent.devin.v1/setConfig', {
    method: 'POST', headers: { ...headers, 'content-type': 'application/json' },
    body: JSON.stringify({ input: { protocolVersion: 1, requestID: 'config-smoke', scope: initial.state.scope,
      activationID: initial.state.activationID, expectedRevision: prepared.state.revision, changes: [{ id: 'runtime', value: 'local' }] } }),
  });
  const configured = (await configuredResponse.json()).output;
  assert.equal(configured.ok, true);
  assert.equal(configured.state.execution, null);
  assert.equal(configured.state.configComplete, false);
  const models = await get('/api/model');
  assert(models.data.filter((model) => model.providerID === 'devin').every((model) => model.variants.length === 0));
  await writeFile(path.join(directory, 'opencode.json'), JSON.stringify({ plugins: [{ package: plugin, options: { mode: 'cloud', smokeReload: 1 } }] }));
  let reloaded = false;
  for (let count = 0; count < 80; count++) {
    try {
      const state = (await rpc('getState', `reload-${count}`)).output.state;
      if (state.activationID !== initial.state.activationID) { reloaded = true; break; }
    } catch {}
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  assert(reloaded, 'Configuration change reloads the plugin activation');
  const restoredModels = await get('/api/model');
  assert(restoredModels.data.filter((model) => model.providerID === 'devin').every((model) => model.variants.length === 0));
  const restored = (await rpc('getState', 'restored-draft-config')).output;
  assert.equal(restored.state.config.find((field) => field.id === 'runtime')?.value, 'local');
  assert.equal(restored.state.execution, null);
  assert.equal(restored.state.configComplete, false);
  if (watched.outcome?.error) throw watched.outcome.error;
  if (watched.outcome) throw new Error('Isolated OpenCode exited before cleanup.');
  summary = { runtimeVersion, plugin: 'active', protocolVersion: 1, initialAuthentication: initial.state.authentication,
    prepareStatus: prepared.state.status, execution: configured.state.execution, draftConfigurationOnly: true,
    draftVariantPublished: false, draftConfigRetainedAfterReload: true, sessions: sessions.data.length, prompts: 0, credentials: 0 };
} catch (error) { failure = error; }
let childExit;
try { childExit = await watched.stop(); }
catch (error) { failure = failure && failure !== error ? new AggregateError([failure, error], 'Smoke checks and child cleanup failed.') : error; }
if (failure) throw failure;
console.log(JSON.stringify({ ...summary, childExit }));
