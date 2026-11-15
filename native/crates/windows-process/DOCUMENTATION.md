# Windows managed-process ownership

## Ownership

This crate owns Win32 process creation, inherited pipe handles, exact process
identity and private job lifetime. The desktop supervisor owns readiness,
restart policy, stdin shutdown and its 35-second grace. External connections
never enter this API. Desktop, core and transport continue to forbid unsafe.

## Invariants

- Create the server suspended with its private kill-on-close job assigned
  atomically by `PROC_THREAD_ATTRIBUTE_JOB_LIST`, then resume its primary
  thread. The job enables neither breakaway flag.
- Retained process and job handles are non-inheritable. Only the child's three
  pipe ends enter its explicit startup handle list.
- Identify a process by its retained kernel handle and exact creation FILETIME.
  No termination operation opens a process by PID or executable name.
- Invalid job assignment fails process creation. A failed preparation or resume
  terminates and reaps the suspended process.
  Closing the last private job handle terminates every remaining member,
  including descendants whose server has already exited.
- Graceful completion requires server exit and zero active job processes.
  Query failures remain errors and cannot count as exhaustion.

## Decisions

### Why a separate unsafe crate and not FFI in the desktop

Win32 requires raw handles, pointers and startup attribute storage. This crate
contains those calls behind owned handles and safe methods so the application
crates retain their unsafe prohibition. Each unsafe block states its lifetime
and ownership assumptions.

### Why the job is assigned atomically at creation

A running child can create descendants before assignment. A suspended child
created before a separate assignment can survive owner death in that gap.
`PROC_THREAD_ATTRIBUTE_JOB_LIST` assigns ownership within process creation;
suspension keeps application code from running before identity is captured.
The separate inherited handle list contains only the child's three pipe ends.

Microsoft documents the job-list attribute for Windows 10 and newer and Windows
Server 2016 and newer in [UpdateProcThreadAttribute](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute).
This is the crate's API baseline. Unsupported attributes fail startup without
a separate-assignment fallback. Runtime tests on Windows 11 do not qualify the
application on Windows 10.
The baseline matches the [Rust MSVC target requirements](https://doc.rust-lang.org/rustc/platform-support/windows-msvc.html).

### Why job accounting is checked during shutdown

Root-process exit cannot prove that detached descendants stopped. The supervisor
reads the job's active-process count while stopping and after a crash, with a
10-millisecond wait between checks and its existing bounded deadlines. These
checks run on the supervisor thread. A ready, idle host has no polling loop.
