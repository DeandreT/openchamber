import { execFile } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createWriteStream } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import path from 'node:path';
import { pipeline } from 'node:stream/promises';
import { promisify } from 'node:util';
import { z } from 'zod';
import { desktopUpdateMirrors, isCanaryVersion, normalizeUpdateChannel } from './update-channel.js';
import { readBoundedResponseText } from './guests/response-body.js';

const debianArchitectures = new Set(['amd64', 'arm64']);
const electronManifests = new Set(['latest-linux.yml', 'latest-linux-arm64.yml']);
// A filename-safe version. dpkg checks Debian syntax and precedence.
const versionSyntax = /^\d[0-9A-Za-z.+:~-]*$/;
const packageFields = '${Package}\t${Version}\t${Architecture}';
const installedFields = `${packageFields}\t\${Status}\t\${OpenChamber-Update-Manifest}`;
const releaseSchema = z.object({ version: z.string().regex(versionSyntax), files: z.array(z.unknown()) });
const archiveSchema = z.object({
  size: z.string().regex(/^[1-9]\d*$/).transform(Number).pipe(z.int().positive()),
  sha512: z.hash('sha512', { enc: 'base64' }).refine((value) => Buffer.from(value, 'base64').toString('base64') === value),
});

// Resolve the owner of the executable the parent actually launched. The web
// package, its Bun binary and the HTTP client's version can all be different.
async function installedPackage(executable, run) {
  const unknownPackage = 'Cannot identify the installed native Debian package.';
  if (!path.posix.isAbsolute(executable) || path.posix.basename(executable) !== 'openchamber-native') {
    throw new Error(unknownPackage);
  }
  const query = (args) => run('dpkg-query', args).catch(() => { throw new Error(unknownPackage); });
  const owner = (await query(['--search', '--', executable])).trimEnd();
  const match = /^([a-z0-9][a-z0-9+.-]+)(?::([a-z0-9-]+))?: (.+)$/.exec(owner);
  if (!match || match[3] !== executable) throw new Error(unknownPackage);
  const key = match[2] ? `${match[1]}:${match[2]}` : match[1];
  const record = await query(['--show', `--showformat=${installedFields}`, '--', key]);
  const [name, version, arch, status, manifest] = record.split('\t');
  if (name !== match[1] || (match[2] && arch !== match[2]) || !debianArchitectures.has(arch)
    || !versionSyntax.test(version) || !/^(install|hold) ok installed$/.test(status)) throw new Error(unknownPackage);
  // The package names its architecture's manifest on the release mirror.
  // Electron's files[] is private to its own updater, which selects by suffix.
  if (!/^latest-[a-z0-9]+(?:-[a-z0-9]+)*\.yml$/.test(manifest) || electronManifests.has(manifest)) {
    throw new Error('The installed native package has no valid release manifest configured.');
  }
  return { name, version, arch, manifest };
}

async function readChannelFile(fetchFile, url, signal) {
  // The release mirror serves both files directly. Neither redirects nor
  // addresses taken from the manifest acquire the mirror's trust.
  try {
    const response = await fetchFile(url, { redirect: 'error', credentials: 'omit', signal });
    if (response.status === 200 && response.body) return response;
    await response.body?.cancel();
  } catch {
    signal.throwIfAborted();
  }
  throw new Error('Cannot read the desktop release channel.');
}

async function readNativeArtifact(response, installed) {
  const { parse } = await import('yaml');
  const body = await readBoundedResponseText(response, 128 * 1024);
  if (body.tooLarge) throw new Error('Native update manifest is too large.');
  let manifest;
  try {
    manifest = releaseSchema.parse(parse(body.text, { schema: 'failsafe', maxAliasCount: 0 }));
  } catch {
    // YAML errors can include lines from the remote response.
    throw new Error('Invalid native update manifest.');
  }
  const { version, files } = manifest;
  // Debian omits the epoch in the archive filename; the version compared
  // below and the control record checked after download both include it.
  const name = `${installed.name}_${version.replace(/^\d+:/, '')}_${installed.arch}.deb`;
  const matches = files.filter((file) => file?.url === name);
  if (matches.length !== 1) {
    throw new Error('The channel must list exactly one archive for the installed native package and architecture.');
  }
  const parsed = archiveSchema.safeParse(matches[0]);
  if (!parsed.success) throw new Error('Invalid size or SHA-512 for the native Debian archive.');
  return { name, package: installed.name, architecture: installed.arch, version, ...parsed.data };
}

async function isUpdateAvailable(candidate, current, allowDowngrade, run) {
  try {
    await run('dpkg', ['--compare-versions', candidate, allowDowngrade ? 'ne' : 'gt', current]);
    return true;
  } catch (error) {
    if (error.code === 1 && !error.stderr) return false;
    throw new Error('Cannot compare the native Debian package versions.');
  }
}

export function createNativeDebUpdater({ executable, readSettings, exec = promisify(execFile), fetchFile = fetch }) {
  const run = async (program, args) => {
    const { stdout, stderr } = await exec(`/usr/bin/${program}`, args, {
      env: { PATH: '/usr/bin:/bin', LC_ALL: 'C' }, timeout: 10_000, maxBuffer: 16_384,
    });
    // dpkg can compare malformed versions with a warning and exit zero.
    if (stderr) throw new Error('Cannot read the native Debian package metadata.');
    return stdout;
  };
  const check = async (signal) => {
    const installed = await installedPackage(executable, run);
    const settings = await readSettings();
    const appVersion = installed.version.replace('~canary.', '-canary.');
    const channel = normalizeUpdateChannel(settings?.desktopUpdateChannel, appVersion);
    const url = new URL(installed.manifest, desktopUpdateMirrors[channel]);
    const timeout = AbortSignal.timeout(10_000);
    const response = await readChannelFile(fetchFile, url, signal ? AbortSignal.any([signal, timeout]) : timeout);
    const artifact = await readNativeArtifact(response, installed);
    // Like Electron, an explicit switch back from Canary can offer an older Stable.
    const available = await isUpdateAvailable(artifact.version, installed.version, channel === 'stable' && isCanaryVersion(appVersion), run);
    return {
      available, currentVersion: installed.version, version: available ? artifact.version : null,
      channel, artifact: available ? artifact : undefined, installBlocked: 'desktop-host',
    };
  };

  // Local package preparation only. Recheck the exact offer before fetching;
  // callers receive a private file only after its bytes and Debian identity agree.
  const download = async (offer, directory, signal = new AbortController().signal) => {
    signal.throwIfAborted();
    const { artifact, channel, currentVersion } = await check(signal);
    if (!artifact || currentVersion !== offer?.currentVersion || channel !== offer.channel || !offer.artifact ||
      Object.keys(artifact).some((key) => artifact[key] !== offer.artifact[key])) {
      throw new Error('The native update offer has changed. Check again before downloading.');
    }
    const transfer = AbortSignal.any([signal, AbortSignal.timeout(10 * 60_000)]);
    transfer.throwIfAborted();
    const work = await mkdtemp(path.join(directory, 'openchamber-update-'));
    const file = path.join(work, artifact.name);
    try {
      const url = new URL(encodeURIComponent(artifact.name), desktopUpdateMirrors[channel]);
      const response = await readChannelFile(fetchFile, url, transfer);
      await pipeline(response.body, async function* verifyBytes(chunks) {
        const hash = createHash('sha512');
        let size = 0;
        for await (const chunk of chunks) {
          size += chunk.byteLength;
          if (size > artifact.size) throw new Error('Native archive exceeds its declared size.');
          hash.update(chunk);
          yield chunk;
        }
        if (size !== artifact.size || hash.digest('base64') !== artifact.sha512) {
          throw new Error('Native archive size or SHA-512 mismatch.');
        }
      }, createWriteStream(file, { flags: 'wx', mode: 0o600 }), { signal: transfer });
      const identity = await run('dpkg-deb', ['--show', `--showformat=${packageFields}`, '--', file])
        .catch(() => { throw new Error('Cannot read the downloaded Debian archive.'); });
      if (identity !== `${artifact.package}\t${artifact.version}\t${artifact.architecture}`) {
        throw new Error('Downloaded Debian package identity mismatch.');
      }
      transfer.throwIfAborted();
      return file;
    } catch (error) {
      await rm(work, { recursive: true, force: true });
      throw error;
    }
  };

  return { check, download, installBlocked: 'desktop-host', packageManager: 'apt', updateOwner: 'native-desktop' };
}
