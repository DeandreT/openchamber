import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { verifyLinuxStage } from './verify-linux-stage.mjs';

const bundle = 'opt/native-fixture';
const native = `${bundle}/openchamber-native`;
const launcher = 'usr/share/applications/test-native.desktop';
const electron = `${bundle}/openchamber`;
const cli = `${bundle}/resources/opencode-cli/opencode`;
const rg = `${bundle}/resources/ripgrep/rg`;
const archive = `${bundle}/resources/app.asar`;
const index = `${bundle}/resources/web-dist/index.html`;
const asset = `${bundle}/resources/web-dist/assets/main-fixture.js`;
const launcherText = (exec = `/${native} %U`) => `[Desktop Entry]\nName=OpenChamber Native\nType=Application\nExec=${exec}\n`;
const cliScript = fileURLToPath(new URL('./verify-linux-stage.mjs', import.meta.url));

function elf(arch) {
  const header = Buffer.alloc(64);
  header.set([0x7f, 0x45, 0x4c, 0x46, 2, 1, 1]);
  header.writeUInt16LE(arch === 'x64' ? 62 : 183, 18);
  return header;
}

function asar(server = { offset: '2', size: 1 }, manifest = { offset: '0', size: 2 }) {
  const web = { 'package.json': manifest, server: { files: server ? { 'index.js': server } : {} } };
  const json = Buffer.from(JSON.stringify({ files: {
    node_modules: { files: { '@openchamber': { files: { web: { files: web } } } } },
  } }));
  const dataStart = 16 + Math.ceil(json.length / 4) * 4;
  const bytes = Buffer.alloc(dataStart + 3);
  bytes.writeUInt32LE(4, 0);
  bytes.writeUInt32LE(dataStart - 8, 4);
  bytes.writeUInt32LE(dataStart - 12, 8);
  bytes.writeUInt32LE(json.length, 12);
  json.copy(bytes, 16);
  bytes.write('{};', dataStart);
  return bytes;
}

function fixture(t, arch = 'x64') {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'openchamber-native-stage-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const inputs = {
    [launcher]: launcherText(),
    [native]: elf(arch),
    [electron]: elf(arch),
    [cli]: elf(arch),
    [rg]: elf(arch),
    [archive]: asar(),
    [index]: '<script type="module" crossorigin src="/assets/main-fixture.js"></script>',
    [asset]: 'export {};',
  };
  for (const [name, content] of Object.entries(inputs)) {
    const file = path.join(root, name);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, content, { mode: [native, electron, cli, rg].includes(name) ? 0o755 : 0o644 });
  }
  return {
    root,
    run: () => verifyLinuxStage(root, arch, launcher, native),
    write: (name, value) => fs.writeFileSync(path.join(root, name), value),
    remove: (name) => fs.unlinkSync(path.join(root, name)),
    readInputs: () => Object.keys(inputs).map((name) => {
      const file = path.join(root, name);
      return [fs.readFileSync(file), fs.statSync(file).mode];
    }),
  };
}

test('the CLI accepts both target architectures without changing staged bytes or modes', (t) => {
  for (const arch of ['x64', 'arm64']) {
    const { root, readInputs } = fixture(t, arch);
    const before = readInputs();
    const result = spawnSync(process.execPath, [cliScript, root, arch, launcher, native], { encoding: 'utf8' });
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /passed static preflight/);
    assert.deepEqual(readInputs(), before);
  }
});

test('the launcher must invoke exactly the staged native binary', (t) => {
  const { run, write } = fixture(t);
  for (const exec of [`/${electron} %U`, `/${native}-old`, 'openchamber-native %U', `env /${native}`, `/${native} --other`]) {
    write(launcher, launcherText(exec));
    assert.throws(run, /Launcher Exec must be/);
  }
  write(launcher, `${launcherText()}Exec=/${electron}\n`);
  assert.throws(run, /Duplicate Exec/);
  write(launcher, `${launcherText()}DBusActivatable=true\n`);
  assert.throws(run, /without D-Bus activation/);
  write(launcher, `${launcherText()}TryExec=/${electron}\n`);
  assert.throws(run, /Launcher TryExec must be/);
  write(launcher, '[Desktop Entry]\nName=OpenChamber Native\nType=Application\n[Desktop Action Other]\n' + `Exec=/${native}\n`);
  assert.throws(run, /Launcher Exec must be/);
  write(launcher, `${launcherText(`/${native}`)}TryExec=/${native}\n`);
  assert.doesNotThrow(run);
  for (const name of ['', 'Name=  \n']) {
    write(launcher, launcherText().replace('Name=OpenChamber Native\n', name));
    assert.throws(run, /Launcher requires a nonempty Name/);
  }
});

test('bad CLI input and bad launchers fail without a success message', (t) => {
  const { root, write } = fixture(t);
  for (const [args, message] of [
    [[], /Usage:/],
    [[root, 'riscv64', launcher, native], /Expected architecture/],
    [[root, 'x64', launcher, `../${native}`], /relative Linux path/],
    [[root, 'x64', launcher, electron], /must be named openchamber-native/],
  ]) {
    const result = spawnSync(process.execPath, [cliScript, ...args], { encoding: 'utf8' });
    assert.equal(result.status, 1, result.stderr);
    assert.match(result.stderr, message);
    assert.equal(result.stdout, '');
  }
  write(launcher, launcherText(`/${electron}`));
  const result = spawnSync(process.execPath, [cliScript, root, 'x64', launcher, native], { encoding: 'utf8' });
  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stderr, /Launcher Exec must be/);
  assert.equal(result.stdout, '');
});

test('all bundled executables must match the requested ELF architecture', (t) => {
  const { run, write } = fixture(t);
  for (const [name, label] of [[native, 'Native executable'], [electron, 'Electron host'], [cli, 'OpenCode CLI'], [rg, 'ripgrep']]) {
    write(name, elf('arm64'));
    assert.throws(run, new RegExp(`${label} architecture mismatch: expected x64, got arm64`));
    write(name, elf('x64'));
  }
  write(cli, Buffer.alloc(64));
  assert.throws(run, /Expected an ELF binary/);
});

test('Linux executable bits are required when visible to Node', { skip: process.platform === 'win32' }, (t) => {
  const { root, run } = fixture(t);
  fs.chmodSync(path.join(root, cli), 0o644);
  assert.throws(run, /Staged file is not executable: .*opencode$/);
});

test('a directory link cannot supply a managed-host file from elsewhere', (t) => {
  const { root, run } = fixture(t);
  const other = fixture(t);
  const directory = path.posix.dirname(cli);
  fs.unlinkSync(path.join(root, cli));
  fs.rmdirSync(path.join(root, directory));
  fs.symlinkSync(path.join(other.root, directory), path.join(root, directory), process.platform === 'win32' ? 'junction' : 'dir');
  assert.throws(run, /Staged path must resolve directly: .*opencode-cli\/opencode$/);
});

test('missing managed-host files fail by name', (t) => {
  for (const name of [electron, cli, rg, archive, index, asset]) {
    const { run, remove } = fixture(t);
    remove(name);
    assert.throws(run, { message: `Missing or empty staged file: ${name}` });
  }
});

test('the ASAR must record packed server bytes inside the file', (t) => {
  const { run, write } = fixture(t);
  for (const entry of [
    null, { size: 1, unpacked: true }, { offset: '2', size: 2 }, { offset: 2, size: 1 }, { offset: ['2'], size: 1 },
  ]) {
    write(archive, asar(entry));
    assert.throws(run, /app\.asar lacks packed bytes for .*\/server\/index\.js/);
  }
  write(archive, Buffer.alloc(16, 0xff));
  assert.throws(run, /Invalid app\.asar header/);
});

test('ASAR server ranges may touch in either order but must not overlap', (t) => {
  const { run, write } = fixture(t);
  assert.doesNotThrow(run);
  write(archive, asar({ offset: '0', size: 1 }, { offset: '1', size: 2 }));
  assert.doesNotThrow(run);
  for (const [server, manifest] of [
    [{ offset: '0', size: 2 }, { offset: '0', size: 2 }],
    [{ offset: '1', size: 2 }, { offset: '0', size: 2 }],
    [{ offset: '0', size: 2 }, { offset: '1', size: 2 }],
  ]) {
    write(archive, asar(server, manifest));
    assert.throws(run, /app\.asar .* overlapping byte ranges/);
  }
});

test('the web index must reference a built local module', (t) => {
  const { run, write } = fixture(t);
  for (const html of [
    '<script type="module" src="/src/main.tsx"></script>',
    '<!-- <script type="module" src="/assets/main-fixture.js"></script> -->',
    `<script data-note=' type="module" src="/assets/main-fixture.js"'></script>`,
    `<div data-note='<script type="module" src="/assets/main-fixture.js"></script>'></div>`,
    `<script>const demo = '<script type="module" src="/assets/main-fixture.js">';</script>`,
    '<template><script type="module" src="/assets/main-fixture.js"></script></template>',
  ]) {
    write(index, html);
    assert.throws(run, /does not name a built .* module script/);
  }
  write(index, '<script type = "module" data-note=">" src = "/assets/main-fixture.js"></script>');
  assert.doesNotThrow(run);
});

test('the web index requires exact asset directory and filename case', (t) => {
  const { root, run, write, remove } = fixture(t);
  remove(asset);
  write(asset.replace('main-fixture', 'main-Fixture'), 'export {};');
  for (const directory of ['Assets', 'ASSETS']) {
    write(index, `<script type="module" src="/${directory}/main-Fixture.js"></script>`);
    assert.throws(run, /does not name a built .* module script/);
  }
  write(index, '<script TYPE="module" SRC="/assets/main-Fixture.js"></script>');
  assert.doesNotThrow(run);
  write(index, '<script type="module" src="/assets/main-fixture.js"></script>');
  assert.throws(run, /Missing or empty staged file: .*main-fixture\.js/);
  const stagedAssets = path.join(root, path.posix.dirname(asset));
  fs.renameSync(stagedAssets, `${stagedAssets}-temp`);
  fs.renameSync(`${stagedAssets}-temp`, path.join(path.dirname(stagedAssets), 'Assets'));
  write(index, '<script type="module" src="/assets/main-Fixture.js"></script>');
  assert.throws(run, /Missing staged directory for: .*\/assets\/main-Fixture\.js/);
});
