import { CloudDevin, CognitionMetaKey, type CloudDevinOptions, type PermissionRequest, type PromptCapabilities } from '@cognition-ai/sdk';
import { localProcess } from './local-process.ts';
import { createHash } from 'node:crypto';
import { Plugin, Model, Provider } from '@opencode/plugin';
import { isEnterpriseMode } from '../../packages/web/server/lib/enterprise-mode.js';
import { Bindings } from './bindings.ts';
import { languageModel } from './model.ts';
import { DevinNativeAgent } from './rpc.ts';
import { DevinRuntime, NativeFailure, type Mode } from './runtime.ts';
import { captureContext } from './context.ts';
import path from 'node:path';

const PROVIDER = 'devin';
const PACKAGE = 'openchamber-devin';

export default Plugin.define({
  id: 'openchamber.devin',
  async setup(ctx) {
    let disposed = false;
    const directory = ctx.location.directory;
    const cliPath = typeof ctx.options.cliPath === 'string' ? ctx.options.cliPath : 'devin';
    const defaultMode: Mode = ctx.options.mode === 'cloud' ? 'cloud' : 'local';
    const providerID = Provider.ID.make(PROVIDER);
    const promptCapabilities = new Map<Mode, PromptCapabilities>();
    const capabilityPublications = new Map<Mode, Promise<void>>();
    const registryKey = `execution-bindings-v1/${createHash('sha256').update(directory).digest('hex')}`;
    const bindings = new Bindings({
      read: () => ctx.storage.get(registryKey),
      write: async (value) => { await ctx.storage.set(registryKey, value); },
      reload: async () => {
        await ctx.model.reload();
        const models = await ctx.model.list();
        for (const mode of ['local', 'cloud'] as const) {
          const model = models.data.find((item) => item.providerID === PROVIDER && item.id === mode);
          if (!model || bindings.variants(mode).some((variant) => !model.variants.some((item) => item.id === variant.id))) throw new NativeFailure('unavailable');
        }
      },
    });
    await bindings.load();
    await ctx.provider.transform((editor) => {
      editor.add({
        info: { ...Provider.Info.empty(providerID), name: 'Devin', activation: 'enabled',
          package: `aisdk:${PACKAGE}`, settings: { openchamber: { nativeAgent: { protocolVersion: 1 } } } },
        models: (['local', 'cloud'] as const).map((mode) => ({
          ...Model.Info.default(providerID, Model.ID.make(mode)), name: mode === 'local' ? 'Devin Local' : 'Devin Cloud',
          // These are routing identities. Account model choices come from ACP configOptions.
          variants: [], capabilities: { tools: true, input: ['text'], output: ['text'] },
        })),
      });
    });
    await ctx.model.transform((editor) => {
      for (const mode of ['local', 'cloud'] as const) editor.update(PROVIDER, mode, (model) => {
        model.variants = bindings.variants(mode).map((variant) => ({ ...variant, id: Model.VariantID.make(variant.id) }));
        const capabilities = promptCapabilities.get(mode);
        model.capabilities.input = ['text', ...(capabilities?.image ? ['image' as const] : []), ...(capabilities?.audio ? ['audio' as const] : []), ...(capabilities?.embeddedContext ? ['pdf' as const] : [])];
      });
    });
    let registration: Awaited<ReturnType<typeof ctx.rpc.register<typeof DevinNativeAgent>>> | undefined;
    const runtime = new DevinRuntime(directory, {
      hasCredential: (mode) => mode === 'local' || Boolean(process.env.DEVIN_API_KEY),
      authAllowed: () => !isEnterpriseMode(),
      checkScope: async (scope) => {
        if (!scope.sessionID) return;
        const session = await ctx.session.get({ sessionID: scope.sessionID });
        if (session.location.directory !== directory) throw new NativeFailure('invalid_request');
        if (session.model?.providerID === PROVIDER && (session.model.id === 'local' || session.model.id === 'cloud')) return session.model.id;
      },
      authorize: async (scope, permission, mode) => {
        if (!scope.sessionID) throw new NativeFailure('not_prepared');
        try {
          await ctx.permission.assert({ sessionID: scope.sessionID, action: 'provider.use', resources: [PROVIDER] });
          // These checks cover only approvals Devin exposes through ACP. Devin owns execution.
          if (permission) {
            for (const { action, resources } of permissionRules(permission, directory, mode)) {
              await ctx.permission.assert({ sessionID: scope.sessionID, action, resources });
            }
          }
        } catch { throw new NativeFailure('policy_denied'); }
      },
      authenticate: async (scope, mode) => {
        if (isEnterpriseMode()) throw new NativeFailure('policy_denied');
        if (mode === 'cloud') {
          if (!process.env.DEVIN_API_KEY) throw new NativeFailure('authentication_required');
          return 'ready';
        }
        // OpenCode owns the command attempt. The CLI owns the browser login and cached grant.
        await ctx.integration.command.connect({ integrationID: PROVIDER, methodID: 'cli' });
        void scope;
        return 'pending';
      },
      open: async (scope, mode, existingID) => {
        if (mode === 'cloud') {
          const key = process.env.DEVIN_API_KEY;
          if (!key) throw new NativeFailure('authentication_required');
          const response = await fetch('https://api.devin.ai/v3/self', { headers: { authorization: `Bearer ${key}` }, redirect: 'error', signal: AbortSignal.timeout(15_000) });
          await response.body?.cancel();
          if (response.status === 401 || response.status === 403) throw new NativeFailure('authentication_required');
          if (!response.ok) throw new NativeFailure('unavailable');
        }
        let client: CloudDevin | Awaited<ReturnType<typeof localProcess.spawn>>;
        if (mode === 'cloud') {
          const options: CloudDevinOptions = { apiKey: process.env.DEVIN_API_KEY, baseUrl: 'https://api.devin.ai' };
          if (typeof ctx.options.orgID === 'string') options.orgId = ctx.options.orgID;
          client = await CloudDevin.connect(options);
        } else {
          client = await localProcess.spawn(cliPath, { cwd: scope.directory });
        }
        try {
          if (disposed) throw new NativeFailure('unavailable');
          const session = existingID
            ? (await client.loadSession(existingID, mode === 'local' ? { cwd: scope.directory } : {})).session
            : await client.createSession(mode === 'local' ? { cwd: scope.directory } : {});
          if (disposed) throw new NativeFailure('unavailable');
          const negotiated = client.promptCapabilities;
          const publication = (capabilityPublications.get(mode) ?? Promise.resolve()).then(async () => {
            if (disposed) throw new NativeFailure('unavailable');
            const previous = promptCapabilities.get(mode);
            promptCapabilities.set(mode, negotiated);
            if (Boolean(previous?.image) !== negotiated.image || Boolean(previous?.audio) !== negotiated.audio || Boolean(previous?.embeddedContext) !== negotiated.embeddedContext) {
              try {
                await ctx.model.reload();
                if (disposed) throw new NativeFailure('unavailable');
              } catch (error) {
                if (previous) promptCapabilities.set(mode, previous);
                else promptCapabilities.delete(mode);
                throw error;
              }
            }
          });
          capabilityPublications.set(mode, publication);
          try { await publication; }
          finally { if (capabilityPublications.get(mode) === publication) capabilityPublications.delete(mode); }
          return { session, promptCapabilities: client.promptCapabilities, close: () => client.close() };
        } catch {
          client.close();
          throw new NativeFailure('unavailable');
        }
      },
      read: (key) => ctx.storage.get(key),
      write: async (key, value) => { await ctx.storage.set(key, JSON.parse(JSON.stringify(value))); },
      emit: async (event) => {
        if (event.type === 'stateChanged') await registration?.events.emit('stateChanged', event);
        else await registration?.events.emit('toolProgress', event);
      },
      bind: (config, mode, signal) => bindings.bind(config, mode, signal),
      resolveBinding: (token) => bindings.resolve(token),
    }, defaultMode);
    registration = await ctx.rpc.register(DevinNativeAgent, {
      getState: (input) => runtime.call('getState', input),
      prepare: (input) => runtime.call('prepare', input),
      authenticate: (input) => runtime.call('authenticate', input),
      setConfig: (input) => runtime.call('setConfig', input),
      answer: (input) => runtime.call('answer', input),
    });
    await ctx.integration.transform((editor) => {
      editor.method.update({ integrationID: PROVIDER, method: { id: 'cli', type: 'command', label: 'Sign in with Devin CLI', command: [cliPath, 'auth', 'login'] } });
    });
    await ctx.aisdk.hook('sdk', (event) => {
      if (event.package !== PACKAGE) return;
      event.sdk = { languageModel: (id: string) => languageModel(id, directory, runtime) };
    }, { providerID: PROVIDER });
    await ctx.session.hook('model.request', (request) => {
      request.headers['x-openchamber-devin-session'] = request.sessionID;
      request.headers['x-openchamber-devin-kind'] = request.kind;
    }, { providerID: PROVIDER });
    await ctx.session.hook('context', (request) => {
      request.options.nativeContext = captureContext(request.system, request.messages, 'opencode-context-hook');
    }, { providerID: PROVIDER });
    // Auxiliary model work must never create a second billable Devin turn.
    await ctx.session.hook('title', (request) => { request.result = 'Devin session'; }, { providerID: PROVIDER });
    await ctx.session.hook('retry', (request) => { request.decision = { retry: false }; }, { providerID: PROVIDER });
    const events = new AbortController();
    void (async () => {
      try {
        for await (const event of ctx.event.subscribe({ types: ['session.tool.called', 'session.tool.success', 'session.tool.failed'], signal: events.signal })) {
          if (event.location?.directory !== directory) continue;
          await runtime.observeTool(event.data.sessionID, event.data.assistantMessageID, event.data.id,
            event.type === 'session.tool.called', event.type === 'session.tool.called' && event.data.executed);
        }
      } catch { /* Unload or disconnected event streams cannot grant tool provenance. */ }
    })();
    return async () => { disposed = true; events.abort(); await runtime.dispose(); };
  },
});

function permissionRules(request: PermissionRequest, directory: string, mode: Mode) {
  const call = request.call;
  if (!call) throw new NativeFailure('policy_denied');
  const detail = call.detail;
  const rules = new Map<string, Set<string>>();
  const add = (action: string, resource: unknown, file = false) => {
    if (typeof resource !== 'string' || !resource.trim() || resource.includes('\0')) throw new NativeFailure('policy_denied');
    const resources = rules.get(action) ?? new Set<string>();
    resources.add(resource);
    if (file) {
      if (mode === 'local') {
        if (/^[A-Za-z]:[^\\/]/.test(resource)) throw new NativeFailure('policy_denied');
        const windows = /^[A-Za-z]:|^\\\\/.test(directory);
        resources.add(windows ? path.win32.resolve(directory, resource).replaceAll('\\', '/') : path.posix.resolve(directory, resource));
      } else {
        // These are VM identities. A relative VM path has no established base here.
        if (path.win32.isAbsolute(resource) && /^[A-Za-z]:|^\\\\/.test(resource)) resources.add(path.win32.normalize(resource).replaceAll('\\', '/'));
        else if (path.posix.isAbsolute(resource)) resources.add(path.posix.normalize(resource));
        else throw new NativeFailure('policy_denied');
      }
    }
    rules.set(action, resources);
  };
  const reportedFiles = call.meta[CognitionMetaKey.FileUpdates];
  if (reportedFiles !== undefined && (!Array.isArray(reportedFiles) || !('files' in detail) || reportedFiles.length !== detail.files.length)) throw new NativeFailure('policy_denied');
  if (detail.tool === 'exec' || (detail.tool === 'permission_request' && detail.permissionType === 'command')) {
    // File locations on a command have no established read/write action. Do not guess.
    if (call.locations.length) throw new NativeFailure('policy_denied');
    add('shell', detail.command);
  } else if (detail.tool === 'read' || detail.tool === 'write' || detail.tool === 'edit' || detail.tool === 'list_directory' || detail.tool === 'file_search') {
    const action = detail.tool === 'write' || detail.tool === 'edit' ? 'edit' : 'read';
    const files = 'files' in detail ? detail.files : [];
    const affected = new Map<string, string>();
    for (const file of files) {
      const effect = file.action === 'open' ? 'read' : file.action === 'create' || file.action === 'edit' || file.action === 'delete' ? 'edit' : file.action === undefined ? action : undefined;
      if (!effect) throw new NativeFailure('policy_denied');
      add(effect, file.path, true);
      affected.set(file.path, effect);
    }
    if (detail.path !== undefined) add(affected.get(detail.path) ?? action, detail.path, true);
    for (const location of call.locations) add(affected.get(location.path) ?? action, location.path, true);
  } else if (call.kind === 'fetch') {
    if (call.locations.length) throw new NativeFailure('policy_denied');
    const input = call.rawInput;
    const url = input && typeof input === 'object' && 'url' in input ? input.url : undefined;
    if (typeof url !== 'string') throw new NativeFailure('policy_denied');
    const parsed = new URL(url);
    if (!['https:', 'http:'].includes(parsed.protocol) || parsed.username || parsed.password) throw new NativeFailure('policy_denied');
    add('webfetch', url);
  } else throw new NativeFailure('policy_denied');
  if (!rules.size) throw new NativeFailure('policy_denied');
  return Array.from(rules, ([action, resources]) => ({ action, resources: [...resources] }));
}
