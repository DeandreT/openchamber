import { afterAll, afterEach, beforeEach, expect, spyOn, test } from 'bun:test';
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
const fetchSpy = spyOn(globalThis, 'fetch').mockImplementation(async (input, init) => {
  const request = new Request(input instanceof Request ? input : new URL(input, window.location.href), init);
  const path = new URL(request.url).pathname;
  if (path === '/api/fs/home') return Response.json({ home: '/test' });
  if (path === '/api/config/settings') return Response.json({});
  if (path !== '/api/routing') throw new Error(`Unexpected request: ${request.method} ${path}`);
  const state = { available: true, autoReady: false, tokenPresent: false, jevSource: 'zen-free', builtins: [], config };
  if (request.method === 'GET') return Response.json(state);
  expect(request.method).toBe('PUT');
  const body = await request.json();
  writes.push(body);
  return Response.json({ ...state, config: body.config });
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

beforeEach(async () => {
  writes = [];
  useConfigStore.setState({ providersLoaded: true });
  useRoutingStore.getState().resetForRuntime();
  root = createRoot(container);
  await act(async () => { root.render(<I18nProvider><TooltipProvider><RoutingPage /></TooltipProvider></I18nProvider>); });
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

async function clearPicker(trigger: RegExp, choice: RegExp) {
  await clickButton(trigger);
  const sheet = document.querySelector('[role="dialog"]');
  if (!sheet) throw new Error('Missing model picker');
  await clickButton(choice, sheet);
  // Leaving Routing flushes the pending edit without waiting for the debounce.
  await act(async () => root.render(null));
}

test('clearing the fallback saves a null fallback', async () => {
  await clearPicker(/^original$/, /^Choose a model$/);
  expect(writes).toEqual([{ config: { ...config, fallback: null } }]);
});

test('choosing Fallback model clears both category overrides', async () => {
  await clickButton(/^Trivial/);
  await clearPicker(/^override$/, /^Fallback model$/);
  expect(writes).toEqual([{ config: { ...config, categories: [{ ...category, model: null, variant: null }] } }]);
});
