import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { assertElfArchitecture } from '../../packages/electron/scripts/verify-linux-appimage.mjs';

const usage = 'Usage: node native/scripts/verify-linux-stage.mjs <stage-root> <x64|arm64> <desktop-file> <native-binary>';
const webPackage = 'node_modules/@openchamber/web';
const maxAsarHeaderBytes = 16 * 1024 * 1024;

// The relative staging paths also name installed paths after adding a leading /.
function assertStagePath(value, label) {
  if (!value.split('/').every((part) => /^[a-zA-Z0-9._+-]+$/.test(part) && part !== '.' && part !== '..')) {
    throw new Error(`${label} must be a relative Linux path with simple path segments: ${value}`);
  }
}

function assertLauncher(text, target) {
  const fields = new Map();
  let inEntry = false;
  let seenEntry = false;
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (line.startsWith('[')) {
      inEntry = line === '[Desktop Entry]';
      if (inEntry && seenEntry) throw new Error('Duplicate [Desktop Entry] in launcher');
      seenEntry ||= inEntry;
    } else if (inEntry) {
      const field = /^(Name|Type|Exec|TryExec|DBusActivatable)\s*=\s*(.*)$/.exec(line);
      if (!field) continue;
      if (fields.has(field[1])) throw new Error(`Duplicate ${field[1]} in launcher`);
      fields.set(field[1], field[2]);
    }
  }
  if (fields.get('Type') !== 'Application') throw new Error('Launcher requires [Desktop Entry] with Type=Application');
  if (!fields.get('Name')) throw new Error('Launcher requires a nonempty Name');
  if (fields.has('DBusActivatable') && fields.get('DBusActivatable') !== 'false') {
    throw new Error('Launcher must use Exec, without D-Bus activation');
  }
  const exec = fields.get('Exec');
  if (exec !== target && exec !== `${target} %U`) throw new Error(`Launcher Exec must be ${target}, optionally followed by %U`);
  if (fields.has('TryExec') && fields.get('TryExec') !== target) throw new Error(`Launcher TryExec must be ${target}`);
}

// Read the script attributes of the generated index. Skip comments, quoted
// values on other tags, inert templates, and raw-text element contents.
function localModuleScripts(html) {
  const tags = /<!--[\s\S]*?(?:-->|$)|<(\/?)([a-z][a-z0-9:-]*)(?=[\s/>])((?:"[^"]*"|'[^']*'|[^"'<>])*)>/gi;
  const attributes = /([^\s/="'<>]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+)))?/g;
  const rawText = new Set(['script', 'style', 'textarea', 'title', 'xmp', 'iframe', 'noembed', 'noframes', 'noscript']);
  const scripts = [];
  let templates = 0;
  for (let tag = tags.exec(html); tag; tag = tags.exec(html)) {
    const [, closing, name, body] = tag;
    if (!name) continue;
    const element = name.toLowerCase();
    if (element === 'template') templates = closing ? Math.max(0, templates - 1) : templates + 1;
    if (closing) continue;
    if (element === 'script' && templates === 0) {
      const values = new Map();
      for (const [, attribute, doubleQuoted, singleQuoted, unquoted] of body.matchAll(attributes)) {
        const key = attribute.toLowerCase();
        if (!values.has(key)) values.set(key, doubleQuoted ?? singleQuoted ?? unquoted ?? '');
      }
      const src = values.get('src');
      if (values.get('type')?.trim().toLowerCase() === 'module' && src && /^\/assets\/[a-z0-9._-]+\.js$/i.test(src)
          && src.startsWith('/assets/')) scripts.push(src);
    }
    if (element === 'plaintext') break;
    if (rawText.has(element)) {
      const end = new RegExp(`</${element}(?=[\\s/>])[^>]*>`, 'gi');
      end.lastIndex = tags.lastIndex;
      if (!end.exec(html)) break;
      tags.lastIndex = end.lastIndex;
    }
  }
  return scripts;
}

// Use the ASAR pickle layout from Electron's packaged-app-contents.cjs.
// Only read the header and check the byte ranges the native bootstrap needs.
function assertPackedServer(archive) {
  const fd = fs.openSync(archive, 'r');
  try {
    const size = fs.fstatSync(fd).size;
    const prefix = Buffer.alloc(16);
    const badHeader = () => new Error('Invalid app.asar header');
    if (fs.readSync(fd, prefix, 0, prefix.length, 0) !== prefix.length) throw badHeader();
    const length = prefix.readUInt32LE(12);
    const headerSize = prefix.readUInt32LE(4);
    const dataStart = 8 + headerSize;
    if (prefix.readUInt32LE(0) !== 4 || prefix.readUInt32LE(8) !== headerSize - 4
        || length === 0 || length > maxAsarHeaderBytes
        || headerSize !== 8 + Math.ceil(length / 4) * 4 || dataStart > size) throw badHeader();
    const header = Buffer.alloc(length);
    if (fs.readSync(fd, header, 0, length, 16) !== length) throw badHeader();
    let tree;
    try {
      tree = JSON.parse(header.toString('utf8'));
    } catch {
      throw badHeader();
    }
    const [manifest, server] = [`${webPackage}/package.json`, `${webPackage}/server/index.js`].map((name) => {
      const entry = name.split('/').reduce((node, part) => node?.files?.[part], tree);
      const decimal = /^[0-9]+$/.exec(entry?.offset);
      const offset = Number(decimal?.[0]);
      if (!entry || entry.files || entry.link || entry.unpacked
          || !decimal || decimal[0] !== entry.offset
          || !Number.isSafeInteger(offset) || !Number.isSafeInteger(entry.size) || entry.size <= 0
          || offset > size - dataStart - entry.size) throw new Error(`app.asar lacks packed bytes for ${name}`);
      return { start: offset, end: offset + entry.size };
    });
    if (manifest.start < server.end && server.start < manifest.end) {
      throw new Error('app.asar web server package.json and server/index.js have overlapping byte ranges');
    }
  } finally {
    fs.closeSync(fd);
  }
}

export function verifyLinuxStage(root, arch, launcher, binary) {
  if (!['x64', 'arm64'].includes(arch)) throw new Error('Expected architecture x64 or arm64');
  assertStagePath(launcher, 'Launcher');
  assertStagePath(binary, 'Native binary');
  if (!launcher.endsWith('.desktop')) throw new Error('Launcher must be a .desktop file');
  if (path.posix.basename(binary) !== 'openchamber-native') throw new Error('Native binary must be named openchamber-native');

  const stage = fs.realpathSync(root);
  const bundle = path.posix.dirname(binary);
  const stagedFile = (relative, executable = false) => {
    const parts = relative.split('/');
    const name = parts.pop();
    let directory = stage;
    for (const part of parts) {
      if (!fs.readdirSync(directory).includes(part)) throw new Error(`Missing staged directory for: ${relative}`);
      directory = path.join(directory, part);
      const stat = fs.lstatSync(directory, { throwIfNoEntry: false });
      if (stat?.isSymbolicLink()) throw new Error(`Staged path must resolve directly: ${relative}`);
      if (!stat?.isDirectory()) throw new Error(`Missing staged directory for: ${relative}`);
    }
    if (!fs.readdirSync(directory).includes(name)) throw new Error(`Missing or empty staged file: ${relative}`);
    const file = path.join(directory, name);
    const stat = fs.lstatSync(file, { throwIfNoEntry: false });
    if (stat?.isSymbolicLink()) throw new Error(`Staged path must resolve directly: ${relative}`);
    if (!stat?.isFile() || stat.size === 0) throw new Error(`Missing or empty staged file: ${relative}`);
    if (executable && process.platform !== 'win32' && (stat.mode & 0o111) === 0) {
      throw new Error(`Staged file is not executable: ${relative}`);
    }
    return file;
  };

  assertLauncher(fs.readFileSync(stagedFile(launcher), 'utf8'), `/${binary}`);
  for (const [relative, label] of [
    [binary, 'Native executable'],
    [path.posix.join(bundle, 'openchamber'), 'Electron host'],
    [path.posix.join(bundle, 'resources/opencode-cli/opencode'), 'OpenCode CLI'],
    [path.posix.join(bundle, 'resources/ripgrep/rg'), 'ripgrep'],
  ]) {
    assertElfArchitecture(stagedFile(relative, true), arch, label);
  }
  assertPackedServer(stagedFile(path.posix.join(bundle, 'resources/app.asar')));
  const webDist = path.posix.join(bundle, 'resources/web-dist');
  const html = fs.readFileSync(stagedFile(`${webDist}/index.html`), 'utf8');
  const scripts = localModuleScripts(html);
  if (scripts.length === 0) throw new Error('web-dist/index.html does not name a built /assets/ module script');
  for (const src of scripts) stagedFile(`${webDist}${src}`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const args = process.argv.slice(2);
    if (args.length !== 4 || args.some((arg) => !arg)) throw new Error(usage);
    verifyLinuxStage(...args);
    console.log(`Native Linux stage passed static preflight for ${args[1]}`);
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
