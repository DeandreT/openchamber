import fs from 'fs';
import path from 'path';
import yaml from 'yaml';
import { OPENCODE_CONFIG_DIR } from './shared.js';

const GLOBAL_SNIPPET_DIR = path.join(OPENCODE_CONFIG_DIR, 'snippet');
const GLOBAL_SNIPPET_DIR_ALT = path.join(OPENCODE_CONFIG_DIR, 'snippets');
const SNIPPET_EXTENSION = '.md';
const SNIPPET_NAME_PATTERN = /^[a-z0-9][a-z0-9_-]{0,79}$/i;
const HASHTAG_PATTERN = /#([a-z0-9_-]+)/gi;
const MAX_EXPANSION_COUNT = 15;
const MAX_SOURCE_RANGES = 4096;
const MAX_SOURCE_MAP_BYTES = 4 * 1024 * 1024;
const MAX_MAPPED_TEXT_BYTES = 50 * 1024 * 1024;

function sourceMapLimit() {
  const error = new Error('Snippet source map exceeds its limit');
  error.code = 'snippet_source_map_limit';
  return error;
}

function appendSourceRange(ranges, range) {
  if (range.inputStart === range.inputEnd) return;
  const previous = ranges.at(-1);
  if (previous?.inputEnd === range.inputStart && previous.outputEnd === range.outputStart) {
    previous.inputEnd = range.inputEnd;
    previous.outputEnd = range.outputEnd;
    return;
  }
  if (ranges.length >= MAX_SOURCE_RANGES) throw sourceMapLimit();
  ranges.push(range);
}

// Each replace callback names a range in that pass's input. Copy only the
// original-origin segments outside those ranges; replacement text has no origin.
function sourceReplacementPass(ranges) {
  const result = [];
  let index = 0;
  let consumed = 0;
  let delta = 0;
  const copyThrough = (end) => {
    while (index < ranges.length && ranges[index].outputStart < end) {
      const range = ranges[index];
      const start = Math.max(range.outputStart, consumed);
      const stop = Math.min(range.outputEnd, end);
      if (start < stop) appendSourceRange(result, {
        inputStart: range.inputStart + start - range.outputStart,
        inputEnd: range.inputStart + stop - range.outputStart,
        outputStart: start + delta,
        outputEnd: stop + delta,
      });
      if (range.outputEnd > end) break;
      index += 1;
    }
  };
  return {
    replace(start, end, length) {
      copyThrough(start);
      consumed = end;
      delta += length - (end - start);
      while (index < ranges.length && ranges[index].outputEnd <= end) index += 1;
    },
    finish(length) {
      copyThrough(length);
      return result;
    },
  };
}

function mappedSnippetResult(input, raw, sourceRanges, collector) {
  const expanded = raw.trim();
  const trimStart = raw.length - raw.trimStart().length;
  const trimEnd = trimStart + expanded.length;
  const sections = [
    ...collector.prepend.map((text) => ({ text, original: false })),
    { text: expanded, original: true },
    ...collector.append.map((text) => ({ text, original: false })),
  ].filter((section) => section.text);
  const text = sections.map((section) => section.text).join('\n\n');
  if (Buffer.byteLength(input, 'utf8') > MAX_MAPPED_TEXT_BYTES
    || Buffer.byteLength(text, 'utf8') > MAX_MAPPED_TEXT_BYTES) throw sourceMapLimit();
  const mapped = [];
  let offset = 0;
  for (const section of sections) {
    if (section.original) {
      for (const range of sourceRanges) {
        const start = Math.max(range.outputStart, trimStart);
        const end = Math.min(range.outputEnd, trimEnd);
        if (start < end) appendSourceRange(mapped, {
          inputStart: range.inputStart + start - range.outputStart,
          inputEnd: range.inputStart + end - range.outputStart,
          outputStart: offset + start - trimStart,
          outputEnd: offset + end - trimStart,
        });
      }
    }
    offset += section.text.length + 2;
  }
  if (Buffer.byteLength(JSON.stringify({ sourceRanges: mapped }), 'utf8') > MAX_SOURCE_MAP_BYTES) throw sourceMapLimit();
  return { text, sourceRanges: mapped };
}

function getProjectSnippetDirs(workingDirectory) {
  if (!workingDirectory) return [];
  return [
    path.join(workingDirectory, '.opencode', 'snippets'),
    path.join(workingDirectory, '.opencode', 'snippet'),
  ];
}

function getGlobalSnippetDirs() {
  return [GLOBAL_SNIPPET_DIR_ALT, GLOBAL_SNIPPET_DIR];
}

function getLoadDirs(workingDirectory) {
  return [
    ...getGlobalSnippetDirs().map((dir) => ({ dir, source: 'global' })),
    ...getProjectSnippetDirs(workingDirectory).map((dir) => ({ dir, source: 'project' })),
  ];
}

function assertValidSnippetName(name) {
  if (typeof name !== 'string' || !SNIPPET_NAME_PATTERN.test(name)) {
    throw new Error('Snippet name must use letters, numbers, dashes, or underscores');
  }
}

function parseMarkdownFile(filePath) {
  const content = fs.readFileSync(filePath, 'utf8');
  const match = content.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?([\s\S]*)$/);
  if (!match) {
    return { frontmatter: {}, body: content.trim() };
  }
  return {
    frontmatter: yaml.parse(match[1]) || {},
    body: match[2].trim(),
  };
}

function normalizeAliases(frontmatter) {
  const raw = frontmatter.aliases ?? frontmatter.alias;
  if (!raw) return [];
  const aliases = Array.isArray(raw) ? raw : [raw];
  return aliases.map((alias) => String(alias).trim()).filter(Boolean);
}

function writeMarkdownFile(filePath, { content, aliases = [], description }) {
  const frontmatter = {};
  const normalizedAliases = aliases.map((alias) => String(alias).trim()).filter(Boolean);
  if (normalizedAliases.length > 0) frontmatter.aliases = normalizedAliases;
  if (description?.trim()) frontmatter.description = description.trim();

  const body = content ?? '';
  const output = Object.keys(frontmatter).length > 0
    ? `---\n${yaml.stringify(frontmatter)}---\n${body ? `\n${body}` : ''}`
    : body;

  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  fs.writeFileSync(filePath, output, 'utf8');
}

function loadSnippetFile(dir, filename, source) {
  const name = path.basename(filename, SNIPPET_EXTENSION);
  if (!SNIPPET_NAME_PATTERN.test(name)) return null;
  const filePath = path.join(dir, filename);
  const { frontmatter, body } = parseMarkdownFile(filePath);
  return {
    name,
    content: body,
    aliases: normalizeAliases(frontmatter),
    description: typeof frontmatter.description === 'string' ? frontmatter.description : undefined,
    filePath,
    source,
  };
}

function removeSnippetAliases(registry, snippet, canonicalNames) {
  for (const alias of snippet.aliases) {
    const aliasKey = alias.toLowerCase();
    if (canonicalNames.has(aliasKey)) continue;
    if (registry.get(aliasKey) === snippet) registry.delete(aliasKey);
  }
}

function registerSnippet(registry, snippet, canonicalNames) {
  const key = snippet.name.toLowerCase();
  const existing = registry.get(key);
  if (existing?.name.toLowerCase() === key) {
    removeSnippetAliases(registry, existing, canonicalNames);
  }
  registry.set(key, snippet);
  canonicalNames.add(key);
  for (const alias of snippet.aliases) {
    const aliasKey = alias.toLowerCase();
    if (SNIPPET_NAME_PATTERN.test(alias) && !canonicalNames.has(aliasKey)) registry.set(aliasKey, snippet);
  }
}

function loadSnippetRegistry(workingDirectory) {
  const registry = new Map();
  const canonicalNames = new Set();
  for (const { dir, source } of getLoadDirs(workingDirectory)) {
    if (!fs.existsSync(dir)) continue;
    for (const filename of fs.readdirSync(dir)) {
      if (!filename.endsWith(SNIPPET_EXTENSION)) continue;
      try {
        const snippet = loadSnippetFile(dir, filename, source);
        if (snippet) registerSnippet(registry, snippet, canonicalNames);
      } catch (error) {
        console.warn(`[Snippets] Failed to load ${path.join(dir, filename)}:`, error);
      }
    }
  }
  return registry;
}

function listUniqueSnippets(registry) {
  const seen = new Set();
  const snippets = [];
  for (const snippet of registry.values()) {
    const key = `${snippet.source}:${snippet.filePath}`;
    if (seen.has(key)) continue;
    seen.add(key);
    snippets.push(snippet);
  }
  return snippets.sort((a, b) => a.name.localeCompare(b.name));
}

function getWritableSnippetDir(scope, workingDirectory) {
  if (scope === 'project') {
    if (!workingDirectory) throw new Error('Project directory is required for project snippets');
    const preferred = path.join(workingDirectory, '.opencode', 'snippet');
    const alternate = path.join(workingDirectory, '.opencode', 'snippets');
    return fs.existsSync(alternate) && !fs.existsSync(preferred) ? alternate : preferred;
  }
  return fs.existsSync(GLOBAL_SNIPPET_DIR_ALT) && !fs.existsSync(GLOBAL_SNIPPET_DIR)
    ? GLOBAL_SNIPPET_DIR_ALT
    : GLOBAL_SNIPPET_DIR;
}

function findSnippetByName(name, workingDirectory) {
  assertValidSnippetName(name);
  const registry = loadSnippetRegistry(workingDirectory);
  return registry.get(name.toLowerCase()) ?? null;
}

// A name or alias that already triggers a snippet is taken: a second file
// under it would shadow or be shadowed by the first.
function assertSnippetNameFree(name, registry, owner = null) {
  const current = registry.get(name.toLowerCase());
  if (current && current !== owner) throw new Error(`Snippet "${name}" already exists`);
}

function isSameFile(a, b) {
  const first = fs.statSync(a);
  const second = fs.statSync(b);
  return first.dev === second.dev && first.ino === second.ino;
}

// Rename and scope moves keep the file's bytes; a move across volumes (a
// project on another disk than the global config) cannot rename.
function moveSnippetFile(from, to) {
  fs.mkdirSync(path.dirname(to), { recursive: true });
  try {
    fs.renameSync(from, to);
  } catch (error) {
    if (error?.code !== 'EXDEV') throw error;
    fs.copyFileSync(from, to);
    fs.unlinkSync(from);
  }
}

function parseSnippetBlocks(content) {
  const blocks = { prepend: [], append: [] };
  let inline = content;
  for (const type of ['prepend', 'append']) {
    const regex = new RegExp(`<${type}>([\\s\\S]*?)(?:<\\/${type}>|$)`, 'gi');
    inline = inline.replace(regex, (_match, value) => {
      const normalized = String(value).trim();
      if (normalized) blocks[type].push(normalized);
      return '';
    });
  }
  inline = inline.replace(/<inject>[\s\S]*?(?:<\/inject>|$)/gi, '').trim();
  return { inline, prepend: blocks.prepend, append: blocks.append };
}

function expandText(text, registry, expansionCounts, collector, source = null) {
  let expanded = text;
  let changed = true;

  while (changed) {
    const previous = expanded;
    let loopDetected = false;
    const sourcePass = source ? sourceReplacementPass(source.ranges) : null;
    HASHTAG_PATTERN.lastIndex = 0;

    expanded = expanded.replace(HASHTAG_PATTERN, (match, name, offset, input) => {
      if (name.toLowerCase() === 'skill' && input[offset + match.length] === '(') return match;
      const snippet = registry.get(name.toLowerCase());
      if (!snippet) return match;

      const key = snippet.name.toLowerCase();
      const count = (expansionCounts.get(key) || 0) + 1;
      if (count > MAX_EXPANSION_COUNT) {
        loopDetected = true;
        return match;
      }
      expansionCounts.set(key, count);

      const parsed = parseSnippetBlocks(snippet.content);
      for (const block of parsed.prepend) collector.prepend.push(expandText(block, registry, expansionCounts, collector));
      for (const block of parsed.append) collector.append.push(expandText(block, registry, expansionCounts, collector));
      const replacement = expandText(parsed.inline, registry, expansionCounts, collector);
      sourcePass?.replace(offset, offset + match.length, replacement.length);
      return replacement;
    });

    if (sourcePass) source.ranges = sourcePass.finish(previous.length);

    changed = expanded !== previous && !loopDetected;
  }

  return expanded;
}

export function listSnippets(workingDirectory) {
  return listUniqueSnippets(loadSnippetRegistry(workingDirectory));
}

export function getSnippet(name, workingDirectory) {
  return findSnippetByName(name, workingDirectory);
}

export function createSnippet(name, config, workingDirectory, scope = 'global') {
  assertValidSnippetName(name);
  assertSnippetNameFree(name, loadSnippetRegistry(workingDirectory));
  const dir = getWritableSnippetDir(scope, workingDirectory);
  const filePath = path.join(dir, `${name}${SNIPPET_EXTENSION}`);
  if (fs.existsSync(filePath)) throw new Error(`Snippet "${name}" already exists`);
  writeMarkdownFile(filePath, config || {});
  return getSnippet(name, workingDirectory);
}

/**
 * Writes the snippet's fields; `updates.name` renames it and `updates.scope`
 * moves it between the global and the project folder.
 */
export function updateSnippet(name, updates, workingDirectory) {
  assertValidSnippetName(name);
  const registry = loadSnippetRegistry(workingDirectory);
  const existing = registry.get(name.toLowerCase());
  if (!existing) throw new Error(`Snippet "${name}" not found`);
  const { name: nextNameInput, scope: nextScopeInput, ...fields } = updates || {};
  const nextName = String(nextNameInput ?? '').trim() || existing.name;
  const nextScope = nextScopeInput === 'project' || nextScopeInput === 'global' ? nextScopeInput : existing.source;
  assertValidSnippetName(nextName);

  const targetPath = nextName === existing.name && nextScope === existing.source
    ? existing.filePath
    : path.join(getWritableSnippetDir(nextScope, workingDirectory), `${nextName}${SNIPPET_EXTENSION}`);
  if (targetPath !== existing.filePath) {
    assertSnippetNameFree(nextName, registry, existing);
    // A case-only rename finds its own file on a case-insensitive disk; any
    // other file there is a snippet the registry shadows, never overwritten.
    if (fs.existsSync(targetPath) && !isSameFile(targetPath, existing.filePath)) {
      throw new Error(`Snippet "${nextName}" already exists`);
    }
  }

  writeMarkdownFile(existing.filePath, { ...existing, ...fields });
  if (targetPath !== existing.filePath) moveSnippetFile(existing.filePath, targetPath);
  return getSnippet(nextName, workingDirectory);
}

export function deleteSnippet(name, workingDirectory) {
  const existing = findSnippetByName(name, workingDirectory);
  if (!existing) throw new Error(`Snippet "${name}" not found`);
  fs.unlinkSync(existing.filePath);
}

export function expandSnippets(text, workingDirectory, options) {
  const input = text || '';
  const mapped = options?.sourceMap === true;
  // The expand route admits only string prompt text for a source map.
  if (mapped && Buffer.byteLength(input, 'utf8') > MAX_MAPPED_TEXT_BYTES) throw sourceMapLimit();
  const registry = loadSnippetRegistry(workingDirectory);
  const collector = { prepend: [], append: [] };
  const source = mapped ? { ranges: input.length ? [{ inputStart: 0, inputEnd: input.length, outputStart: 0, outputEnd: input.length }] : [] } : null;
  const raw = expandText(input, registry, new Map(), collector, source);
  if (source) return mappedSnippetResult(input, raw, source.ranges, collector);
  const expanded = raw.trim();
  return [...collector.prepend, expanded, ...collector.append].filter(Boolean).join('\n\n');
}

/**
 * What a snippet body, saved or not, sends once used: its own prepend and
 * append blocks placed, and the snippets it names expanded.
 */
export function previewSnippet(content, workingDirectory) {
  const registry = loadSnippetRegistry(workingDirectory);
  const collector = { prepend: [], append: [] };
  const counts = new Map();
  const parsed = parseSnippetBlocks(content || '');
  // The order `expandText` gives a used snippet: its blocks, then its text.
  for (const block of parsed.prepend) collector.prepend.push(expandText(block, registry, counts, collector));
  for (const block of parsed.append) collector.append.push(expandText(block, registry, counts, collector));
  const inline = expandText(parsed.inline, registry, counts, collector).trim();
  return [...collector.prepend, inline, ...collector.append].filter(Boolean).join('\n\n');
}
