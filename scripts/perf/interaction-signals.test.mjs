import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { setTimeout as wait } from 'node:timers/promises';
import test from 'node:test';
import { REPO_ROOT } from './isolated-server.mjs';

function alive(pid) {
  try { process.kill(pid, 0); return true; } catch { return false; }
}

function descendants(pid, found = new Set()) {
  found.add(pid);
  try {
    const children = readFileSync(`/proc/${pid}/task/${pid}/children`, 'utf8').trim();
    for (const child of children.split(/\s+/).filter(Boolean).map(Number)) descendants(child, found);
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
  }
  return found;
}

async function until(check, label) {
  const end = Date.now() + 30000;
  while (Date.now() < end) {
    const result = await check();
    if (result) return result;
    await wait(10);
  }
  throw new Error(`Timed out: ${label}`);
}

// Run the smoke file directly so the signalled PID is the test process itself,
// not a node --test supervisor. No UI or real provider is involved.
test('SIGINT and SIGTERM clean up the engine smoke during startup and after readiness', {
  skip: process.platform !== 'linux' || !process.env.OPENCODE_INTERACTION_BINARY,
  timeout: 180000,
}, async (t) => {
  for (const phase of ['provider-starting', 'server-starting', 'ready']) {
    for (const signal of ['SIGINT', 'SIGTERM']) {
      const root = `/tmp/opencode/interaction-signal-${phase}-${signal}-${process.pid}-${Date.now()}`;
      const child = spawn(process.execPath, [join(REPO_ROOT, 'scripts/perf/interaction-engine.test.mjs')], {
        cwd: '/tmp/opencode', detached: true,
        env: {
          PATH: process.env.PATH, LANG: 'C.UTF-8',
          OPENCODE_INTERACTION_BINARY: process.env.OPENCODE_INTERACTION_BINARY,
          OPENCODE_INTERACTION_DIST: process.env.OPENCODE_INTERACTION_DIST ?? join(REPO_ROOT, 'packages/electron/resources/web-dist'),
          OPENCODE_INTERACTION_ROOT: root,
        },
        stdio: ['ignore', 'pipe', 'pipe'],
      });
      let output = '';
      child.stdout.on('data', (data) => { output += data; });
      child.stderr.on('data', (data) => { output += data; });
      const owned = new Set([child.pid]);
      const ports = new Set();
      const exited = new Promise((resolveExit, reject) => {
        child.once('error', reject);
        child.once('close', (code, termination) => resolveExit({ code, termination }));
      });
      try {
        await until(() => {
          if (child.exitCode !== null) throw new Error(output);
          const log = join(root, 'fixture.log');
          if (!existsSync(log)) return false;
          const port = readFileSync(log, 'utf8').match(/listening on http:\/\/127\.0\.0\.1:(\d+)/)?.[1];
          if (!port) return false;
          ports.add(Number(port));
          if (phase === 'ready') return existsSync(join(root, 'smoke-ready.json'));
          if (phase === 'server-starting') {
            const log = join(root, 'server.log');
            if (!existsSync(log) || !readFileSync(log, 'utf8').includes('Starting OpenChamber')) return false;
          }
          assert.equal(existsSync(join(root, 'smoke-ready.json')), false, 'signal must arrive while startup is pending');
          return true;
        }, `${phase} fixture`);
        if (phase === 'ready') {
          const ready = JSON.parse(readFileSync(join(root, 'smoke-ready.json'), 'utf8'));
          ports.add(Number(new URL(ready.url).port));
          owned.add(ready.pid);
          owned.add(ready.fixturePid);
        }
        for (const pid of descendants(child.pid)) owned.add(pid);
        const serverLog = join(root, 'server.log');
        if (existsSync(serverLog)) {
          for (const match of readFileSync(serverLog, 'utf8').matchAll(/(?:127\.0\.0\.1:|allocated port )(\d+)/g)) ports.add(Number(match[1]));
        }
        assert.ok(owned.size > 1, 'the interrupted smoke must own a started child');
        child.kill(signal);
        let timer;
        let result;
        try {
          result = await Promise.race([exited, new Promise((_, reject) => {
            timer = setTimeout(() => reject(new Error(`Smoke did not exit: ${output}`)), 15000);
          })]);
        } finally { clearTimeout(timer); }
        assert.equal(result.termination, null, 'signal must run scoped cleanup, not terminate the smoke directly');
        assert.match(output, new RegExp(`Interaction fixture interrupted by ${signal}`));
        await until(() => [...owned].every((pid) => !alive(pid)), 'all owned processes stopped');
        for (const port of ports) {
          await assert.rejects(fetch(`http://127.0.0.1:${port}/health`, { signal: AbortSignal.timeout(1000) }));
        }
        t.diagnostic(`${phase} ${signal}: ${owned.size} processes stopped, ${ports.size} listeners closed`);
      } finally {
        // Contain a failing regression too, using only the recorded descendants.
        for (const pid of [...owned].reverse()) if (alive(pid)) process.kill(pid, 'SIGKILL');
        await exited;
      }
    }
  }
});
