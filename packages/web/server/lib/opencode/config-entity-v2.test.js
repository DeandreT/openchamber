import { afterAll, beforeEach, describe, expect, it, vi } from 'vitest';
import fs from 'fs';
import os from 'os';
import path from 'path';

// The path constants are frozen when the modules load, so point
// XDG_CONFIG_HOME at a scratch directory BEFORE importing them. Nothing in this
// file may touch the real ~/.config/opencode.
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'openchamber-entity-v2-'));
process.env.XDG_CONFIG_HOME = path.join(root, 'xdg');
delete process.env.OPENCODE_CONFIG;

const { parseMdFile } = await import('./shared.js');
const { getAgentConfig, getAgentPermissions, updateAgent, createAgent, listDisabledAgents } = await import('./agents.js');
const { getCommandConfig, updateCommand } = await import('./commands.js');
const { getMcpConfig, listMcpConfigs, updateMcpConfig } = await import('./mcp.js');
const { getProviderSources, removeProviderConfig, upsertProviderConfig } = await import('./providers.js');

let projectDir;

function write(relativePath, content) {
  const filePath = path.join(projectDir, relativePath);
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  fs.writeFileSync(filePath, content, 'utf8');
  return filePath;
}

function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, 'utf8'));
}

describe('entity modules speak OpenCode 2 shapes', () => {
  beforeEach(() => {
    projectDir = fs.mkdtempSync(path.join(root, 'project-'));
  });

  afterAll(() => {
    fs.rmSync(root, { recursive: true, force: true });
    delete process.env.XDG_CONFIG_HOME;
  });

  it('purpose model-only JSONC edits preserve unknown siblings and comments', () => {
    const file = write('opencode.jsonc', `{
      // Keep this configuration comment.
      "agents": { "compaction": { "model": "old/a", "custom": { "keep": true }, "system": "Keep instructions" },
        "title": { "model": "other/b" } }
    }`);
    const result = updateAgent('compaction', { model: 'new/b' }, projectDir);
    expect(result.path).toBe(file);
    const raw = fs.readFileSync(file, 'utf8');
    expect(raw).toContain('// Keep this configuration comment.');
    expect(raw).toContain('"custom": { "keep": true }');
    expect(raw).toContain('Keep instructions');
    expect(getAgentConfig('compaction', projectDir).config.model).toBe('new/b');
    expect(getAgentConfig('title', projectDir).config.model).toBe('other/b');
  });

  it('purpose model-only Markdown edits preserve unknown frontmatter comments and exact body', () => {
    const file = write('.opencode/agents/title.md', '---\n# Keep frontmatter comment\nmodel: old/a # model comment\ncustom: keep\n---\n\nExact body.\n  Indented body.\n');
    const result = updateAgent('title', { model: 'new/b' }, projectDir);
    expect(result.path).toBe(file);
    const raw = fs.readFileSync(file, 'utf8');
    expect(raw).toContain('# Keep frontmatter comment');
    expect(raw).toContain('custom: keep');
    expect(raw).toContain('# model comment');
    expect(raw).toContain('---\n\nExact body.\n  Indented body.\n');
    expect(getAgentConfig('title', projectDir).config.model).toBe('new/b');
  });

  it.each([['assignment', 'new/b#high'], ['null removal', null]])('purpose model-only %s accepts the reader\'s lenient frontmatter', (_operation, model) => {
    const body = '\r\n---\r\n\r\nExact body.\r\n  Indented body.\r\n\r\n';
    const file = write('.opencode/agents/title.md', '\uFEFF---\r\n# Keep frontmatter comment\r\ndescription: Build agent: creates builds\r\nmodel: old/a # Keep model comment\r\nvariant: low # Keep variant comment\r\ncustom:\r\n  keep: true\r\n  note: "keep: nested value"' + body);
    expect(parseMdFile(file).frontmatter.description).toBe('Build agent: creates builds');
    expect(getAgentConfig('title', projectDir).config.model).toBe('old/a#low');
    expect(updateAgent('title', { model }, projectDir)).toEqual({ source: 'md', scope: 'project', path: file });
    const raw = fs.readFileSync(file, 'utf8');
    expect(raw.startsWith('\uFEFF---\r\n')).toBe(true);
    expect(raw.endsWith(body)).toBe(true);
    expect(raw).toContain('# Keep frontmatter comment');
    expect(raw).toContain('# Keep model comment');
    expect(raw).toContain('# Keep variant comment');
    const parsed = parseMdFile(file).frontmatter;
    expect(parsed.description).toBe('Build agent: creates builds');
    expect(parsed.custom).toEqual({ keep: true, note: 'keep: nested value' });
    expect(parsed.variant).toBeUndefined();
    expect(getAgentConfig('title', projectDir).config.model).toBe(model ?? undefined);
  });

  it('purpose model-only null removal retains legacy JSONC siblings and source', () => {
    const file = write('opencode.jsonc', '{\n// keep\n"agent":{"compaction":{"model":"old/a","variant":"high","custom":7}},"other":true\n}');
    expect(updateAgent('compaction', { model: null }, projectDir)).toEqual({ source: 'json', scope: 'project', path: file });
    const raw = fs.readFileSync(file, 'utf8');
    expect(raw).toContain('// keep');
    expect(raw).toContain('"custom":7');
    expect(getAgentConfig('compaction', projectDir).config.model).toBeUndefined();
    expect(raw).not.toContain('"variant"');
    expect(raw).not.toContain('"agents"');
  });

  it('purpose model-only null removal keeps BOM CRLF Markdown body exactly', () => {
    const suffix = '\r\n---\r\n\r\n  Keep body.\r\n\r\n';
    const file = write('.opencode/agent/title.md', '\uFEFF---\r\n# keep\r\nmodel: old/a\r\nvariant: high\r\ncustom: keep' + suffix);
    updateAgent('title', { model: null }, projectDir);
    const raw = fs.readFileSync(file, 'utf8');
    expect(raw.startsWith('\uFEFF---\r\n')).toBe(true);
    expect(raw.endsWith(suffix)).toBe(true);
    expect(raw).toContain('# keep');
    expect(raw).toContain('custom: keep');
    expect(getAgentConfig('title', projectDir).config.model).toBeUndefined();
    expect(raw).not.toContain('variant:');
  });

  it('purpose model-only writes retain both source formats after a failed atomic write', () => {
    for (const [name, relative, original] of [
      ['compaction', 'opencode.json', '{"agents":{"compaction":{"model":"old/a","custom":7}}}'],
      ['title', '.opencode/agents/title.md', '---\nmodel: old/a\ncustom: keep\n---\n\nBody.\n'],
    ]) {
      const file = write(relative, original);
      const failure = vi.spyOn(fs, 'writeFileSync').mockImplementation(() => { throw new Error('fixture write refusal'); });
      try { expect(() => updateAgent(name, { model: 'new/b' }, projectDir)).toThrow(); }
      finally { failure.mockRestore(); }
      expect(fs.readFileSync(file, 'utf8')).toBe(original);
      expect(getAgentConfig(name, projectDir).config.model).toBe('old/a');
    }
  });

  it('purpose model-only built-in override uses the existing user source and null fallback', () => {
    const before = getAgentConfig('compaction', projectDir);
    expect(before.source).toBe('none');
    expect(updateAgent('compaction', { model: null }, projectDir).source).toBe('none');
    const created = updateAgent('compaction', { model: 'ordinary/text#high' }, projectDir);
    try {
      expect(created.source).toBe('md');
      expect(created.scope).toBe('user');
      expect(getAgentConfig('compaction', projectDir).config.model).toBe('ordinary/text#high');
      expect(updateAgent('compaction', { model: null }, projectDir).path).toBe(created.path);
      expect(getAgentConfig('compaction', projectDir).config.model).toBeUndefined();
    } finally { fs.unlinkSync(created.path); }
  });

  it('reads a v1 agent markdown file and answers the v2 shape', () => {
    write('.opencode/agent/reviewer.md', [
      '---',
      'description: Reviewer',
      'model: anthropic/claude-sonnet-4-5',
      'variant: high',
      'temperature: 0.4',
      'maxSteps: 9',
      'permission:',
      '  bash:',
      '    "git push *": ask',
      '  edit: deny',
      '---',
      '',
      'Review for correctness.',
    ].join('\n'));

    const result = getAgentConfig('reviewer', projectDir);
    expect(result.source).toBe('md');
    expect(result.legacy).toBe(true);
    expect(result.config).toEqual({
      description: 'Reviewer',
      model: 'anthropic/claude-sonnet-4-5#high',
      steps: 9,
      system: 'Review for correctness.',
      request: { body: { temperature: 0.4 } },
      permissions: [
        { action: 'shell', resource: 'git push *', effect: 'ask' },
        { action: 'edit', resource: '*', effect: 'deny' },
      ],
    });
  });

  it('rewrites a v1 agent file in place, at its own path, in v2 shape', () => {
    const agentPath = write('.opencode/agent/reviewer.md', [
      '---',
      'description: Reviewer',
      'temperature: 0.4',
      'permission:',
      '  bash: ask',
      '---',
      '',
      'Old prompt.',
    ].join('\n'));

    const result = updateAgent('reviewer', { description: 'Careful reviewer' }, projectDir);

    // Same file: OpenChamber never moves an entity between directories.
    expect(result.path).toBe(agentPath);
    expect(fs.existsSync(path.join(projectDir, '.opencode', 'agents', 'reviewer.md'))).toBe(false);

    const parsed = parseMdFile(agentPath);
    expect(parsed.frontmatter).toEqual({
      description: 'Careful reviewer',
      request: { body: { temperature: 0.4 } },
      permissions: [{ action: 'shell', resource: '*', effect: 'ask' }],
    });
    expect(parsed.body).toBe('Old prompt.');
  });

  it('writes the permission rule array when the client sends a v1 map', () => {
    write('.opencode/agents/reviewer.md', '---\ndescription: Reviewer\n---\n\nPrompt.\n');

    updateAgent('reviewer', { permission: { task: 'deny', write: 'allow' } }, projectDir);

    const parsed = parseMdFile(path.join(projectDir, '.opencode', 'agents', 'reviewer.md'));
    expect(parsed.frontmatter.permissions).toEqual([
      { action: 'subagent', resource: '*', effect: 'deny' },
      { action: 'edit', resource: '*', effect: 'allow' },
    ]);
    expect(parsed.frontmatter.permission).toBeUndefined();
  });

  it('creates a new agent in the v2 agents directory', () => {
    createAgent('fresh', { description: 'Fresh', system: 'Do the thing.' }, projectDir, 'project');

    const created = path.join(projectDir, '.opencode', 'agents', 'fresh.md');
    expect(fs.existsSync(created)).toBe(true);
    expect(parseMdFile(created)).toEqual({
      frontmatter: { description: 'Fresh' },
      body: 'Do the thing.',
    });
  });

  it('moves a v1 JSON agent entry into the agents section of the same file', () => {
    const configPath = write('opencode.json', JSON.stringify({
      agent: {
        reviewer: { prompt: 'Old', permission: { bash: 'ask' } },
        other: { prompt: 'Untouched' },
      },
    }, null, 2));

    updateAgent('reviewer', { system: 'New' }, projectDir);

    const written = readJson(configPath);
    expect(written.agent).toEqual({ other: { prompt: 'Untouched' } });
    expect(written.agents.reviewer).toEqual({
      system: 'New',
      permissions: [{ action: 'shell', resource: '*', effect: 'ask' }],
    });
  });

  it('clears v1 fields at the v2 location they moved to', () => {
    const configPath = write('opencode.json', JSON.stringify({
      agent: { build: { variant: 'fast', temperature: 0.3, top_p: 0.8, mode: 'subagent' } },
    }, null, 2));

    // The client still names the v1 fields, but on a v2 entity `variant` is the
    // `#` suffix on `model` and `temperature`/`top_p` live in `request.body`.
    updateAgent('build', { variant: null, temperature: null, top_p: null }, projectDir);

    const written = readJson(configPath);
    expect(written.agent).toBeUndefined();
    expect(written.agents.build).toEqual({ mode: 'subagent' });
  });

  it('drops only the variant when a model is set', () => {
    write('.opencode/agents/reviewer.md', '---\nmodel: anthropic/claude-sonnet-4-5#high\n---\n\nPrompt.\n');

    updateAgent('reviewer', { variant: null }, projectDir);

    const parsed = parseMdFile(path.join(projectDir, '.opencode', 'agents', 'reviewer.md'));
    expect(parsed.frontmatter.model).toBe('anthropic/claude-sonnet-4-5');
  });

  it('reports global and agent rules in evaluation order', () => {
    write('opencode.json', JSON.stringify({
      permission: { edit: 'allow' },
      tools: { websearch: false },
    }, null, 2));
    write('.opencode/agents/reviewer.md', [
      '---',
      'permissions:',
      '  - action: edit',
      '    resource: "*"',
      '    effect: deny',
      '---',
      '',
      'Prompt.',
    ].join('\n'));

    const result = getAgentPermissions('reviewer', projectDir);
    expect(result.agent).toEqual([{ action: 'edit', resource: '*', effect: 'deny' }]);
    expect(result.effective).toEqual([
      { action: 'websearch', resource: '*', effect: 'deny', source: 'global' },
      { action: 'edit', resource: '*', effect: 'allow', source: 'global' },
      { action: 'edit', resource: '*', effect: 'deny', source: 'agent' },
    ]);
  });

  it('reads a v1 command file and rewrites it in place as subagent', () => {
    const commandPath = write('.opencode/command/review.md', [
      '---',
      'description: Review',
      'model: anthropic/claude-sonnet-4-5',
      'variant: high',
      'subtask: true',
      '---',
      '',
      'Review the current changes.',
    ].join('\n'));

    expect(getCommandConfig('review', projectDir).config).toEqual({
      description: 'Review',
      model: 'anthropic/claude-sonnet-4-5#high',
      subagent: true,
      template: 'Review the current changes.',
    });

    const result = updateCommand('review', { description: 'Review changes' }, projectDir);
    expect(result.path).toBe(commandPath);
    expect(parseMdFile(commandPath).frontmatter).toEqual({
      description: 'Review changes',
      model: 'anthropic/claude-sonnet-4-5#high',
      subagent: true,
    });
  });

  it('rewrites a v1 theme color as the hex OpenCode migrates it to', () => {
    const agentPath = write('.opencode/agent/tinted.md', [
      '---',
      'description: Tinted',
      'color: primary',
      '---',
      '',
      'Prompt.',
    ].join('\n'));

    updateAgent('tinted', { description: 'Still tinted' }, projectDir);

    // `primary` is not a v2 color; the native decoder would skip the whole file.
    expect(parseMdFile(agentPath).frontmatter).toEqual({ description: 'Still tinted', color: '#aaaaaa' });
  });

  it('answers the project v1 override when the user file already holds the v2 spelling', () => {
    const userConfigPath = path.join(process.env.XDG_CONFIG_HOME, 'opencode', 'opencode.json');
    fs.mkdirSync(path.dirname(userConfigPath), { recursive: true });
    fs.writeFileSync(userConfigPath, JSON.stringify({
      mcp: { servers: { docs: { type: 'remote', url: 'https://global.example.com/mcp' } } },
    }, null, 2), 'utf8');
    const projectConfigPath = write('.opencode/opencode.json', JSON.stringify({
      mcp: { docs: { type: 'remote', url: 'https://project.example.com/mcp', timeout: 5000 } },
    }, null, 2));

    try {
      expect(getMcpConfig('docs', projectDir)).toEqual(expect.objectContaining({
        url: 'https://project.example.com/mcp',
        scope: 'project',
        legacy: true,
      }));
      expect(listMcpConfigs(projectDir).find((entry) => entry.name === 'docs').url).toBe('https://project.example.com/mcp');

      updateMcpConfig('docs', { timeout: { catalog: 9000, execution: 9000 } }, projectDir);

      expect(readJson(projectConfigPath).mcp).toEqual({
        servers: { docs: { type: 'remote', url: 'https://project.example.com/mcp', timeout: { catalog: 9000, execution: 9000 } } },
      });
      expect(readJson(userConfigPath).mcp.servers.docs.url).toBe('https://global.example.com/mcp');
    } finally {
      fs.rmSync(userConfigPath, { force: true });
    }
  });

  it('lists servers from a global opencode.jsonc next to opencode.json and edits them in place', () => {
    const configDir = path.join(process.env.XDG_CONFIG_HOME, 'opencode');
    const jsonPath = path.join(configDir, 'opencode.json');
    const jsoncPath = path.join(configDir, 'opencode.jsonc');
    fs.mkdirSync(configDir, { recursive: true });
    fs.writeFileSync(jsonPath, JSON.stringify({ autoupdate: true }, null, 2), 'utf8');
    fs.writeFileSync(jsoncPath, [
      '{',
      '  // servers live only in the jsonc file',
      '  "mcp": { "servers": { "docs": { "type": "remote", "url": "https://docs.example.com/mcp" } } }',
      '}',
    ].join('\n'), 'utf8');

    try {
      expect(listMcpConfigs(projectDir).map((entry) => entry.name)).toContain('docs');
      expect(getMcpConfig('docs', projectDir)).toEqual(expect.objectContaining({ scope: 'user' }));

      updateMcpConfig('docs', { disabled: true }, projectDir);

      expect(getMcpConfig('docs', projectDir)).toEqual(expect.objectContaining({ disabled: true }));
      expect(readJson(jsonPath)).toEqual({ autoupdate: true });
    } finally {
      fs.rmSync(jsonPath, { force: true });
      fs.rmSync(jsoncPath, { force: true });
    }
  });

  it('finds, edits and removes a provider from a global opencode.jsonc next to opencode.json in place', () => {
    const configDir = path.join(process.env.XDG_CONFIG_HOME, 'opencode');
    const jsonPath = path.join(configDir, 'opencode.json');
    const jsoncPath = path.join(configDir, 'opencode.jsonc');
    const jsonConfig = {
      providers: {
        w1: {
          name: 'w1',
          package: 'aisdk:@ai-sdk/openai-compatible',
          settings: { baseURL: 'https://w1.example.com/v1' },
          models: { m: { modelID: 'm', name: 'M' } },
        },
      },
    };
    fs.mkdirSync(configDir, { recursive: true });
    // Settings turned dots3 off, which it records in the primary file.
    fs.writeFileSync(jsonPath, JSON.stringify({ ...jsonConfig, disabled_providers: ['dots3', 'other'] }, null, 2), 'utf8');
    fs.writeFileSync(jsoncPath, [
      '{',
      '  // providers live only in the jsonc file',
      '  "providers": { "dots3": { "name": "dots3", "package": "aisdk:@ai-sdk/openai-compatible",',
      '    "settings": { "baseURL": "https://old.example.com/v1" }, "models": { "m": { "modelID": "m", "name": "M" } } } }',
      '}',
    ].join('\n'), 'utf8');

    try {
      const sources = getProviderSources('dots3', projectDir);
      expect(sources.sources.user).toEqual({ exists: true, path: jsoncPath });
      expect(sources.config.settings).toEqual({ baseURL: 'https://old.example.com/v1' });
      expect(getProviderSources('w1', projectDir).sources.user).toEqual({ exists: true, path: jsonPath });
      expect(getProviderSources('new-llm', projectDir).sources.user).toEqual({ exists: false, path: jsonPath });

      const result = upsertProviderConfig('dots3', {
        name: 'dots3',
        npm: '@ai-sdk/openai-compatible',
        settings: { baseURL: 'https://new.example.com/v1' },
        models: { m: { name: 'M' } },
      }, projectDir, 'user', { hasStoredAuth: true });

      expect(result.path).toBe(jsoncPath);
      expect(fs.readFileSync(jsoncPath, 'utf8')).toContain('// providers live only in the jsonc file');
      expect(getProviderSources('dots3', projectDir).config.settings).toEqual({ baseURL: 'https://new.example.com/v1' });
      expect(readJson(jsonPath)).toEqual({ ...jsonConfig, disabled_providers: ['other'] });

      expect(removeProviderConfig('dots3', projectDir, 'user')).toBe(true);
      expect(getProviderSources('dots3', projectDir).sources.user.exists).toBe(false);
      expect(readJson(jsonPath)).toEqual({ ...jsonConfig, disabled_providers: ['other'] });
    } finally {
      fs.rmSync(jsonPath, { force: true });
      fs.rmSync(jsoncPath, { force: true });
    }
  });

  it('reads a v1 mcp entry and rewrites it under mcp.servers in the same file', () => {
    const configPath = write('opencode.json', JSON.stringify({
      mcp: {
        playwright: { type: 'local', command: ['npx', '@playwright/mcp'], enabled: true, timeout: 30000 },
      },
    }, null, 2));

    expect(getMcpConfig('playwright', projectDir)).toEqual(expect.objectContaining({
      name: 'playwright',
      type: 'local',
      disabled: false,
      timeout: { catalog: 30000, execution: 30000 },
      legacy: true,
    }));

    updateMcpConfig('playwright', { disabled: true }, projectDir);

    const written = readJson(configPath);
    expect(written.mcp.playwright).toBeUndefined();
    expect(written.mcp.servers.playwright).toEqual({
      type: 'local',
      command: ['npx', '@playwright/mcp'],
      disabled: true,
      timeout: { catalog: 30000, execution: 30000 },
    });
  });

  it('lists agents switched off in files and config sections, and forgets them once enabled', () => {
    write('.opencode/agents/incident.md', ['---', 'description: Incident response', 'disabled: true', '---', '', 'Handle incidents.'].join('\n'));
    write('.opencode/agents/ops/oncall.md', ['---', 'disable: true', '---', '', 'On call.'].join('\n'));
    write('.opencode/agents/reviewer.md', ['---', 'description: Reviewer', '---', '', 'Review.'].join('\n'));
    write('opencode.json', JSON.stringify({ agents: { plan: { disabled: true }, build: { color: '#112233' } } }));

    expect(listDisabledAgents(projectDir).map((agent) => agent.name)).toEqual(['incident', 'ops/oncall', 'plan']);
    expect(listDisabledAgents(projectDir)[0]).toMatchObject({ name: 'incident', scope: 'project', description: 'Incident response' });

    updateAgent('incident', { disabled: null }, projectDir);
    updateAgent('plan', { disabled: null }, projectDir);
    expect(listDisabledAgents(projectDir).map((agent) => agent.name)).toEqual(['ops/oncall']);
    expect(getAgentConfig('incident', projectDir).config.description).toBe('Incident response');
  });
});
