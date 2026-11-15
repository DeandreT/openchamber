import assert from 'node:assert/strict';
import test from 'node:test';
import { auditAnswer, FINAL_REVIEW_CLEAN, FINAL_REVIEW_DISPUTED, fixtureProviderConfig, planWorkflow, startFixtureProvider, INTERACTION_QUESTIONS, INTERACTION_SHELL_COMMAND } from './fixture-provider.mjs';

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

test('a controlled think turn stays held until release and cleans up when aborted', async () => {
  const server = await startFixtureProvider(0);
  const origin = `http://127.0.0.1:${server.address().port}`;
  const control = async (held) => {
    const response = await fetch(`${origin}/__fixture/turn-gate`, held === undefined ? {} : {
      method: 'PUT', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ held }),
    });
    assert.equal(response.status, 200);
    return response.json();
  };
  const turn = (signal) => fetch(`${origin}/v1/chat/completions`, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ model: 'think-30s', stream: true, messages: [] }), signal,
  });
  try {
    assert.deepEqual(await control(), { held: false, waiting: 0 });
    await control(true);
    const response = await turn(AbortSignal.timeout(5000));
    assert.deepEqual(await control(), { held: true, waiting: 1 });
    await control(false);
    assert.equal((await frames(response)).map((frame) => frame.choices[0].delta.content ?? '').join(''), 'Done.');
    assert.deepEqual(await control(), { held: false, waiting: 0 });

    await control(true);
    const controller = new AbortController();
    const aborted = await turn(controller.signal);
    assert.equal((await control()).waiting, 1);
    controller.abort();
    await assert.rejects(aborted.text());
    for (let attempt = 0; attempt < 100 && (await control()).waiting; attempt++) {
      await new Promise((done) => setTimeout(done, 10));
    }
    assert.equal((await control()).waiting, 0);
    const invalid = await fetch(`${origin}/__fixture/turn-gate`, { method: 'PUT', body: 'null' });
    assert.equal(invalid.status, 400);
  } finally {
    try { await control(false); } finally {
      server.closeAllConnections();
      await new Promise((done) => server.close(done));
    }
  }
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

test('file-link and conflict replies apply only to their explicit fixture turns', async () => {
  await withProvider(false, async (request) => {
    const body = { model: 'stream-20000cps', stream: true,
      messages: [{ role: 'user', content: 'files-link-proof: file:///fixture/outside.txt' }] };
    const text = async (value) => (await frames(await request(value))).map((frame) => frame.choices[0].delta.content ?? '').join('');
    assert.equal(await text(body), '[Outside file](file:///fixture/outside.txt)');
    body.messages.push({ role: 'user', content: 'Investigate the merge conflicts and concisely report the intended resolution strategy.' });
    assert.equal(await text(body), 'Conflict context received. Waiting for confirmation.');
    body.messages.push({ role: 'user', content: 'An ordinary fixture turn' });
    assert.doesNotMatch(await text(body), /Outside file|Conflict context received/);
  });
});

test('walkthrough fixtures preserve aliases supplied by the server digest', async () => {
  await withProvider(false, async (request) => {
    const digest = { files: [
      { path: 'a.rs', hunks: [{ alias: 'h1' }] },
      { path: 'b.rs', hunks: [{ alias: 'h2' }] },
    ] };
    const response = await request({ model: 'stream-20000cps', stream: false, messages: [
      { role: 'system', content: 'You are writing a walkthrough of a code change' },
      { role: 'user', content: 'Change digest:\n' + JSON.stringify(digest) },
    ] });
    assert.equal(response.status, 200);
    const result = JSON.parse((await response.json()).choices[0].message.content);
    assert.deepEqual(result.chapters.flatMap((chapter) => chapter.stops.flatMap((stop) => stop.hunks)), ['h1', 'h2']);
  });
});

const user = (content) => ({ role: 'user', content });
const intro = user('<system-reminder>\nGoal mode is active for this session. The user message above defines the goal objective.\n</system-reminder>');
const continuation = user('The session goal is still open. Take it through to the end in this run.\n<objective>\nShip [turns:3]\n</objective>');
const audit = (report) => [user(`You check the progress of an AI coding agent.\n\n<objective>\nShip\n</objective>\n\n<answer>\n${report}\n</answer>\n\nReturn the JSON.`)];

test('the workflow model reports a goal turn by turn, as its objective asks', () => {
  const first = planWorkflow([intro, user('Ship [turns:3]')]);
  assert.match(first, /- Remaining: steps 2 to 3/);
  assert.match(planWorkflow([intro, user('Ship [turns:3]'), continuation]), /- Remaining: steps 3 to 3/);
  assert.match(planWorkflow([intro, user('Ship [turns:3]'), continuation, continuation]), /- Remaining: nothing\n- Needs from the user: nothing/);
  assert.match(planWorkflow([intro, user('Ship [blocked]')]), /- Needs from the user: an API key/);
  assert.equal(planWorkflow([user('Hello')]), 'Done.');
  // A later goal in the same session counts from its own start.
  const earlier = [intro, user('Ship [turns:2]'), continuation, user('[blocked]')];
  assert.match(planWorkflow([...earlier, intro, user('Next [turns:4]')]), /- Remaining: steps 2 to 4/);
});

test('the progress check answers from the report lines', () => {
  const answer = (report) => JSON.parse(auditAnswer(audit(report)));
  assert.deepEqual(answer(planWorkflow([intro, user('Ship [turns:2]')])), { all_done: false, remaining: true, needs_user: false });
  assert.deepEqual(answer(planWorkflow([intro, user('Ship [turns:1]')])), { all_done: true, remaining: false, needs_user: false });
  assert.deepEqual(answer(planWorkflow([intro, user('Ship [blocked]')])), { all_done: true, remaining: false, needs_user: true });
  assert.equal(auditAnswer([user('Generate a title')]), null);
});

test('the workflow model walks a review loop to its final line', () => {
  const handoff = planWorkflow([user('Prepare a handoff for another agent to review this work.')]);
  assert.match(handoff, /^Handoff:/);
  const review = [user(`Please review the changes described in this handoff.\n\n${handoff}`)];
  assert.doesNotMatch(planWorkflow(review), /FINAL_REVIEW_STATUS/);
  assert.match(planWorkflow([user('Another agent reviewed your changes and left the feedback below.')]), /^Fixed/);
  const reReview = [...review, user('The agent implementing the changes has responded to the previous review feedback.')];
  assert.ok(planWorkflow(reReview).endsWith(FINAL_REVIEW_CLEAN));
  assert.ok(planWorkflow([user('[dispute]'), ...reReview]).endsWith(FINAL_REVIEW_DISPUTED));
  assert.ok(planWorkflow([user('Please review the current workspace changes. [clean]')]).endsWith(FINAL_REVIEW_CLEAN));
});

test('workflow models and answers require opt-in', async () => {
  assert.equal(fixtureProviderConfig(1234).provider.perf.models.workflow, undefined);
  assert.ok(fixtureProviderConfig(1234, { workflows: true }).provider.perf.models['workflow-1s']);
  const post = async (workflows, body) => {
    const server = await startFixtureProvider(0, { workflows });
    try {
      const response = await fetch(`http://127.0.0.1:${server.address().port}/v1/chat/completions`, {
        method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body), signal: AbortSignal.timeout(5000),
      });
      return (await response.json()).choices[0].message.content;
    } finally { await new Promise((resolve) => server.close(resolve)); }
  };
  const check = { model: 'stream-1200cps', stream: false, messages: audit('- Done: all\n- Remaining: nothing\n- Needs from the user: nothing') };
  assert.equal(await post(false, check), 'Streaming capture');
  assert.deepEqual(JSON.parse(await post(true, check)), { all_done: true, remaining: false, needs_user: false });
});
