import { execFileSync } from 'node:child_process';
import { platform } from 'node:os';
import { setTimeout as wait } from 'node:timers/promises';

/** Descendants of a process owned by the capture, before its parent exits. */
const childPids = (pid) => {
  if (platform() === 'win32') {
    const output = execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command',
      `$all = @(Get-CimInstance Win32_Process); $root = $all | Where-Object ProcessId -eq ${pid}; $queue = [Collections.Generic.Queue[object]]::new(); $seen = [Collections.Generic.HashSet[int]]::new(); if ($root) { $queue.Enqueue($root); $null = $seen.Add(${pid}) }; while ($queue.Count) { $parent = $queue.Dequeue(); foreach ($p in $all) { if ($p.ParentProcessId -eq $parent.ProcessId -and $p.CreationDate -ge $parent.CreationDate -and $seen.Add([int]$p.ProcessId)) { $p.ProcessId; $queue.Enqueue($p) } } }`,
    ], { encoding: 'utf8', windowsHide: true, timeout: 30_000 });
    return output.split(/\r?\n/).map(Number).filter(Boolean);
  }
  try {
    return execFileSync('pgrep', ['-P', String(pid)], { encoding: 'utf8' }).split('\n').map(Number).filter(Boolean);
  } catch {
    return [];
  }
};

const alive = (pid) => {
  try { process.kill(pid, 0); return true; } catch { return false; }
};

const stop = (pid) => {
  try { process.kill(pid, 'SIGTERM'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
};

/** Windows needs leaf-first termination; killing Chromium's parent can leave its children holding the profile. */
export const stopProcessTree = async (child, children = child.pid && child.exitCode === null ? childPids(child.pid) : []) => {
  if (platform() === 'win32') {
    for (const pid of [...children].reverse()) if (alive(pid)) stop(pid);
    if (child.pid && child.exitCode === null) child.kill('SIGTERM');
  } else {
    if (child.pid && child.exitCode === null) {
      try { process.kill(-child.pid, 'SIGTERM'); } catch { child.kill('SIGTERM'); }
    }
    for (const pid of children) if (alive(pid)) stop(pid);
  }
  for (let attempt = 0; attempt < 40 && child.pid && alive(child.pid); attempt += 1) await wait(250);
  if (child.pid && alive(child.pid)) throw new Error(`Capture process ${child.pid} did not exit`);
  return children;
};
