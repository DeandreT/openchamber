import { afterAll, afterEach, beforeAll, beforeEach, expect, it, vi } from 'vitest';
import { createHash } from 'node:crypto';
import { execFile } from 'node:child_process';
import { existsSync } from 'node:fs';
import { chmod, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { promisify } from 'node:util';
import express from 'express';
import request from 'supertest';
import { stringify } from 'yaml';
import { createNativeDebUpdater } from './native-deb-updater.js';
import { registerOpenChamberRoutes } from './opencode/openchamber-routes.js';

// Leave the reader, HTTP routes, YAML, stream, digest and filesystem real.
// Windows CI has no Debian database. Only read-only Debian tools may run.
const debian = vi.fn();

const executable = '/opt/openchamber-native/openchamber-native';
const pkg = 'openchamber-native-test';
const current = '0.1.0~test.1-1';
const next = '0.1.0~test.2-1';
const bytes = Buffer.from('unsigned fixture bytes');
const digest = createHash('sha512').update(bytes).digest('base64');
const amd64Manifest = 'latest-fixture-amd64.yml';
const arm64Manifest = 'latest-fixture-arm64.yml';
const archive = (version = next, arch = 'amd64') => ({
  url: `${pkg}_${version}_${arch}.deb`, size: bytes.length, sha512: digest,
});
let channel, origin, home, fixture, app, updater, requests;

const readFixture = (url, options) => {
  const target = new URL(url);
  expect(target.origin).toBe('https://downloads.openchamber.dev');
  expect(options).toMatchObject({ credentials: 'omit', redirect: 'error' });
  expect(options.headers).toBeUndefined();
  return fetch(`${origin}${target.pathname}`, options);
};

beforeAll(async () => {
  channel = createServer((req, res) => {
    requests.push(req.url);
    if (req.url.endsWith('.yml')) {
      res.writeHead(fixture.status, fixture.headers);
      res.end(fixture.raw ?? stringify(fixture.manifest));
    } else if (fixture.pauseDownload) {
      res.write(bytes.subarray(0, 1));
      fixture.pauseDownload();
    } else {
      res.end(fixture.bytes);
    }
  });
  await new Promise((resolve) => channel.listen(0, '127.0.0.1', resolve));
  origin = `http://127.0.0.1:${channel.address().port}`;
});

beforeEach(async () => {
  home = await mkdtemp(path.join(tmpdir(), 'oc-native-update-test-'));
  requests = [];
  fixture = {
    status: 200, headers: {}, bytes, installedVersion: current, arch: 'amd64',
    control: `${pkg}\t${next}\tamd64`, settings: {}, statusField: 'install ok installed', manifestName: amd64Manifest,
    manifest: { version: next, files: [{ ...archive(), url: 'OpenChamber-2.7.1-linux-amd64.deb' }, archive()] },
  };
  debian.mockReset().mockImplementation(async (program, args, { env }) => {
    expect(env).toEqual({ PATH: '/usr/bin:/bin', LC_ALL: 'C' });
    if (program === '/usr/bin/dpkg-query' && args[0] === '--search') {
      expect(args.slice(1)).toEqual(['--', executable]);
      return { stdout: `${pkg}:${fixture.arch}: ${executable}\n` };
    }
    if (program === '/usr/bin/dpkg-query' && args[0] === '--show') {
      expect(args.at(-1)).toBe(`${pkg}:${fixture.arch}`);
      return { stdout: `${pkg}\t${fixture.installedVersion}\t${fixture.arch}\t${fixture.statusField}\t${fixture.manifestName}` };
    }
    if (program === '/usr/bin/dpkg' && args[0] === '--compare-versions') {
      expect(args).toEqual(['--compare-versions', fixture.manifest.version, fixture.comparison ?? 'gt', fixture.installedVersion]);
      if (fixture.warning) return { stdout: '', stderr: 'bad Debian version' };
      if (fixture.manifest.version === fixture.installedVersion || fixture.older) throw Object.assign(new Error(), { code: 1 });
      return { stdout: '' };
    }
    if (program === '/usr/bin/dpkg-deb' && args[0] === '--show') return { stdout: fixture.control };
    throw new Error('Unexpected program or mutation requested');
  });
  const readSettings = async () => fixture.settings;
  updater = createNativeDebUpdater({ executable, readSettings, exec: debian, fetchFile: readFixture });
  app = express();
  registerOpenChamberRoutes(app, {
    process: { env: { OPENCHAMBER_RUNTIME: 'desktop' }, platform: 'linux' },
    desktopUpdater: updater, readSettingsFromDiskMigrated: readSettings,
  });
});

afterEach(async () => {
  vi.restoreAllMocks();
  await rm(home, { recursive: true, force: true });
  const allowed = new Set(['dpkg-query --search', 'dpkg-query --show', 'dpkg --compare-versions', 'dpkg-deb --show']);
  expect(debian.mock.calls.filter(([program, args]) => !allowed.has(`${path.posix.basename(program)} ${args[0]}`))).toEqual([]);
});
afterAll(async () => {
  channel.closeAllConnections();
  await new Promise((resolve) => channel.close(resolve));
});

const check = () => request(app).get('/api/openchamber/update-check?appType=web&reportUsage=false&arch=arm64&currentVersion=999');

it('offers only the installed package and CPU, irrespective of client fields; a remote apply cannot start a download', async () => {
  const { body } = await check().expect(200);
  expect(body).toEqual({
    available: true, packageManager: 'apt', updateOwner: 'native-desktop', installBlocked: 'desktop-host',
    channel: 'stable', version: next, currentVersion: current,
    artifact: { name: archive().url, package: pkg, version: next, architecture: 'amd64', size: bytes.length, sha512: digest },
  });
  await request(app).post('/api/openchamber/update-install').send({ approved: true, url: archive().url }).expect(409, {
    code: 'DESKTOP_UPDATE_LOCAL_ONLY', error: 'Update OpenChamber on the computer it runs on.',
  });
  expect(requests).toEqual([`/desktop/stable/${amd64Manifest}`]);
});

it('reports current only after validating the matching arm64 archive on the selected channel', async () => {
  fixture.arch = 'arm64';
  fixture.manifestName = arm64Manifest;
  fixture.settings.desktopUpdateChannel = 'canary';
  fixture.manifest = { version: current, files: [archive(current, 'arm64'), archive(current)] };
  const { body } = await request(app).get('/api/openchamber/update-check').expect(200);
  expect(body).toMatchObject({ available: false, currentVersion: current, version: null, channel: 'canary' });
  expect(body.artifact).toBeUndefined();
  expect(requests).toEqual([`/desktop/canary/${arm64Manifest}`]);
});

it('does not offer an unsolicited downgrade', async () => {
  fixture.installedVersion = '1:0.1.0-1';
  fixture.older = true;
  expect((await check().expect(200)).body.available).toBe(false);
});

it('uses Debian version inequality for an explicit return to Stable from a Debian Canary', async () => {
  fixture.installedVersion = '2.7.2~canary.20261127.1-1';
  fixture.settings.desktopUpdateChannel = 'stable';
  fixture.comparison = 'ne';
  const { body } = await request(app).get('/api/openchamber/update-check?appType=desktop-native').expect(200);
  expect(body).toMatchObject({ available: true, version: next, channel: 'stable' });
});

it('keeps unrecognized client types on the native host check', async () => {
  const { body } = await request(app).get('/api/openchamber/update-check?appType=unknown').expect(200);
  expect(body.updateOwner).toBe('native-desktop');
  expect(requests).toEqual([`/desktop/stable/${amd64Manifest}`]);
});

it.each([
  ['missing manifest', (f) => { f.status = 404; }],
  ['untrusted redirect', (f) => { f.status = 302; f.headers = { Location: '/other' }; }],
  ['malformed metadata', (f) => { f.raw = '[unterminated'; }],
  ['wrong product', (f) => { f.manifest.files.pop(); }],
  ['wrong architecture', (f) => { f.manifest.files = [archive(next, 'arm64')]; }],
  ['wrong version', (f) => { f.manifest.files = [archive(current)]; }],
  ['foreign URL with the right basename', (f) => { f.manifest.files[1].url = `https://other.invalid/${archive().url}`; }],
  ['ambiguous archive', (f) => { f.manifest.files.push(archive()); }],
  ['no size', (f) => { delete f.manifest.files[1].size; }],
  ['bad digest', (f) => { f.manifest.files[1].sha512 = 'invalid'; }],
])('signals %s as a failed check, including when the version already matches', async (_label, change) => {
  fixture.installedVersion = next;
  change(fixture);
  vi.spyOn(console, 'error').mockImplementation(() => {});
  const { body } = await check().expect(500);
  expect(body.error).toEqual(expect.any(String));
  expect(body.artifact).toBeUndefined();
  expect(requests).toEqual([`/desktop/stable/${amd64Manifest}`]);
});

it('refuses a partial installation before contacting a channel', async () => {
  fixture.statusField = 'install ok unpacked';
  vi.spyOn(console, 'error').mockImplementation(() => {});
  const { body } = await check().expect(500);
  expect(body.error).toContain('installed native Debian package');
  expect(requests).toEqual([]);
});

it.each(['', 'latest-linux.yml', 'https://other.invalid/latest-fixture.yml', '../latest-fixture.yml'])(
  'refuses an unconfigured or untrusted package manifest selector %j before making a request', async (manifestName) => {
    fixture.manifestName = manifestName;
    vi.spyOn(console, 'error').mockImplementation(() => {});
    const { body } = await check().expect(500);
    expect(body.error).toContain('no valid release manifest configured');
    expect(requests).toEqual([]);
  },
);

it('fails the check when dpkg compares an invalid Debian version with a warning', async () => {
  fixture.warning = true;
  fixture.manifest = { version: '2-', files: [archive('2-')] };
  vi.spyOn(console, 'error').mockImplementation(() => {});
  expect((await check().expect(500)).body.error).toContain('compare the native Debian package versions');
});

it('returns a local download only after checking the offer, all bytes, and Debian control identity', async () => {
  const { body: offer } = await check().expect(200);
  const file = await updater.download(offer, home);
  expect(await readFile(file)).toEqual(bytes);
  expect(path.basename(file)).toBe(offer.artifact.name);
  expect(requests).toEqual([`/desktop/stable/${amd64Manifest}`, `/desktop/stable/${amd64Manifest}`, `/desktop/stable/${archive().url}`]);
});

it.each([
  ['same-size corruption', (f) => { f.bytes = Buffer.alloc(bytes.length); }, /SHA-512/],
  ['truncation', (f) => { f.bytes = bytes.subarray(1); }, /size/],
  ['excess bytes', (f) => { f.bytes = Buffer.concat([bytes, bytes]); }, /size/],
  ['wrong embedded package', (f) => { f.control = `electron\t${next}\tamd64`; }, /identity/],
  ['wrong embedded architecture', (f) => { f.control = `${pkg}\t${next}\tarm64`; }, /identity/],
])('rejects %s before handing out a file', async (_label, change, reason) => {
  const offer = await updater.check();
  change(fixture);
  await expect(updater.download(offer, home)).rejects.toThrow(reason);
  expect(await readdir(home)).toEqual([]);
});

it('requires a fresh choice when the published digest changes after lookup', async () => {
  const offer = await updater.check();
  fixture.manifest.files[1].sha512 = Buffer.alloc(64).toString('base64');
  await expect(updater.download(offer, home)).rejects.toThrow('offer has changed');
  expect(requests.every((route) => route.endsWith('.yml'))).toBe(true);
  expect(await readdir(home)).toEqual([]);
});

it('cancels an in-flight download with no file, control read, or installer transaction', async () => {
  const offer = await updater.check();
  const controller = new AbortController();
  const started = new Promise((resolve) => { fixture.pauseDownload = resolve; });
  const transfer = updater.download(offer, home, controller.signal);
  await started;
  controller.abort();
  await expect(transfer).rejects.toThrow();
  expect(await readdir(home)).toEqual([]);
  expect(debian.mock.calls.some(([program]) => program.endsWith('dpkg-deb'))).toBe(false);
});

const hasDebianTools = process.platform === 'linux' && ['dpkg', 'dpkg-query', 'dpkg-deb'].every((tool) => existsSync(`/usr/bin/${tool}`));
it.skipIf(!hasDebianTools)('reads a private dpkg database and the control record of an unsigned Debian archive', async () => {
  const exec = promisify(execFile);
  const db = path.join(home, 'db');
  const stage = path.join(home, 'stage');
  const fields = `Package: ${pkg}\nArchitecture: amd64\nMaintainer: Fixture <fixture@example.invalid>\nDescription: Test archive\nOpenChamber-Update-Manifest: ${amd64Manifest}\n`;
  await mkdir(path.join(db, 'info'), { recursive: true });
  await mkdir(path.join(db, 'updates'));
  await writeFile(path.join(db, 'status'), `${fields}Version: ${current}\nStatus: install ok installed\n\n`);
  await writeFile(path.join(db, 'info', `${pkg}.list`), `/.\n${executable}\n`);
  await mkdir(path.join(stage, 'DEBIAN'), { recursive: true });
  await chmod(path.join(stage, 'DEBIAN'), 0o755);
  await writeFile(path.join(stage, 'DEBIAN', 'control'), `${fields}Version: ${next}\n`);
  const source = path.join(home, archive().url);
  await exec('/usr/bin/dpkg-deb', ['--build', stage, source]);
  fixture.bytes = await readFile(source);
  fixture.manifest.files = [{
    url: archive().url, size: fixture.bytes.length, sha512: createHash('sha512').update(fixture.bytes).digest('base64'),
  }];
  const reader = createNativeDebUpdater({
    executable, readSettings: async () => fixture.settings, fetchFile: readFixture,
    exec: (program, args, options) => exec(program, program === '/usr/bin/dpkg-query' ? [`--admindir=${db}`, ...args] : args, options),
  });
  const offer = await reader.check();
  expect(offer).toMatchObject({ available: true, currentVersion: current, version: next });
  expect(await readFile(await reader.download(offer, home))).toEqual(fixture.bytes);
});
