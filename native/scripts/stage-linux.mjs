import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { parseOpenCodeCliVersion, readPinnedOpenCodeCliVersion } from '../../packages/electron/scripts/opencode-cli-version.mjs';
import { assertElfArchitecture } from '../../packages/electron/scripts/verify-linux-appimage.mjs';
import { stageProgramNotices } from './linux-program-notices.mjs';
import { verifyLinuxStage } from './verify-linux-stage.mjs';

const repo = fileURLToPath(new URL('../../', import.meta.url));
const bundle = 'opt/openchamber-native';
const native = `${bundle}/openchamber-native`;
const launcher = 'usr/share/applications/openchamber-native.desktop';
const usage = 'Usage: bun run native:stage:linux <new-stage-root> <openchamber-native> <web.tgz> <sdk.tgz>';
const json = (file) => JSON.parse(fs.readFileSync(file, 'utf8'));

function run(program, args, cwd, env, options) {
  const result = spawnSync(program, args, { cwd, env, stdio: ['ignore', 'pipe', 'pipe'], encoding: 'utf8', ...options });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${path.basename(program)} ${args[0]} failed: ${result.stderr || result.signal || result.status}`);
  return result.stdout?.trim();
}

// pm pack supplies the publishable workspace files, including generated SDK
// code, extension services and the web UI. Read only regular file entries.
// No tar command or archive-provided path can create a link in the stage.
export async function unpackWorkspace(tarball, destination, expected, sdkVersion) {
  const entries = await new Bun.Archive(await Bun.file(tarball).arrayBuffer()).files();
  const manifest = entries.get('package/package.json');
  if (!manifest) throw new Error(`Missing package.json in ${tarball}`);
  const packed = JSON.parse(await manifest.text());
  assert.equal(packed.name, expected.name);
  assert.equal(packed.version, expected.version);
  const dependencies = { ...expected.dependencies };
  if (dependencies['@openchamber/sdk'] === 'workspace:*') dependencies['@openchamber/sdk'] = sdkVersion;
  assert.deepEqual(packed.dependencies, dependencies, `Packed ${expected.name} dependencies differ from the checkout`);
  for (const [key, value] of entries) {
    const [prefix, ...parts] = key.split('/');
    if (prefix !== 'package' || key.includes('\\') || parts.length === 0
        || parts.some((part) => !part || part === '.' || part === '..' || part.includes(':') || part === 'node_modules')) {
      throw new Error(`Invalid ${expected.name} archive path: ${key}`);
    }
    if (expected.name === '@openchamber/web' && /^package\/(?:bin|server)\/.*\.test\.[cm]?js$/.test(key)) continue;
    const file = path.join(destination, ...parts);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    await Bun.write(file, value);
  }
  for (const executable of Object.values(packed.bin ?? {})) {
    const file = path.resolve(destination, executable);
    if (!file.startsWith(destination + path.sep)) throw new Error(`Invalid package binary: ${executable}`);
    fs.chmodSync(file, 0o755);
  }
}

function installServerDependencies(work, resources, arch, env) {
  const lock = Bun.JSONC.parse(fs.readFileSync(path.join(repo, 'bun.lock'), 'utf8'));
  for (const dir of Object.keys(lock.workspaces)) {
    fs.mkdirSync(path.join(work, dir), { recursive: true });
    fs.copyFileSync(path.join(repo, dir, 'package.json'), path.join(work, dir, 'package.json'));
  }
  fs.copyFileSync(path.join(repo, 'bun.lock'), path.join(work, 'bun.lock'));
  fs.cpSync(path.join(repo, 'bun-patches'), path.join(work, 'bun-patches'), { recursive: true });
  run(process.execPath, ['install', '--filter', '@openchamber/web', '--production', '--frozen-lockfile',
    '--linker=hoisted', '--ignore-scripts', '--backend=copyfile', '--no-env-file',
    '--os=linux', `--cpu=${arch}`, '--libc=glibc'], work, env, { stdio: 'inherit' });

  // Keep hoisted and workspace-local versions in the same lookup order as Bun.
  // Unix workspace links are relative: check and unlink them before the move.
  for (const name of ['web', 'sdk']) {
    const workspace = path.join(work, 'packages', name);
    const link = path.join(work, 'node_modules/@openchamber', name);
    if (!fs.lstatSync(link).isSymbolicLink() || fs.realpathSync(link) !== fs.realpathSync(workspace)) {
      throw new Error(`Expected the installed workspace link for @openchamber/${name}`);
    }
    fs.unlinkSync(link);
  }
  const common = path.join(resources, 'node_modules');
  fs.renameSync(path.join(work, 'node_modules'), common);
  for (const [name, target] of [['web', path.join(resources, 'server')], ['sdk', path.join(common, '@openchamber/sdk')]]) {
    fs.mkdirSync(target, { recursive: true });
    const local = path.join(work, 'packages', name, 'node_modules');
    if (fs.existsSync(local)) fs.renameSync(local, path.join(target, 'node_modules'));
  }
  // These command shims can point back to installation paths. The package
  // entrypoints themselves stay in their packages, with the original modes.
  for (const bin of fs.globSync('**/node_modules/.bin', { cwd: resources })) {
    fs.rmSync(path.join(resources, bin), { recursive: true });
  }
}

// With this lock the three multi-platform packages include other OS binaries
// *inside* the same tarball. Leave their JS, types, helpers and license files.
function trimForeignBinaries(modules, arch) {
  const matches = [
    ['node-pty/prebuilds/**/*', (p) => !p.startsWith(`node-pty/prebuilds/linux-${arch}/`) && /(?:\.(?:node|dll|exe|pdb)|\/spawn-helper)$/.test(p)],
    ['node-pty/third_party/**/*', (p) => /\.(?:dll|exe)$/.test(p)],
    ['bun-pty/rust-pty/target/release/*', (p) => /\.(?:so|dll|dylib)$/.test(p) && !p.endsWith(`/librust_pty${arch === 'arm64' ? '_arm64' : ''}.so`)],
    ['7zip-bin/{linux,mac,win}/*/*', (p) => /\/7za(?:\.exe)?$/.test(p) && p !== `7zip-bin/linux/${arch}/7za`],
  ];
  for (const [glob, remove] of matches) {
    for (const relative of fs.globSync(glob, { cwd: modules })) {
      if (remove(relative.split(path.sep).join('/'))) fs.unlinkSync(path.join(modules, relative));
    }
  }
}

async function stageServer(resources, work, webTarball, sdkTarball, arch, env) {
  fs.mkdirSync(resources, { recursive: true });
  installServerDependencies(work, resources, arch, env);
  const modules = path.join(resources, 'node_modules');
  trimForeignBinaries(modules, arch);
  const sdk = json(path.join(repo, 'packages/sdk/package.json'));
  await unpackWorkspace(sdkTarball, path.join(modules, '@openchamber/sdk'), sdk);
  const server = path.join(resources, 'server');
  await unpackWorkspace(webTarball, server, json(path.join(repo, 'packages/web/package.json')), sdk.version);
  fs.renameSync(path.join(server, 'dist'), path.join(resources, 'web-dist'));
}

async function installTools(resources, env) {
  const cliVersion = readPinnedOpenCodeCliVersion();
  const cliTarget = process.arch === 'x64' ? 'linux-x64-baseline' : 'linux-arm64';
  const cliName = `@opencode/cli-${cliTarget}`;
  const metadata = await download(`https://registry.npmjs.org/${cliName}/${cliVersion}`).then((r) => r.json());
  const url = `https://registry.npmjs.org/${cliName}/-/cli-${cliTarget}-${cliVersion}.tgz`;
  if (metadata.name !== cliName || metadata.version !== cliVersion || metadata.dist?.tarball !== url
      || !/^sha512-[A-Za-z0-9+/]{86}==$/.test(metadata.dist.integrity)) throw new Error(`Invalid npm metadata for ${cliName}`);
  const tgz = await (await download(url)).arrayBuffer();
  const integrity = `sha512-${new Bun.CryptoHasher('sha512').update(tgz).digest('base64')}`;
  if (integrity !== metadata.dist.integrity) throw new Error(`Integrity mismatch for ${cliName}`);

  const bun = path.join(resources, 'bun/bun');
  const cli = path.join(resources, 'opencode-cli/opencode');
  const rg = path.join(resources, 'ripgrep/rg');
  for (const file of [bun, cli, rg]) fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.copyFileSync(process.execPath, bun);
  // The official CLI's Linux package places the executable in package/bin.
  const files = await new Bun.Archive(tgz).files();
  await Bun.write(cli, files.get('package/bin/opencode'));
  const ripgrep = `@vscode/ripgrep-linux-${process.arch}`;
  const rgSource = path.join(resources, 'node_modules', ripgrep, 'bin/rg');
  fs.copyFileSync(rgSource, rg);
  for (const file of [bun, cli, rg]) fs.chmodSync(file, 0o755);
  assert.equal(run(bun, ['--version'], resources, env), process.versions.bun);
  assert.equal(parseOpenCodeCliVersion(run(cli, ['--version'], resources, env, { timeout: 60000 })), cliVersion);
  assert.match(run(rg, ['--version'], resources, env), /^ripgrep \d/);

  for (const [file, source] of [
    ['bun/LICENSE.md', `https://raw.githubusercontent.com/oven-sh/bun/${Bun.revision}/LICENSE.md`],
    ['opencode-cli/LICENSE', `https://raw.githubusercontent.com/anomalyco/opencode/v${cliVersion}/LICENSE`],
  ]) {
    fs.writeFileSync(path.join(resources, file), Buffer.from(await (await download(source)).arrayBuffer()));
  }
  await stageProgramNotices(resources, process.arch, download);
}

async function download(url) {
  const response = await fetch(url);
  if (!response.ok) throw new Error(`Cannot download ${url}: HTTP ${response.status}`);
  return response;
}

async function main(output, binary, webTarball, sdkTarball) {
  const { packageManager } = json(path.join(repo, 'package.json'));
  if (packageManager !== `bun@${process.versions.bun}`) throw new Error(`Staging requires ${packageManager}`);
  if (process.platform !== 'linux' || !['arm64', 'x64'].includes(process.arch)) {
    throw new Error('Stage on the target x64 or arm64 glibc Linux host using its native Bun executable');
  }
  if (fs.readFileSync('/proc/self/maps', 'utf8').includes('ld-musl')) throw new Error('The Linux stage requires glibc');
  if (!Bun.which('patchelf', { PATH: '/usr/bin:/bin' })) throw new Error('Install patchelf on the staging host');
  // A smoke import must never succeed by walking up into a developer checkout.
  for (let dir = fs.realpathSync(path.dirname(output)); ; dir = path.dirname(dir)) {
    if (fs.existsSync(path.join(dir, 'node_modules'))) throw new Error(`Choose a stage outside the node_modules at ${dir}`);
    if (dir === path.dirname(dir)) break;
  }
  assertElfArchitecture(binary, process.arch, 'Native executable');
  for (const input of [webTarball, sdkTarball]) {
    if (!fs.statSync(input).isFile()) throw new Error(`Expected a packed workspace: ${input}`);
  }
  // Starting with a new directory also keeps failed runs from deleting or
  // following anything that belonged to a previous stage.
  process.umask(0o022);
  fs.mkdirSync(output);
  try {
    const stage = fs.realpathSync(output);
    const resources = path.join(stage, bundle, 'resources');
    const server = path.join(resources, 'server');
    const work = fs.mkdtempSync(path.join(stage, '.pack-'));
    const home = path.join(work, 'home');
    const tmp = path.join(work, 'tmp');
    fs.mkdirSync(home);
    fs.mkdirSync(tmp);
    // Isolate npm/Bun user configuration, module resolution, caches and
    // runtime writes from the maintainer's account and development checkout.
    const env = { HOME: home, PATH: '/usr/bin:/bin', TMPDIR: tmp,
      XDG_CONFIG_HOME: path.join(home, 'config'), XDG_CACHE_HOME: path.join(home, 'cache'),
      XDG_DATA_HOME: path.join(home, 'data'), XDG_STATE_HOME: path.join(home, 'state'),
      BUN_INSTALL_CACHE_DIR: path.join(work, 'cache'), BUN_RUNTIME_TRANSPILER_CACHE_PATH: '0',
      DO_NOT_TRACK: '1', NODE_ENV: 'production' };
    await stageServer(resources, work, webTarball, sdkTarball, process.arch, env);
    // The upstream prebuild also searches its build machine and, on x64, the
    // working directory. Its ONNX libraries ship right beside the addon.
    const addon = path.join(resources, `node_modules/sherpa-onnx-linux-${process.arch}/sherpa-onnx.node`);
    run('patchelf', ['--set-rpath', '$ORIGIN', addon], work, env);
    assert.equal(run('patchelf', ['--print-rpath', addon], work, env), '$ORIGIN');
    fs.copyFileSync(path.join(repo, 'LICENSE'), path.join(stage, bundle, 'LICENSE'));
    fs.copyFileSync(binary, path.join(stage, native));
    fs.chmodSync(path.join(stage, native), 0o755);
    fs.mkdirSync(path.dirname(path.join(stage, launcher)), { recursive: true });
    fs.writeFileSync(path.join(stage, launcher), `[Desktop Entry]\nType=Application\nName=OpenChamber Native\nExec=/${native}\nTerminal=false\nCategories=Development;\n`);
    await installTools(resources, env);
    verifyLinuxStage(stage, process.arch, launcher, native);
    run(path.join(resources, 'bun/bun'), ['--no-env-file', '--no-install', '-e', `
      const { startWebUiServer } = await import('./server/index.js');
      if (typeof startWebUiServer !== 'function') throw new Error('Missing web server export');
      const { loadTerminalPtyProvider } = await import('./server/lib/terminal/runtime.js');
      const pty = await loadTerminalPtyProvider();
      if (pty.backend !== 'bun-pty') throw new Error('Missing preferred Bun PTY');
      const child = pty.spawn('/bin/sh', ['-c', 'printf stage-pty'], { cwd: process.cwd(), env: process.env });
      let text = ''; child.onData((chunk) => text += chunk);
      await new Promise((resolve, reject) => child.onExit(({ exitCode }) =>
        exitCode === 0 && text.includes('stage-pty') ? resolve() : reject(new Error('PTY smoke failed'))));
      // Covers the fallback PTY and the Sherpa addon loaded by the dictation
      // worker, including its sibling libraries, without LD_LIBRARY_PATH.
      const { dependencies } = await Bun.file('./package.json').json();
      for (const name of Object.keys(dependencies)) await import(name);
      // Importing the server registers background listeners. This process
      // probes its modules only and has no running server to shut down.
      process.exit(0);
    `], server, env, { timeout: 30000 });
    fs.rmSync(work, { recursive: true });
    console.log(`Staged native Linux ${process.arch} package at ${stage}`);
  } catch (error) {
    fs.rmSync(output, { recursive: true, force: true });
    throw error;
  }
}

if (import.meta.main) {
  try {
    const args = process.argv.slice(2);
    if (args.length !== 4 || args.some((arg) => !arg)) throw new Error(usage);
    await main(...args.map((arg) => path.resolve(arg)));
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
