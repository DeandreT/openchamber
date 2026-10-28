import assert from 'node:assert/strict';
import test from 'node:test';
import { fixtureProviderConfig, startFixtureProvider, INTERACTION_QUESTIONS, INTERACTION_SHELL_COMMAND } from './fixture-provider.mjs';

async function withProvider(interactions, run) {
  const server = await startFixtureProvider(0, { interactions });
  const url = `http://127.0.0.1:${server.address().port}/v1/chat/completions`;
  const request = (body) => fetch(url, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body), signal: AbortSignal.timeout(5000),
  });
  try { await run(request); } finally { await new Promise((resolve) => server.close(resolve)); }
}

const prompt = (name) => ({
  model: `interaction-${name}`, stream: true,
  messages: [{ role: 'user', content: 'Fixture only.' }],
  tools: [{ type: 'function', function: { name, parameters: { type: 'object' } } }],
});

async function frames(response) {
  assert.equal(response.status, 200);
  return (await response.text()).split('\n').filter((line) => line.startsWith('data: {'))
    .map((line) => JSON.parse(line.slice(6)));
}

test('interaction models require opt-in and the requested tool, with HTTP errors before SSE', async () => {
  assert.equal(fixtureProviderConfig(1234).provider.perf.models['interaction-shell'], undefined);
  assert.equal(fixtureProviderConfig(1234, { interactions: true }).provider.perf.models['interaction-shell'].tool_call, true);
  await withProvider(false, async (request) => {
    assert.equal((await request(prompt('shell'))).status, 400);
  });
  await withProvider(true, async (request) => {
    for (const name of ['shell', 'question']) {
      const response = await request({ ...prompt(name), tools: [] });
      assert.equal(response.status, 400);
      assert.match((await response.json()).error.message, new RegExp(`requires the ${name} tool`));
    }
    assert.equal((await request({ ...prompt('shell'), model: 'interaction-unknown' })).status, 400);
    assert.equal((await request({ ...prompt('shell'), stream: false })).status, 400);
  });
});

test('interaction models emit exact calls, finish after a tool result, and reset each user turn', async () => {
  await withProvider(true, async (request) => {
    for (const name of ['shell', 'question']) {
      const body = prompt(name);
      const first = await frames(await request(body));
      const calls = first.flatMap((frame) => frame.choices[0].delta.tool_calls ?? []);
      assert.equal(calls[0].function.name, name);
      assert.deepEqual(JSON.parse(calls[1].function.arguments), name === 'shell'
        ? { command: INTERACTION_SHELL_COMMAND, timeout: 5000, background: false }
        : { questions: INTERACTION_QUESTIONS });
      assert.equal(first.at(-1).choices[0].finish_reason, 'tool_calls');
      body.messages.push({ role: 'tool', tool_call_id: calls[0].id, content: 'Synthetic tool result.' });
      const completed = await frames(await request(body));
      assert.equal(completed.at(-1).choices[0].finish_reason, 'stop');
      assert.equal(completed.map((frame) => frame.choices[0].delta.content ?? '').join(''), 'Fixture interaction complete.');
      body.messages.push({ role: 'user', content: 'Another fixture turn.' });
      assert.equal((await frames(await request(body))).at(-1).choices[0].finish_reason, 'tool_calls');
    }
  });
});

test('existing agent and auxiliary responses keep their behavior', async () => {
  await withProvider(false, async (request) => {
    const auxiliary = await request({ model: 'stream-1200cps', stream: false, messages: [] });
    assert.equal((await auxiliary.json()).choices[0].message.content, 'Streaming capture');
    const result = await frames(await request({ model: 'agent-1tools-20000cps', stream: true, messages: [{ role: 'user', content: 'List' }] }));
    assert.equal(result.flatMap((frame) => frame.choices[0].delta.tool_calls ?? [])[0].function.name, 'glob');
  });
});
