import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

import { unpackWorkspace } from './stage-linux.mjs';
import { stageProgramNotices } from './linux-program-notices.mjs';

const bun = { skip: !process.versions.bun && 'Requires the pinned Bun runtime' };

test('a packed web workspace retains its manifest, runtime helpers, assets and notices', bun, async (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'native-pack-test-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const expected = { name: '@openchamber/web', version: '1.0.0', dependencies: { '@openchamber/sdk': 'workspace:*' } };
  const manifest = JSON.stringify({ ...expected, dependencies: { '@openchamber/sdk': '1.0.0' }, bin: { openchamber: './bin/cli.js' } });
  const files = {
    'package/package.json': manifest,
    'package/bin/cli.js': '#!/usr/bin/env node\n',
    'package/server/lib/worker.js': 'export {};',
    'package/server/built-in-extensions/registry.json': '{"version":1,"extensions":[]}',
    'package/dist/index.html': '<html></html>',
    'package/LICENSE': 'license fixture',
  };
  const tarball = path.join(dir, 'web.tgz');
  await Bun.write(tarball, await new Bun.Archive(files, { compress: 'gzip' }).bytes());
  const server = path.join(dir, 'server');
  await unpackWorkspace(tarball, server, expected, '1.0.0');
  for (const [key, value] of Object.entries(files)) assert.equal(fs.readFileSync(path.join(server, key.slice(8)), 'utf8'), value);
  await assert.rejects(unpackWorkspace(tarball, server, expected, '2.0.0'), /dependencies differ/);
});

test('packed files cannot escape their workspace or supply an installed dependency', bun, async (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'native-pack-path-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const expected = { name: '@openchamber/sdk', version: '1.0.0', dependencies: {} };
  const tarball = path.join(dir, 'sdk.tgz');
  const sdk = path.join(dir, 'sdk');
  for (const key of ['package/../outside', 'package/node_modules/injected.js']) {
    const archive = new Bun.Archive({ 'package/package.json': JSON.stringify(expected), [key]: 'unexpected' });
    await Bun.write(tarball, await archive.bytes());
    await assert.rejects(unpackWorkspace(tarball, sdk, expected), /Invalid .* archive path/);
  }
  assert.equal(fs.existsSync(path.join(dir, 'outside')), false);
  assert.equal(fs.existsSync(path.join(sdk, 'node_modules')), false);
});

test('changed Sherpa or ripgrep inputs are refused before fetching notices', async (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'native-notice-inputs-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const put = (file, bytes) => {
    fs.mkdirSync(path.dirname(path.join(dir, file)), { recursive: true });
    fs.writeFileSync(path.join(dir, file), bytes);
  };
  const manifest = (name, version) => put(`node_modules/${name}/package.json`, JSON.stringify({ name, version }));
  const download = () => assert.fail('unexpected notice download');
  manifest('sherpa-onnx-node', '1.13.8');
  manifest('sherpa-onnx-linux-arm64', '1.13.9');
  await assert.rejects(stageProgramNotices(dir, 'arm64', download), /No upstream notice mapping for sherpa-onnx-linux-arm64@1\.13\.9/);
  manifest('sherpa-onnx-linux-arm64', '1.13.8');
  put('ripgrep/rg', 'a different Linux program');
  await assert.rejects(stageProgramNotices(dir, 'arm64', download), /SHA-256 mismatch for ripgrep\/rg/);
});
