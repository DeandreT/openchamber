import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { assertElfArchitecture } from '../../packages/electron/scripts/verify-linux-appimage.mjs';
import { programNotices } from './linux-program-notices.mjs';

const usage = 'Usage: node native/scripts/verify-linux-stage.mjs <stage-root> <x64|arm64> <desktop-file> <native-binary>';

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

// Follow Node's package-directory lookup only as far as the shipped resources.
// require.resolve() here could accept a developer's node_modules or NODE_PATH.
function assertServer(stage, resources, stagedFile, arch) {
  const web = `${resources}/server`;
  const json = (file) => JSON.parse(fs.readFileSync(stagedFile(file), 'utf8'));
  const manifest = json(`${web}/package.json`);
  if (manifest.name !== '@openchamber/web' || manifest.main !== './server/index.js' || manifest.type !== 'module') {
    throw new Error('resources/server/package.json must be the @openchamber/web server package');
  }
  stagedFile(`${web}/server/index.js`);
  const builtins = `${web}/server/built-in-extensions`;
  const registry = json(`${builtins}/registry.json`);
  if (registry.version !== 1 || !Array.isArray(registry.extensions)) throw new Error('Invalid built-in extension registry');
  for (const { directory } of registry.extensions) {
    assert.match(directory, /^[a-z][a-z0-9-]*$/, 'Invalid built-in extension directory');
    stagedFile(`${builtins}/${directory}/package.json`);
  }

  const findPackage = (from, name, optional = false) => {
    if (!/^(?:@[\w.-]+\/)?[\w-][\w.-]*$/.test(name)) throw new Error(`Invalid dependency name: ${name}`);
    for (let dir = from; dir === resources || dir.startsWith(`${resources}/`); dir = path.posix.dirname(dir)) {
      if (path.posix.basename(dir) === 'node_modules') continue;
      const candidate = `${dir}/node_modules/${name}`;
      if (fs.lstatSync(path.join(stage, candidate), { throwIfNoEntry: false })) return candidate;
    }
    if (!optional) throw new Error(`Missing staged dependency ${name} for ${from}`);
  };
  const visited = new Set();
  const visit = (dir, pkg) => {
    visited.add(dir);
    for (const name of Object.keys({ ...pkg.dependencies, ...pkg.peerDependencies, ...pkg.optionalDependencies })) {
      const optional = Object.hasOwn(pkg.optionalDependencies ?? {}, name) || pkg.peerDependenciesMeta?.[name]?.optional;
      const dependency = findPackage(dir, name, optional);
      if (dependency && !visited.has(dependency)) visit(dependency, json(`${dependency}/package.json`));
    }
  };
  visit(web, manifest);
  const ripgrep = findPackage(web, '@vscode/ripgrep');
  stagedFile(`${ripgrep}/LICENSE`);
  stagedFile(`${findPackage(ripgrep, `@vscode/ripgrep-linux-${arch}`)}/LICENSE`);
  stagedFile(`${findPackage(web, '@openchamber/sdk')}/dist/index.js`);
  const bunPty = findPackage(web, 'bun-pty');
  const nodePty = findPackage(web, 'node-pty');
  const sherpa = findPackage(findPackage(web, 'sherpa-onnx-node'), `sherpa-onnx-linux-${arch}`);
  stagedFile(`${sherpa}/package.json`);
  for (const [file, label] of [
    [`${bunPty}/rust-pty/target/release/librust_pty${arch === 'arm64' ? '_arm64' : ''}.so`, 'Bun PTY'],
    [`${nodePty}/prebuilds/linux-${arch}/pty.node`, 'Node PTY'],
    [`${sherpa}/sherpa-onnx.node`, 'Sherpa ONNX'],
    [`${sherpa}/libsherpa-onnx-c-api.so`, 'Sherpa C API'],
    [`${sherpa}/libonnxruntime.so`, 'ONNX runtime'],
  ]) {
    assertElfArchitecture(stagedFile(file), arch, label);
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
  const resources = path.posix.join(bundle, 'resources');
  for (const [relative, label] of [
    [binary, 'Native executable'],
    [`${resources}/bun/bun`, 'Bun runtime'],
    [`${resources}/opencode-cli/opencode`, 'OpenCode CLI'],
    [`${resources}/ripgrep/rg`, 'ripgrep'],
  ]) {
    assertElfArchitecture(stagedFile(relative, true), arch, label);
  }
  stagedFile(`${resources}/bun/LICENSE.md`);
  stagedFile(`${resources}/opencode-cli/LICENSE`);
  for (const [file] of programNotices) stagedFile(`${resources}/notices/${file}`);
  assertServer(stage, resources, stagedFile, arch);
  const webDist = `${resources}/web-dist`;
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
