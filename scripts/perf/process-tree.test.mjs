import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import test from 'node:test';
import { stopProcessTree } from './process-tree.mjs';

test('stops only the capture tree, including its managed child', { timeout: 45_000 }, async () => {
  const source = `
    const { spawn } = require('node:child_process');
    const child = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore' });
    console.log(child.pid);
    setInterval(() => {}, 1000);
  `;
  const owned = spawn(process.execPath, ['-e', source], { detached: true, windowsHide: true, stdio: ['ignore', 'pipe', 'inherit'] });
  const unrelated = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { windowsHide: true, stdio: 'ignore' });
  let descendant;
  try {
    const [output] = await once(owned.stdout, 'data');
    descendant = Number(String(output).trim());
    assert.ok(descendant > 0);
    const stopped = await stopProcessTree(owned);
    assert.ok(stopped.includes(descendant));
    assert.throws(() => process.kill(owned.pid, 0), { code: 'ESRCH' });
    assert.throws(() => process.kill(descendant, 0), { code: 'ESRCH' });
    assert.doesNotThrow(() => process.kill(unrelated.pid, 0));
  } finally {
    owned.kill();
    unrelated.kill();
    if (descendant) {
      try { process.kill(descendant); } catch (error) { if (error.code !== 'ESRCH') throw error; }
    }
  }
});
