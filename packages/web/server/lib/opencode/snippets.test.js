import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import fs from 'fs';
import os from 'os';
import path from 'path';
import {
  createSnippet,
  deleteSnippet,
  expandSnippets,
  getSnippet,
  listSnippets,
  previewSnippet,
  updateSnippet,
} from './snippets.js';

let projectDir;

function writeSnippet(relativePath, content) {
  const filePath = path.join(projectDir, relativePath);
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  fs.writeFileSync(filePath, content, 'utf8');
}

function expectSourceRanges(input, mapped) {
  let inputEnd = 0;
  let outputEnd = 0;
  const scalarBoundary = (text, offset) => !(offset > 0 && offset < text.length
    && /[\uD800-\uDBFF]/.test(text[offset - 1]) && /[\uDC00-\uDFFF]/.test(text[offset]));
  for (const range of mapped.sourceRanges) {
    expect(range.inputStart).toBeGreaterThanOrEqual(inputEnd);
    expect(range.outputStart).toBeGreaterThanOrEqual(outputEnd);
    expect(range.inputEnd).toBeGreaterThan(range.inputStart);
    expect(range.outputEnd - range.outputStart).toBe(range.inputEnd - range.inputStart);
    expect(input.slice(range.inputStart, range.inputEnd)).toBe(mapped.text.slice(range.outputStart, range.outputEnd));
    for (const offset of [range.inputStart, range.inputEnd]) expect(scalarBoundary(input, offset)).toBe(true);
    for (const offset of [range.outputStart, range.outputEnd]) expect(scalarBoundary(mapped.text, offset)).toBe(true);
    inputEnd = range.inputEnd;
    outputEnd = range.outputEnd;
  }
}

describe('snippets', () => {
  beforeEach(() => {
    projectDir = fs.mkdtempSync(path.join(os.tmpdir(), 'openchamber-snippets-'));
  });

  afterEach(() => {
    fs.rmSync(projectDir, { recursive: true, force: true });
  });

  test('loads project snippets with aliases and description', () => {
    writeSnippet('.opencode/snippet/review.md', '---\naliases: [rev]\ndescription: Review helper\n---\nReview carefully.');

    expect(listSnippets(projectDir)).toContainEqual(
      expect.objectContaining({ name: 'review', aliases: ['rev'], description: 'Review helper', source: 'project' }),
    );
    expect(getSnippet('rev', projectDir)).toEqual(expect.objectContaining({ name: 'review' }));
  });

  test('snippet directory wins over snippets directory', () => {
    writeSnippet('.opencode/snippets/same.md', 'Old');
    writeSnippet('.opencode/snippet/same.md', 'New');

    expect(getSnippet('same', projectDir)?.content).toBe('New');
  });

  test('canonical snippet names take precedence over colliding aliases', () => {
    writeSnippet('.opencode/snippets/review.md', 'Review by name');
    writeSnippet('.opencode/snippet/helper.md', '---\naliases: [review, help]\n---\nReview by alias');

    expect(getSnippet('review', projectDir)).toEqual(expect.objectContaining({ name: 'review', content: 'Review by name' }));
    expect(getSnippet('help', projectDir)).toEqual(expect.objectContaining({ name: 'helper', content: 'Review by alias' }));
    expect(expandSnippets('Use #review and #help', projectDir)).toBe('Use Review by name and Review by alias');
  });

  test('canonical snippet names override earlier same-directory aliases', () => {
    writeSnippet('.opencode/snippet/alias-first.md', '---\naliases: [review, assist]\n---\nReview by alias');
    writeSnippet('.opencode/snippet/review.md', 'Review by name');

    const snippetDir = path.join(projectDir, '.opencode', 'snippet');
    const originalReaddirSync = fs.readdirSync;
    fs.readdirSync = function readdirSync(dir, ...args) {
      if (dir === snippetDir) return ['alias-first.md', 'review.md'];
      return originalReaddirSync.call(fs, dir, ...args);
    };

    try {
      expect(getSnippet('review', projectDir)).toEqual(expect.objectContaining({ name: 'review', content: 'Review by name' }));
      expect(getSnippet('assist', projectDir)).toEqual(expect.objectContaining({ name: 'alias-first', content: 'Review by alias' }));
      expect(expandSnippets('Use #review and #assist', projectDir)).toBe('Use Review by name and Review by alias');
    } finally {
      fs.readdirSync = originalReaddirSync;
    }
  });

  test('creates updates and deletes snippets', () => {
    expect(createSnippet('custom-one', { content: 'Body', aliases: ['co'] }, projectDir, 'project')).toEqual(
      expect.objectContaining({ name: 'custom-one', content: 'Body', aliases: ['co'] }),
    );
    expect(updateSnippet('custom-one', { content: 'Updated' }, projectDir)).toEqual(
      expect.objectContaining({ name: 'custom-one', content: 'Updated', aliases: ['co'] }),
    );
    deleteSnippet('custom-one', projectDir);
    expect(getSnippet('custom-one', projectDir)).toBeNull();
  });

  test('expands snippets recursively with prepend and append blocks', () => {
    writeSnippet('.opencode/snippet/base.md', 'Base text');
    writeSnippet('.opencode/snippet/review.md', '<prepend>Before</prepend>Review #base<append>After</append>');

    expect(expandSnippets('Please #review', projectDir)).toBe('Before\n\nPlease Review Base text\n\nAfter');
  });

  test('maps actual recursive replacements and prepend/append placement in UTF-16', () => {
    writeSnippet('.opencode/snippet/native-map-base.md', 'Base text');
    writeSnippet('.opencode/snippet/native-map-review.md', '<prepend>Before</prepend>Review #native-map-base<append>After</append>');
    const input = '  😀 left #native-map-review right 😀  ';
    const mapped = expandSnippets(input, projectDir, { sourceMap: true });
    expect(mapped.text).toBe('Before\n\n😀 left Review Base text right 😀\n\nAfter');
    expect(mapped.text).toBe(expandSnippets(input, projectDir));
    const tokenStart = '  😀 left '.length;
    const tokenEnd = tokenStart + '#native-map-review'.length;
    const outputStart = 'Before\n\n'.length;
    const rightStart = outputStart + '😀 left Review Base text'.length;
    expect(mapped.sourceRanges).toEqual([
      { inputStart: 2, inputEnd: tokenStart, outputStart, outputEnd: outputStart + '😀 left '.length },
      { inputStart: tokenEnd, inputEnd: input.length - 2, outputStart: rightStart, outputEnd: rightStart + ' right 😀'.length },
    ]);
    expectSourceRanges(input, mapped);
  });

  test('identical generated text never acquires an original occurrence', () => {
    writeSnippet('.opencode/snippet/native-map-same.md', '[token]');
    const input = '[token] #native-map-same [token]';
    const tokenEnd = '[token] #native-map-same'.length;
    const mapped = expandSnippets(input, projectDir, { sourceMap: true });
    expect(mapped.text).toBe('[token] [token] [token]');
    expect(mapped.sourceRanges).toEqual([
      { inputStart: 0, inputEnd: '[token] '.length, outputStart: 0, outputEnd: '[token] '.length },
      { inputStart: tokenEnd, inputEnd: input.length, outputStart: '[token] [token]'.length, outputEnd: mapped.text.length },
    ]);
    expectSourceRanges(input, mapped);
  });

  test('source mapping preserves loop counts and distinguishes a same-spelling replacement', () => {
    writeSnippet('.opencode/snippet/native-map-self.md', '<prepend>p</prepend>#native-map-self');
    const input = 'kept #native-map-self suffix #native-map-self';
    const mapped = expandSnippets(input, projectDir, { sourceMap: true });
    const prefix = `${Array(15).fill('p').join('\n\n')}\n\n`;
    expect(mapped.text).toBe(`${prefix}${input}`);
    expect(mapped.text).toBe(expandSnippets(input, projectDir));
    expect(mapped.sourceRanges).toEqual([
      { inputStart: 0, inputEnd: 'kept '.length, outputStart: prefix.length, outputEnd: prefix.length + 'kept '.length },
      { inputStart: 'kept #native-map-self'.length, inputEnd: input.length, outputStart: prefix.length + 'kept #native-map-self'.length, outputEnd: mapped.text.length },
    ]);
    expectSourceRanges(input, mapped);
  });

  test('unknown and skill-call exception tokens keep origin while trims remove only whitespace', () => {
    writeSnippet('.opencode/snippet/skill.md', 'must not expand the call');
    const input = '\uFEFF😀 #skill(arg) #native-map-unknown\n';
    const mapped = expandSnippets(input, projectDir, { sourceMap: true });
    expect(mapped.text).toBe('😀 #skill(arg) #native-map-unknown');
    expect(mapped.sourceRanges).toEqual([{ inputStart: 1, inputEnd: input.length - 1, outputStart: 0, outputEnd: mapped.text.length }]);
    expectSourceRanges(input, mapped);
    expect(expandSnippets(' \n ', projectDir, { sourceMap: true })).toEqual({ text: '', sourceRanges: [] });
  });

  test('source mapping refuses excessive origin fragmentation without changing default limits', () => {
    const tokens = [];
    for (let index = 0; index < 274; index += 1) {
      const name = `native-map-bound-${index}`;
      writeSnippet(`.opencode/snippet/${name}.md`, 'x');
      tokens.push(...Array(15).fill(`#${name}`));
    }
    const input = `left ${tokens.join(' kept ')} right`;
    expect(() => expandSnippets(input, projectDir, { sourceMap: true })).toThrow('source map exceeds');
    expect(expandSnippets(input, projectDir)).toBe(`left ${Array(tokens.length).fill('x').join(' kept ')} right`);
  });

  test('refuses a new snippet under a name or alias already in use', () => {
    writeSnippet('.opencode/snippet/review.md', '---\naliases: [rev]\n---\nOriginal');

    expect(() => createSnippet('review', { content: 'Other' }, projectDir, 'project')).toThrow('already exists');
    expect(() => createSnippet('rev', { content: 'Other' }, projectDir, 'project')).toThrow('already exists');
    expect(getSnippet('review', projectDir)?.content).toBe('Original');
  });

  test('renames a snippet with its fields', () => {
    writeSnippet('.opencode/snippet/old-name.md', '---\naliases: [on]\n---\nBody');

    expect(updateSnippet('old-name', { name: 'new-name', content: 'Changed' }, projectDir)).toEqual(
      expect.objectContaining({ name: 'new-name', content: 'Changed', aliases: ['on'], source: 'project' }),
    );
    expect(fs.existsSync(path.join(projectDir, '.opencode', 'snippet', 'old-name.md'))).toBe(false);
    expect(getSnippet('on', projectDir)).toEqual(expect.objectContaining({ name: 'new-name' }));
  });

  test('renames a snippet by case alone', () => {
    writeSnippet('.opencode/snippet/fix-pr.md', 'Body');

    expect(updateSnippet('fix-pr', { name: 'Fix-PR' }, projectDir)).toEqual(expect.objectContaining({ name: 'Fix-PR', content: 'Body' }));
    expect(fs.readdirSync(path.join(projectDir, '.opencode', 'snippet'))).toEqual(['Fix-PR.md']);
  });

  test('refuses a rename onto a name or alias in use and keeps the snippet', () => {
    writeSnippet('.opencode/snippet/first.md', 'One');
    writeSnippet('.opencode/snippet/second.md', '---\naliases: [two]\n---\nTwo');

    expect(() => updateSnippet('first', { name: 'second', content: 'Lost' }, projectDir)).toThrow('already exists');
    expect(() => updateSnippet('first', { name: 'two' }, projectDir)).toThrow('already exists');
    expect(getSnippet('first', projectDir)?.content).toBe('One');
    expect(getSnippet('second', projectDir)?.content).toBe('Two');
  });

  test('previews an unsaved body with its blocks and the snippets it names', () => {
    const body = '<prepend>Before</prepend>Use #base<append>After</append>';
    writeSnippet('.opencode/snippet/base.md', '<append>Base after</append>Base text');

    expect(previewSnippet(body, projectDir)).toBe('Before\n\nUse Base text\n\nAfter\n\nBase after');
    // The same body saved and used sends the same text.
    writeSnippet('.opencode/snippet/saved.md', body);
    expect(expandSnippets('#saved', projectDir)).toBe(previewSnippet(body, projectDir));
  });

  test('rejects invalid snippet names', () => {
    expect(() => createSnippet('../bad', { content: '' }, projectDir, 'project')).toThrow('Snippet name');
  });
});
