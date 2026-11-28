import { afterAll, afterEach, beforeEach, describe, expect, jest, spyOn, test } from 'bun:test';
import { Window } from 'happy-dom';
import { act } from 'react';
import type { RoutingConfig } from '@/lib/routing/routingApi';

const browser = new Window({ url: 'http://localhost/mobile.html?surface=mobile', width: 390, height: 844 });
Object.assign(globalThis, {
  window: browser, document: browser.document, navigator: browser.navigator,
  localStorage: browser.localStorage, sessionStorage: browser.sessionStorage,
  Node: browser.Node, Element: browser.Element, HTMLElement: browser.HTMLElement,
  Event: browser.Event, CustomEvent: browser.CustomEvent, MutationObserver: browser.MutationObserver,
  ResizeObserver: browser.ResizeObserver, getComputedStyle: browser.getComputedStyle.bind(browser),
  requestAnimationFrame: browser.requestAnimationFrame.bind(browser),
  cancelAnimationFrame: browser.cancelAnimationFrame.bind(browser), IS_REACT_ACT_ENVIRONMENT: true,
});
// Give the virtualized picker a viewport in happy-dom.
Object.defineProperty(browser.HTMLElement.prototype, 'clientHeight', { configurable: true, value: 400 });

const category = {
  id: 'trivial', name: 'Trivial', description: 'Small tasks', builtin: true, enabled: true,
  model: { providerID: 'fixture', modelID: 'override' }, variant: 'medium', agent: null,
};
const config: RoutingConfig = {
  enabled: true, fallback: { model: { providerID: 'fixture', modelID: 'original' }, variant: 'high' },
  minConfidence: 0.8, safetyNet: { enabled: false, threshold: 0.9 }, categories: [category],
};
let writes: unknown[];
let hostConfig: RoutingConfig;
let nextAcknowledgement: Promise<void> | undefined;
const fetchSpy = spyOn(globalThis, 'fetch').mockImplementation(async (input, init) => {
  const request = new Request(input instanceof Request ? input : new URL(input, window.location.href), init);
  const path = new URL(request.url).pathname;
  if (path === '/api/fs/home') return Response.json({ home: '/test' });
  if (path === '/api/config/settings') return Response.json({});
  if (path === '/api/agent') return Response.json({ data: [] });
  if (path !== '/api/routing') throw new Error(`Unexpected request: ${request.method} ${path}`);
  const state = { available: true, autoReady: false, tokenPresent: false, jevSource: 'zen-free', builtins: [] };
  if (request.method === 'GET') return Response.json({ ...state, config: hostConfig });
  expect(request.method).toBe('PUT');
  const body = await request.json();
  writes.push(body);
  hostConfig = body.config;
  const reply = Response.json({ ...state, config: hostConfig });
  const acknowledgement = nextAcknowledgement;
  nextAcknowledgement = undefined;
  await acknowledgement;
  return reply;
});
const { createRoot } = await import('react-dom/client');
const { I18nProvider } = await import('@/lib/i18n');
const { TooltipProvider } = await import('@/components/ui/tooltip');
const { useConfigStore } = await import('@/stores/useConfigStore');
const { useRoutingStore } = await import('@/stores/useRoutingStore');
const { RoutingPage } = await import('./RoutingPage');
const initialConfig = useConfigStore.getState();
const container = document.body.appendChild(document.createElement('div'));
let root: ReturnType<typeof createRoot>;

const openRouting = () => act(async () => {
  root.render(<I18nProvider><TooltipProvider><RoutingPage /></TooltipProvider></I18nProvider>);
});

beforeEach(async () => {
  writes = [];
  hostConfig = config;
  nextAcknowledgement = undefined;
  useConfigStore.setState({ providersLoaded: true });
  useRoutingStore.getState().resetForRuntime();
  root = createRoot(container);
  await openRouting();
});

afterEach(async () => {
  await act(async () => root.unmount());
  useConfigStore.setState(initialConfig, true);
});
afterAll(() => { fetchSpy.mockRestore(); browser.close(); });

async function clickButton(label: RegExp, parent: ParentNode = container) {
  const button = [...parent.querySelectorAll('button')].find((item) => label.test(item.textContent ?? ''));
  if (!button) throw new Error(`Missing button ${label}`);
  await act(async () => button.click());
}

async function openPicker(trigger: RegExp) {
  await clickButton(trigger);
  const sheet = document.querySelector('[role="dialog"]');
  if (!sheet) throw new Error('Missing model picker');
  return sheet;
}

test('clearing the fallback saves a null fallback', async () => {
  await clickButton(/^Choose a model$/, await openPicker(/^original$/));
  // Leaving Routing flushes the pending edit without waiting for the debounce.
  await act(async () => root.render(null));
  expect(writes).toEqual([{ config: { ...config, fallback: null } }]);
});

test('choosing Fallback model clears both category overrides', async () => {
  await clickButton(/^Trivial/);
  await clickButton(/^Fallback model$/, await openPicker(/^override$/));
  await act(async () => root.render(null));
  expect(writes).toEqual([{ config: { ...config, categories: [{ ...category, model: null, variant: null }] } }]);
});

describe('with a Session Default', () => {
  const sessionFallback = { model: { providerID: 'fixture', modelID: 'session' }, variant: 'medium' };
  const advanceSave = () => act(async () => { jest.advanceTimersByTime(1000); });
  const reread = () => act(async () => { await useRoutingStore.getState().load(); });

  beforeEach(async () => {
    jest.useFakeTimers();
    await act(async () => useConfigStore.setState({
      settingsDefaultModel: 'fixture/session', settingsDefaultVariant: 'medium',
      providers: [{
        id: 'fixture', name: 'Fixture', activation: 'enabled', package: 'fixture',
        models: ['session', 'replacement'].map((id) => ({
          id, providerID: 'fixture', modelID: id, name: id,
          enabled: true, status: 'active', variants: [{ id: 'medium' }],
          capabilities: { tools: true, input: ['text'], output: ['text'] },
          cost: [], limit: { context: 0, output: 0 }, time: { released: 0 },
        })),
      }],
    }));
  });
  afterEach(() => { jest.useRealTimers(); });

  for (const rereadFirst of [false, true]) {
    test(`clearing stays empty for this visit when the ${rereadFirst ? 'reread' : 'acknowledgement'} arrives first`, async () => {
      let acknowledge!: () => void;
      nextAcknowledgement = new Promise<void>((resolve) => { acknowledge = resolve; });
      const cleared = { ...config, fallback: null };
      try {
        await clickButton(/^Choose a model$/, await openPicker(/^original$/));
        await advanceSave();
        expect(writes).toEqual([{ config: cleared }]);
        if (rereadFirst) await reread();
      } finally {
        await act(async () => { acknowledge(); });
      }
      await advanceSave();
      await reread();
      await advanceSave();
      expect(writes).toEqual([{ config: cleared }]);
      const row = container.querySelector('[data-settings-item="routing.fallback-model"]');
      expect(row?.textContent).toContain('Choose a model');

      await act(async () => root.render(null));
      await openRouting();
      await advanceSave();
      expect(writes).toEqual([{ config: cleared }, { config: { ...config, fallback: sessionFallback } }]);
    });
  }

  test('picking a model still allows prefill when another client clears the fallback', async () => {
    const sheet = await openPicker(/^original$/);
    const replacement = [...sheet.querySelectorAll<HTMLElement>('[role="option"]')]
      .find((option) => option.textContent?.includes('replacement'));
    if (!replacement) throw new Error('Missing replacement model');
    await act(async () => replacement.click());
    await advanceSave();
    const picked = { ...config, fallback: { model: { providerID: 'fixture', modelID: 'replacement' }, variant: null } };
    expect(writes).toEqual([{ config: picked }]);

    hostConfig = { ...picked, fallback: null };
    await reread();
    await advanceSave();
    expect(writes).toEqual([{ config: picked }, { config: { ...config, fallback: sessionFallback } }]);
  });
});
