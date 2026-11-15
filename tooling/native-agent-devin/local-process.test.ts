import { test, expect, beforeAll, afterAll, spyOn } from 'bun:test';
import type { ChildProcess } from 'node:child_process';
import { Readable } from 'node:stream';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { LocalDevin } from '@cognition-ai/sdk';
import { localProcess } from './local-process.ts';

const windows = process.platform === 'win32';
let staging: string;
let executable: string;
beforeAll(() => {
  if (!windows) return;
  staging = mkdtempSync(join(process.env.LOCALAPPDATA ? join(process.env.LOCALAPPDATA, 'Temp', 'opencode') : tmpdir(), 'devin-windows-acp-'));
  executable = join(staging, 'fake-devin.exe');
  const csc = join(process.env.SystemRoot ?? 'C:\\Windows', 'Microsoft.NET', 'Framework64', 'v4.0.30319', 'csc.exe');
  const result = spawnSync(csc, ['/nologo', '/target:exe', `/out:${executable}`, fileURLToPath(new URL('./fixtures/windows-acp.cs', import.meta.url))], { windowsHide: true, encoding: 'utf8' });
  if (result.status !== 0) throw Error(`Fixture compilation failed: ${result.error?.message ?? result.stdout}`);
});
afterAll(() => { if (staging) rmSync(staging, { recursive: true, force: true, maxRetries: 20, retryDelay: 50 }); });
const alive = (pid: number) => { try { process.kill(pid, 0); return true; } catch { return false; } };
async function stopped(pid: number) {
  for (let attempt = 0; attempt < 100 && alive(pid); attempt++) await new Promise((resolve) => setTimeout(resolve, 10));
  expect(alive(pid)).toBe(false);
}

test.skipIf(!windows)('records pinned SDK spawn baseline console allocation', async () => {
  const client = await LocalDevin.spawn(executable, { cwd: staging }, {});
  try {
    const session = await client.createSession();
    const handle = Number(session.id.split('-')[3]);
    // Bun may already suppress console allocation for piped children.
    console.log(JSON.stringify({ proof: 'pinned-sdk-baseline', consoleWindowPresent: handle > 0 }));
  } finally { client.close(); }
});

test.skipIf(!windows)('Node SDK baseline and hidden public-connect wrapper run against the fake CLI', () => {
  const result = spawnSync('node.exe', ['--experimental-transform-types', fileURLToPath(new URL('./local-process-node-smoke.ts', import.meta.url)), executable], { windowsHide: true, encoding: 'utf8', timeout: 10_000 });
  console.log(result.stdout.trim());
  if (result.status !== 0) console.error(result.stderr);
  expect(result.status).toBe(0);
}, 20_000);

test.skipIf(!windows)('Windows public-connect tree has no visible windows and SDK close owns termination', async () => {
  const client = await localProcess.spawn(executable, { cwd: staging }, {});
  let pid = 0;
  try {
    const session = await client.createSession();
    const match = /^fake-(\d+)-console-(\d+)$/.exec(session.id)!;
    pid = Number(match[1]);
    expect(match[2]).toBe('0');
    expect(alive(pid)).toBe(true);
    const snapshot = spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-WindowStyle', 'Hidden', '-Command', `
      $rows = @(Get-CimInstance Win32_Process | Where-Object { $_.ProcessId -eq ${pid} -or $_.ParentProcessId -eq ${pid} } | Select-Object ProcessId,ParentProcessId,Name)
      Add-Type 'using System; using System.Collections.Generic; using System.Runtime.InteropServices; public class WindowProof { public delegate bool Callback(IntPtr w, IntPtr p); [DllImport("user32.dll")] static extern bool EnumWindows(Callback c, IntPtr p); [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr w); [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr w, out uint p); public static uint[] Visible() { var ids = new List<uint>(); EnumWindows((w,p) => { uint id; GetWindowThreadProcessId(w, out id); if(IsWindowVisible(w)) ids.Add(id); return true; }, IntPtr.Zero); return ids.ToArray(); } }'
      $visible = @([WindowProof]::Visible() | Where-Object { $_ -in $rows.ProcessId })
      @{ processes = $rows; visibleWindowProcessIds = $visible } | ConvertTo-Json -Depth 4 -Compress
    `], { windowsHide: true, encoding: 'utf8' });
    expect(snapshot.status).toBe(0);
    const proof = JSON.parse(snapshot.stdout);
    const rows = proof.processes;
    expect(proof.visibleWindowProcessIds).toEqual([]);
    console.log(JSON.stringify({ proof: 'windows-process-snapshot', rows }));
    const target = rows.find((row) => row.ProcessId === pid);
    expect(target?.ParentProcessId).toBe(process.pid);
    // Bun may own an invisible console host even when the child has no console.
    expect(rows.filter((row) => row.ProcessId !== pid).every((row) => row.Name === 'conhost.exe')).toBe(true);
    console.log(JSON.stringify({ proof: 'windows-local-process', consoleWindow: Number(match[2]), ...proof, sdkOwnsChild: true }));
    const turn = session.stream('local fixture only');
    expect((await turn.next()).done).toBe(false);
    session.cancel();
    expect(await turn.next()).toMatchObject({ done: true, value: { stopReason: 'cancelled' } });
  } finally { client.close(); }
  await stopped(pid);
});

test.skipIf(!windows)('missing executable and pre-handshake exit reject safely', async () => {
  await expect(localProcess.spawn(join(staging, 'absent.exe'), {}, {})).rejects.toMatchObject({ code: 'unavailable' });
  await expect(localProcess.spawn(executable, {}, { FAKE_ACP_EXIT: '1' })).rejects.toMatchObject({ code: 'unavailable' });
});

test.skipIf(!windows)('child exit rejects an in-flight SDK turn and repeated close is safe', async () => {
  const client = await localProcess.spawn(executable, {}, { FAKE_ACP_EXIT: 'turn' });
  const session = await client.createSession();
  const pid = Number(session.id.split('-')[1]);
  try { await expect(session.stream('fixture only').next()).rejects.toThrow(); }
  finally { client.close(); client.close(); }
  await stopped(pid);
});

for (const mode of ['final', 'turn']) test.skipIf(!windows)(`paused SDK stdout preserves ${mode === 'final' ? 'a final response after exit' : 'bounded rejection when EOF is absent'}`, async () => {
  let child: ChildProcess | undefined;
  let readable: Readable | undefined;
  const connect = LocalDevin.connect.bind(LocalDevin);
  const observed = spyOn(LocalDevin, 'connect').mockImplementation(async (...args) => {
    child = args[2] ?? undefined;
    if (args[0].readable instanceof Readable) readable = args[0].readable;
    return connect(...args);
  });
  let client: LocalDevin | undefined;
  try {
    client = await localProcess.spawn(executable, {}, { FAKE_ACP_EXIT: mode });
    const session = await client.createSession();
    if (!child || !readable) throw Error('SDK did not receive the owned child and readable stream');
    const stdout = readable;
    stdout.pause();
    const exited = new Promise<void>((resolve) => child!.once('exit', () => resolve()));
    const result = session.stream('fixture only').next().then((value) => ({ value }), (error: unknown) => ({ error }));
    await exited;
    console.log(JSON.stringify({ proof: 'bun-buffer-at-exit', mode, readableLength: stdout.readableLength, readableEnded: stdout.readableEnded }));
    if (mode === 'final') {
      stdout.resume();
      expect(await result).toMatchObject({ value: { done: true, value: { stopReason: 'end_turn' } } });
    } else {
      expect(await result).toHaveProperty('error');
      stdout.resume();
    }
  } finally {
    client?.close();
    if (readable) expect(readable.listenerCount('data')).toBe(0);
    observed.mockRestore();
  }
});

test.skipIf(!windows)('progressing stdout preserves the final SDK response across the original exit deadline', async () => {
  let child: ChildProcess | undefined;
  let readable: Readable | undefined;
  const connect = LocalDevin.connect.bind(LocalDevin);
  const observed = spyOn(LocalDevin, 'connect').mockImplementation(async (...args) => {
    child = args[2] ?? undefined;
    if (args[0].readable instanceof Readable) readable = args[0].readable;
    return connect(...args);
  });
  let client: LocalDevin | undefined;
  try {
    client = await localProcess.spawn(executable, {}, { FAKE_ACP_EXIT: 'final' });
    const session = await client.createSession();
    if (!child || !readable) throw Error('Missing SDK child and readable');
    const stdout = readable;
    stdout.pause();
    const exited = new Promise<void>((resolve) => child!.once('exit', () => resolve()));
    const result = session.stream('fixture only').next().then((value) => ({ value }), (error: unknown) => ({ error }));
    await exited;
    let bytes = 0;
    const started = performance.now();
    // Pending bytes must also survive a consumer pause beyond the idle limit.
    await new Promise((resolve) => setTimeout(resolve, 1_250));
    while (stdout.readableLength > 0) {
      const chunk: Buffer | null = stdout.read(Math.min(10, stdout.readableLength));
      bytes += chunk?.length ?? 0;
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
    stdout.resume();
    expect(performance.now() - started).toBeGreaterThan(1_000);
    expect(bytes).toBeGreaterThan(0);
    expect(await result).toMatchObject({ value: { done: true, value: { stopReason: 'end_turn' } } });
    console.log(JSON.stringify({ proof: 'bun-progressing-drain-after-exit', bytes, drainMilliseconds: Math.round(performance.now() - started), stopReason: 'end_turn' }));
  } finally {
    client?.close();
    if (readable) expect(readable.listenerCount('data')).toBe(0);
    observed.mockRestore();
  }
});
