import { expect, test } from 'bun:test';
import { prompt } from '@cognition-ai/sdk';
import { captureContext, captureRenderedContext, readCapturedContext, readContextCheckpoint, transferContext } from './context.ts';

test('context encoding preserves primitive values and special numbers', () => {
  const captured = captureContext([null, true, false, 'text', 0, 12.5, Infinity, -Infinity, undefined], [{ role: 'user', id: 'u' }], 'opencode-context-hook');
  expect(JSON.parse(captured.system)).toEqual([
    null, true, false, 'text', 0, 12.5,
    { encoding: 'number', value: 'Infinity' }, { encoding: 'number', value: '-Infinity' }, { encoding: 'undefined' },
  ]);
});

test('context encoding preserves special values and prevents user objects from impersonating encoding tags', () => {
  const system = [{ missing: undefined, bytes: new Uint8Array([1, 2, 3]), url: new URL('https://example.com/reference'),
    date: new Date('2026-11-13T00:00:00Z'), number: Number.NaN, negativeZero: -0, integer: 3n,
    fake: { encoding: 'bytes-base64', value: 'not-binary' } }];
  const captured = captureContext(system, [{ role: 'user', id: 'u' }], 'opencode-context-hook');
  const encoded = captured.system;
  expect(encoded).toContain('"encoding":"undefined"');
  expect(encoded).toContain('"encoding":"bytes-base64","value":"AQID"');
  expect(encoded).toContain('"encoding":"number","value":"NaN"');
  expect(encoded).toContain('"encoding":"number","value":"-0"');
  expect(encoded).toContain('"encoding":"bigint","value":"3"');
  expect(encoded).toContain('"encoding":"date"');
  expect(encoded).toContain('"encoding":"url"');
  expect(encoded).toContain('["fake",{"encoding":"object","entries":');
  const transfer = transferContext(captured, undefined, [prompt.text('already represented user text'), prompt.image('AQID', 'image/png')]);
  expect(transfer.prompt).toHaveLength(2);
  expect(transfer.prompt[1]).toMatchObject({ type: 'image', data: 'AQID' });
  expect(JSON.stringify(transfer.prompt)).not.toContain('already represented user text');
});

test('context capture is deterministic across object key order and immutable after capture', () => {
  const first = captureContext([{ a: 1, b: 2 }], [{ role: 'user', id: 'u' }], 'opencode-context-hook');
  const other = captureContext([{ b: 2, a: 1 }], [{ id: 'u', role: 'user' }], 'opencode-context-hook');
  expect(first).toEqual(other);
  const transferred = transferContext(first, undefined, 'represented in envelope');
  const decoded = readCapturedContext(first);
  decoded.messages[0] = '{}';
  expect(first.messages[0]).not.toBe('{}');
  expect(readContextCheckpoint(transferred.checkpoint)).toEqual(transferred.checkpoint);
});

test('context rejects unsupported, cyclic, hidden and symbol fields before they can be silently omitted', () => {
  type Cycle = { self?: Cycle };
  const cyclic: Cycle = {}; cyclic.self = cyclic;
  const hidden = Object.defineProperty({}, 'secret', { value: 'hidden-data', enumerable: false });
  const symbol = { [Symbol('data')]: 'symbol-data' };
  const customArray: unknown[] & { extra?: string } = []; customArray.extra = 'would be lost';
  const hiddenLength = Object.defineProperty({}, 'length', { value: 'hidden-data', enumerable: false });
  const credentialedMedia = { source: { type: 'url', url: 'https://example.com/private' }, headers: { authorization: 'fixture-only-noncredential' } };
  for (const value of [cyclic, hidden, hiddenLength, symbol, customArray, credentialedMedia, new Map(), new Set(), new ArrayBuffer(1), Promise.resolve(), () => {}, Symbol('value')]) {
    expect(() => captureContext([value], [{ role: 'user', id: 'u' }], 'opencode-context-hook')).toThrow('unsupported');
  }
  expect(() => captureContext([], [{ role: 'assistant' }], 'opencode-context-hook')).toThrow('invalid_request');
});

test('malformed captured context and persisted checkpoints fail closed', () => {
  const good = captureContext([], [{ role: 'user', id: 'u' }], 'opencode-context-hook');
  for (const value of [null, {}, { ...good, version: 2 }, { ...good, system: '{' }, { ...good, messages: ['{'] }, { ...good, userIndex: 10 }, { ...good, user: 'bad' }]) {
    expect(() => readCapturedContext(value)).toThrow('invalid_request');
  }
  for (const value of [null, {}, { version: 1, user: good.user, messages: [], system: 'bad' }, { version: 1, user: good.user, system: good.user, messages: ['bad'] }]) {
    expect(() => readContextCheckpoint(value)).toThrow('unavailable');
  }
});

test('final rendered context preserves later-hook edits and has its own incremental checkpoint', () => {
  const raw = captureContext([{ text: 'original provenance' }], [{ role: 'user', id: 'u' }], 'opencode-context-hook');
  const rendered = [{ role: 'system', content: 'later-hook instruction' }, { role: 'user', content: 'current input' }];
  const captured = captureRenderedContext(raw, rendered);
  const first = transferContext(captured, undefined, 'represented');
  rendered[0].content = 'mutated after capture';
  const body = prompt.toPromptBlocks(first.prompt)[0];
  expect(body.type === 'text' && body.text.includes('later-hook instruction')).toBe(true);
  expect(body.type === 'text' && body.text.includes('mutated after capture')).toBe(false);
  expect(first.checkpoint.rendered).toHaveLength(2);
  const nextRaw = captureContext([{ text: 'original provenance' }], [{ role: 'user', id: 'u' }, { role: 'user', id: 'v' }], 'opencode-context-hook');
  const next = transferContext(captureRenderedContext(nextRaw, [{ role: 'system', content: 'later-hook instruction' }, { role: 'user', content: 'current input' }, { role: 'user', content: 'next input' }]), first.checkpoint, 'represented');
  const nextText = prompt.toPromptBlocks(next.prompt)[0];
  if (nextText.type !== 'text') throw Error('missing envelope');
  const envelope = JSON.parse(nextText.text.slice(nextText.text.indexOf('\n') + 1));
  expect(envelope.renderedRequest).toMatchObject({ operation: 'append', startIndex: 2 });
  expect(envelope.renderedRequest.messages).toHaveLength(1);
});
