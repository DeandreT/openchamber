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
const bun = `${bundle}/resources/bun/bun`;
const cli = `${bundle}/resources/opencode-cli/opencode`;
const rg = `${bundle}/resources/ripgrep/rg`;
const server = `${bundle}/resources/server`;
const modules = `${bundle}/resources/node_modules`;
const registry = `${server}/server/built-in-extensions/registry.json`;
const programTexts = ['ripgrep/COPYING', 'ripgrep/LICENSE-MIT', 'ripgrep/UNLICENSE',
  'sherpa-onnx/LICENSE', 'onnxruntime/LICENSE', 'onnxruntime/ThirdPartyNotices.txt']
  .map((file) => `${bundle}/resources/notices/${file}`);
const p7zipTexts = Object.fromEntries(['License.txt', 'copying.txt'].map((name) => [
  `${bundle}/resources/notices/p7zip/${name}`, fs.readFileSync(new URL(`./fixtures/p7zip/DOC/${name}`, import.meta.url)),
]));
const sevenZipLicense = `${modules}/7zip-bin/LICENSE.txt`;
const webManifest = {
  name: '@openchamber/web', type: 'module', main: './server/index.js',
  dependencies: { '@openchamber/sdk': '1', '@vscode/ripgrep': '1', '7zip-bin': '5.2.0', 'bun-pty': '1', 'node-pty': '1', 'sherpa-onnx-node': '1' },
};
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

function fixture(t, arch = 'x64') {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'openchamber-native-stage-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const inputs = {
    [launcher]: launcherText(),
    [native]: elf(arch),
    [bun]: elf(arch),
    [`${bundle}/resources/bun/LICENSE.md`]: 'Bun license fixture',
    [cli]: elf(arch),
    [`${bundle}/resources/opencode-cli/LICENSE`]: 'OpenCode license fixture',
    [rg]: elf(arch),
    ...Object.fromEntries(programTexts.map((file) => [file, 'upstream notice fixture'])),
    ...p7zipTexts,
    [sevenZipLicense]: '7zip-bin MIT wrapper fixture',
    [`${modules}/7zip-bin/linux/${arch}/7za`]: elf(arch),
    [`${modules}/@vscode/ripgrep/LICENSE`]: 'Microsoft wrapper fixture',
    [`${modules}/@vscode/ripgrep-linux-${arch}/LICENSE`]: 'Microsoft platform package fixture',
    [`${server}/package.json`]: JSON.stringify(webManifest),
    [`${server}/server/index.js`]: 'export {};',
    [registry]: '{"version":1,"extensions":[]}',
    ...Object.fromEntries(Object.keys(webManifest.dependencies).map((name) => [`${modules}/${name}/package.json`, JSON.stringify({ name })])),
    [`${modules}/@openchamber/sdk/dist/index.js`]: 'export {};',
    [`${modules}/bun-pty/rust-pty/target/release/librust_pty${arch === 'arm64' ? '_arm64' : ''}.so`]: elf(arch),
    [`${modules}/node-pty/prebuilds/linux-${arch}/pty.node`]: elf(arch),
    [`${modules}/sherpa-onnx-linux-${arch}/package.json`]: '{}',
    [`${modules}/sherpa-onnx-linux-${arch}/sherpa-onnx.node`]: elf(arch),
    [`${modules}/sherpa-onnx-linux-${arch}/libsherpa-onnx-c-api.so`]: elf(arch),
    [`${modules}/sherpa-onnx-linux-${arch}/libonnxruntime.so`]: elf(arch),
    [index]: '<script type="module" crossorigin src="/assets/main-fixture.js"></script>',
    [asset]: 'export {};',
  };
  for (const [name, content] of Object.entries(inputs)) {
    const file = path.join(root, name);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, content, { mode: [native, bun, cli, rg].includes(name) ? 0o755 : 0o644 });
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
  for (const [name, label] of [
    [native, 'Native executable'], [bun, 'Bun runtime'], [cli, 'OpenCode CLI'], [rg, 'ripgrep'],
    [`${modules}/7zip-bin/linux/x64/7za`, '7za'],
    [`${modules}/bun-pty/rust-pty/target/release/librust_pty.so`, 'Bun PTY'],
    [`${modules}/node-pty/prebuilds/linux-x64/pty.node`, 'Node PTY'], [`${modules}/sherpa-onnx-linux-x64/sherpa-onnx.node`, 'Sherpa ONNX'],
  ]) {
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
  fs.rmSync(path.join(root, directory), { recursive: true });
  fs.symlinkSync(path.join(other.root, directory), path.join(root, directory), process.platform === 'win32' ? 'junction' : 'dir');
  assert.throws(run, /Staged path must resolve directly: .*opencode-cli\/opencode$/);
});

test('missing managed-host files fail by name', (t) => {
  for (const name of [bun, `${bundle}/resources/bun/LICENSE.md`, cli, rg, `${server}/package.json`,
    `${server}/server/index.js`, registry, `${modules}/sherpa-onnx-linux-x64/libonnxruntime.so`, index, asset]) {
    const { run, remove } = fixture(t);
    remove(name);
    assert.throws(run, { message: `Missing or empty staged file: ${name}` });
  }
});

test('preflight requires each program notice even when the npm wrapper license is present', (t) => {
  for (const arch of ['x64', 'arm64']) {
    const { run, remove, write } = fixture(t, arch);
    for (const file of [...programTexts, ...Object.keys(p7zipTexts), sevenZipLicense, `${bundle}/resources/opencode-cli/LICENSE`,
      `${modules}/@vscode/ripgrep/LICENSE`, `${modules}/@vscode/ripgrep-linux-${arch}/LICENSE`]) {
      remove(file);
      assert.throws(run, { message: `Missing or empty staged file: ${file}` });
      write(file, p7zipTexts[file] ?? 'restored fixture');
    }
  }
});

test('preflight rejects altered p7zip notices and the wrapper at either program path', (t) => {
  const { root, run, write } = fixture(t);
  const wrapper = fs.readFileSync(path.join(root, sevenZipLicense));
  for (const [file, original] of Object.entries(p7zipTexts)) {
    const altered = Buffer.from(original);
    altered[0] ^= 1;
    for (const bytes of [altered, wrapper]) {
      write(file, bytes);
      assert.throws(run, { message: `SHA-256 mismatch for ${file}; check the upstream program and its notices` });
    }
    write(file, original);
  }
  assert.doesNotThrow(run);
});

test('the packaged server has its own manifest and built-in registry', (t) => {
  const { root, run, write } = fixture(t);
  write(`${server}/package.json`, JSON.stringify({ ...webManifest, name: '@openchamber/electron' }));
  assert.throws(run, /must be the @openchamber\/web server package/);
  write(`${server}/package.json`, JSON.stringify(webManifest));
  const setDirectory = (directory) => write(registry, JSON.stringify({
    version: 1, extensions: [{ id: 'openchamber-builtin-example', directory }],
  }));
  setDirectory('example');
  assert.throws(run, /built-in-extensions\/example\/package.json/);
  const extension = `${server}/server/built-in-extensions/example`;
  fs.mkdirSync(path.join(root, extension));
  write(`${extension}/package.json`, '{"name":"openchamber-builtin-example"}');
  assert.doesNotThrow(run);
  for (const directory of ['../somewhere', ['example'], null, true]) {
    setDirectory(directory);
    assert.throws(run, /Invalid built-in extension directory/);
  }
});

test('dependencies resolve through the shipped package tree only', (t) => {
  const { root, run, write } = fixture(t);
  const local = `${modules}/node-pty/node_modules/node-addon-api`;
  const ambient = `${bundle}/node_modules/node-addon-api`;
  write(`${modules}/node-pty/package.json`, '{"dependencies":{"node-addon-api":"1"}}');
  fs.mkdirSync(path.join(root, ambient), { recursive: true });
  write(`${ambient}/package.json`, '{}');
  assert.throws(run, /Missing staged dependency node-addon-api/);
  fs.mkdirSync(path.join(root, local), { recursive: true });
  write(`${local}/package.json`, '{}');
  assert.doesNotThrow(run);
  fs.rmSync(path.join(root, local), { recursive: true });
  fs.symlinkSync(path.join(root, ambient), path.join(root, local), process.platform === 'win32' ? 'junction' : 'dir');
  assert.throws(run, /Staged path must resolve directly/);
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
