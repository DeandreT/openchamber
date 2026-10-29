/** Official 2.2 blocking-request calls against synthetic, local recording responses. */
import { OpenCode } from '@opencode/client';
import type { FormAnswer, FormField } from '@opencode/client';
import { z } from 'zod';
import { classifyStreamPayload } from '@/sync/event-pipeline';
import { buildFormAnswer, initialFormValues, missingRequiredKeys, visibleFields } from '@/components/chat/formCardState';

const sampleSchema = z.object({
  directory: z.string(),
  permission: z.object({ id: z.string(), sessionID: z.string() }).passthrough(),
  form: z.object({ id: z.string(), sessionID: z.string() }).passthrough(),
  answer: z.record(z.string(), z.union([z.string(), z.number(), z.boolean(), z.array(z.string())])),
});

export async function blockingRequestFixtures(sampleText: string) {
  const sample = sampleSchema.parse(JSON.parse(sampleText));
  const requests: Array<{ name: string; method: string; path: string; query: string[][]; directoryHeader: string | null; contentType: string | null; body: unknown }> = [];
  let name = '';
  const fetch = async (input: string | URL | Request, init?: RequestInit) => {
    const request = new Request(input, init);
    const url = new URL(request.url);
    requests.push({ name, method: request.method, path: url.pathname, query: [...url.searchParams.entries()],
      directoryHeader: request.headers.get('x-opencode-directory'), contentType: request.headers.get('content-type'),
      body: request.body ? JSON.parse(await request.text()) : null });
    if (request.method !== 'GET') return new Response(null, { status: 204 });
    const form = { ...sample.form, sessionID: url.pathname.includes('/global/') ? 'global' : sample.form.sessionID };
    if (url.pathname === '/api/permission/request') return Response.json({ location: { directory: sample.directory }, data: [sample.permission] });
    if (url.pathname === '/api/form') return Response.json({ location: { directory: sample.directory }, data: [sample.form, { ...sample.form, id: 'frm_global_fixture', sessionID: 'global' }] });
    if (url.pathname.endsWith('/permission')) return Response.json({ data: [sample.permission] });
    if (url.pathname.endsWith('/form')) return Response.json({ data: [form] });
    if (url.pathname.includes('/permission/')) return Response.json({ data: sample.permission });
    return Response.json({ data: { ...form, state: { status: 'pending' } } });
  };
  const global = OpenCode.make({ baseUrl: 'https://synthetic.invalid', fetch });
  const scoped = OpenCode.make({ baseUrl: 'https://synthetic.invalid', fetch, headers: { 'x-opencode-directory': encodeURIComponent(sample.directory) } });
  const named = async <T,>(label: string, call: () => Promise<T>) => { name = label; return call(); };
  const sessionID = sample.permission.sessionID;
  const requestID = sample.permission.id;
  const formID = sample.form.id;
  const permissions = await named('location permissions', () => scoped.permission.request.list());
  const forms = await named('location forms', () => scoped.form.list());
  await named('session permissions', () => global.permission.list({ sessionID }));
  await named('session forms', () => global.session.form.list({ sessionID }));
  const permission = await named('permission get', () => global.permission.get({ sessionID, requestID }));
  const form = await named('form get', () => global.session.form.get({ sessionID, formID }));
  await named('global form get', () => scoped.session.form.get({ sessionID: 'global', formID }));
  for (const decision of ['once', 'always', 'reject'] as const) {
    await named(`permission ${decision}`, () => global.permission.reply({ sessionID, requestID, decision }));
  }
  await named('permission feedback', () => global.permission.reply({ sessionID, requestID, decision: 'reject', message: 'Use another command' }));
  const answer: FormAnswer = sample.answer;
  await named('form answer', () => global.session.form.reply({ sessionID, formID, answer }));
  await named('global form answer', () => scoped.session.form.reply({ sessionID: 'global', formID, answer }));
  await named('form cancel', () => global.session.form.cancel({ sessionID, formID }));
  await named('global form cancel', () => scoped.session.form.cancel({ sessionID: 'global', formID }));
  await named('form cancel feedback', () => global.session.form.cancel({ sessionID, formID, message: 'No answer available' }));
  const payloads = [
    { type: 'permission.asked', data: sample.permission },
    { type: 'permission.replied', data: { sessionID, requestID, reply: 'once' } },
    { type: 'form.created', data: { form: sample.form } },
    { type: 'form.created', data: { form: { ...sample.form, sessionID: 'global' } } },
    { type: 'form.replied', data: { sessionID, id: formID, answer } },
    { type: 'form.cancelled', data: { sessionID: 'global', id: formID } },
    { type: 'location.shutdown', data: {} },
  ].map((event, index) => ({ id: `evt_fixture_${index}`, created: 1, location: { directory: sample.directory }, ...event }));
  const events = payloads.map((payload) => {
    const classified = classifyStreamPayload(payload, sample.directory);
    if (classified.kind !== 'events') throw new Error(`No projection for ${payload.type}`);
    return { payload, projected: classified.events.map(({ event }) => event) };
  });
  const initial = initialFormValues(form.fields);
  const formDefaults = {
    answer: buildFormAnswer(form.fields, initial),
    visible: visibleFields(form.fields, initial).map((field) => field.key),
    missing: missingRequiredKeys(form.fields, initial),
  };
  const defaultCases: Array<{ name: string; field: FormField }> = [
    ...([
      ['empty', ''], ['ascii whitespace', ' \t\r\n'], ['BOM and NBSP', '\uFEFF\u00A0'],
      ['Unicode trimmed', '\u3000value\u2029'], ['BOM trimmed', '\uFEFFvalue\uFEFF'],
      ['NEL retained', '\u0085'], ['Mongolian separator retained', '\u180E'], ['zero-width space retained', '\u200B'],
    ] as const).map(([name, value]) => ({ name, field: { key: 'first', type: 'string' as const, default: value, custom: true } })),
    { name: 'exact whitespace option retained', field: { key: 'first', type: 'string', default: ' \uFEFF', options: [{ value: ' \uFEFF', label: 'Exact option' }], custom: true } },
    { name: 'exact empty option retained', field: { key: 'first', type: 'string', default: '', options: [{ value: '', label: 'Empty option' }], custom: true } },
    { name: 'empty multiselect omitted', field: { key: 'first', type: 'multiselect', default: [], options: [], custom: true } },
    { name: 'multiselect values retained', field: { key: 'first', type: 'multiselect', default: ['', ' \uFEFF'], options: [], custom: true } },
  ];
  const defaultParity = defaultCases.map(({ name, field }, index) => {
    const fields: FormField[] = [field, { key: 'dependent', type: 'string', default: 'shown', when: [{ key: 'first', op: 'neq', value: 'never' }] }];
    const values = initialFormValues(fields);
    return { name, form: { id: `frm_defaults_${index}`, sessionID, title: 'Default parity', fields },
      answer: buildFormAnswer(fields, values), visible: visibleFields(fields, values).map((field) => field.key) };
  });
  return { requests, permissions, forms, permission, form, formDefaults, defaultParity, events };
}
