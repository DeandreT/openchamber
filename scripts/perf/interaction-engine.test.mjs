import assert from 'node:assert/strict';
import { existsSync, readFileSync, realpathSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { setTimeout as wait } from 'node:timers/promises';
import test from 'node:test';
import { OpenCode } from '@opencode/client';
import { INTERACTION_SHELL_COMMAND } from './fixture-provider.mjs';
import { REPO_ROOT, withInteractionServer } from './isolated-server.mjs';

// Explicit opt-in: this starts the bundled engine and answers only its own
// exact synthetic request after checking session, directory and resources.
test('real engine permissions and question forms in a fresh private directory', {
  skip: !process.env.OPENCODE_INTERACTION_BINARY, timeout: 180000,
}, async (t) => {
  const root = process.env.OPENCODE_INTERACTION_ROOT ?? `/tmp/opencode/interaction-engine-${process.pid}-${Date.now()}`;
  t.diagnostic(`Private evidence: ${root}`);
  let server;
  const listenersBefore = ['SIGINT', 'SIGTERM'].map((name) => process.listenerCount(name));
  await withInteractionServer({ root,
    dist: process.env.OPENCODE_INTERACTION_DIST ?? join(REPO_ROOT, 'packages/electron/resources/web-dist'),
    opencodeBinary: process.env.OPENCODE_INTERACTION_BINARY,
  }, async (owned, signal) => {
  server = owned;
  writeFileSync(join(root, 'smoke-ready.json'), JSON.stringify({ url: server.url, pid: server.pid,
    fixturePort: server.fixturePort, fixturePid: server.fixturePid }));
  const client = OpenCode.make({ baseUrl: server.url,
    headers: { 'x-opencode-directory': encodeURIComponent(server.project) },
    fetch: (url, options) => fetch(url, { ...options, signal: AbortSignal.any([signal, AbortSignal.timeout(10000)]) }),
  });
  async function until(label, read) {
    const deadline = Date.now() + 30000;
    while (Date.now() < deadline) {
      signal.throwIfAborted();
      const value = await read();
      if (value) return value;
      await wait(100);
    }
    throw new Error(`Timed out: ${label}; logs in ${root}`);
  }
  async function create(model) {
    const session = await client.session.create({ title: `Fixture ${model}`, agent: 'build',
      model: { providerID: 'perf', id: model }, location: { directory: server.project } });
    assert.equal(session.location.directory, server.project);
    return session.id;
  }
  async function prompt(sessionID) {
    await client.session.prompt({ sessionID, text: 'Run the synthetic fixture interaction.' });
  }
  async function permission(sessionID) {
    const request = await until('owned permission', async () => (await client.permission.list({ sessionID }))[0]);
    assert.equal((await client.session.get({ sessionID })).location.directory, realpathSync(server.project));
    assert.equal(request.sessionID, sessionID);
    assert.equal(request.action, 'shell');
    assert.deepEqual(request.resources, [INTERACTION_SHELL_COMMAND]);
    return request.id;
  }
  async function toolStates(sessionID, name) {
    const messages = (await client.message.list({ sessionID, limit: 100 })).data;
    return messages.filter((message) => message.type === 'assistant').flatMap((message) => message.content)
      .filter((part) => part.type === 'tool' && part.name === name).map((part) => part.state);
  }
  async function shellCompleted(sessionID, count) {
    return until('shell completed', async () => {
      const states = (await toolStates(sessionID, 'shell')).filter((state) => state.status === 'completed');
      if (states.length !== count) return false;
      assert.ok(states.every((state) => state.content.some((part) => part.type === 'text' && part.text.includes('native-permission-ok'))));
      return true;
    });
  }
    assert.equal(statSync(root).mode & 0o777, 0o700);
    assert.equal(existsSync(join(server.project, '.git')), false);
    for (const pid of [server.pid, server.fixturePid]) {
      const env = Object.fromEntries(readFileSync(`/proc/${pid}/environ`, 'utf8').split('\0').filter(Boolean).map((line) => {
        const index = line.indexOf('='); return [line.slice(0, index), line.slice(index + 1)];
      }));
      assert.equal(env.HOME, join(server.state, 'home'));
      assert.equal(env.XDG_CONFIG_HOME, join(server.state, 'config'));
      assert.equal(env.OPENAI_API_KEY, undefined);
      assert.equal(env.OPENCODE_PASSWORD, undefined);
      assert.equal(realpathSync(`/proc/${pid}/cwd`), server.project);
    }
    await until('OpenCode ready', async () => (await (await fetch(`${server.url}/health`, { signal })).json()).isOpenCodeReady);

    const once = await create('interaction-shell');
    await prompt(once);
    await client.permission.reply({ sessionID: once, requestID: await permission(once), decision: 'once' });
    await shellCompleted(once, 1);
    await client.session.wait({ sessionID: once });
    await prompt(once);
    await client.permission.reply({ sessionID: once, requestID: await permission(once), decision: 'reject' });
    await client.session.wait({ sessionID: once });
    assert.equal((await toolStates(once, 'shell')).filter((state) => state.status === 'completed').length, 1);
    assert.ok((await toolStates(once, 'shell')).some((state) => state.status === 'error'));
    assert.equal((await client.permission.list({ sessionID: once })).length, 0);
    assert.equal((await client.permission.saved.list({ projectID: (await client.session.get({ sessionID: once })).projectID })).length, 0);

    const always = await create('interaction-shell');
    await prompt(always);
    await client.permission.reply({ sessionID: always, requestID: await permission(always), decision: 'always' });
    await shellCompleted(always, 1);
    await client.session.wait({ sessionID: always });
    const session = await client.session.get({ sessionID: always });
    assert.ok((await client.permission.saved.list({ projectID: session.projectID })).some((rule) => rule.action === 'shell'));
    await prompt(always);
    await shellCompleted(always, 2);
    await client.session.wait({ sessionID: always });
    assert.equal((await client.permission.list({ sessionID: always })).length, 0);

    const question = await create('interaction-question');
    async function form() {
      const value = await until('owned question form', async () => (await client.session.form.list({ sessionID: question }))[0]);
      assert.equal(value.sessionID, question);
      assert.equal(value.metadata.kind, 'question');
      assert.deepEqual(value.fields.map((field) => [field.key, field.type, field.custom]), [
        ['q0', 'string', true], ['q1', 'multiselect', true], ['q2', 'string', true],
      ]);
      assert.deepEqual(value.fields[0].options.map((option) => option.value), ['Alpha', 'Beta']);
      assert.equal((await client.session.get({ sessionID: question })).location.directory, server.project);
      return value.id;
    }
    await prompt(question);
    const formID = await form();
    const answer = { q0: 'Beta', q1: ['One', 'Two'], q2: 'Fixture 你好' };
    await client.session.form.reply({ sessionID: question, formID, answer });
    assert.deepEqual((await client.session.form.get({ sessionID: question, formID })).state, { status: 'answered', answer });
    await client.session.wait({ sessionID: question });
    const answered = (await toolStates(question, 'question')).find((state) => state.status === 'completed');
    assert.deepEqual(answered.metadata.answers, [['Beta'], ['One', 'Two'], ['Fixture 你好']]);
    await prompt(question);
    const cancelledID = await form();
    await client.session.form.cancel({ sessionID: question, formID: cancelledID });
    assert.equal((await client.session.form.get({ sessionID: question, formID: cancelledID })).state.status, 'cancelled');
    await client.session.wait({ sessionID: question });
    assert.equal((await client.session.form.list({ sessionID: question })).length, 0);
  });
  assert.deepEqual(['SIGINT', 'SIGTERM'].map((name) => process.listenerCount(name)), listenersBefore);
  for (const port of [new URL(server.url).port, server.fixturePort]) {
    await assert.rejects(fetch(`http://127.0.0.1:${port}/health`, { signal: AbortSignal.timeout(1000) }));
  }
});
