# Parity correction

The pinned 2.6.1 behavior preserves the previous SSH connection when selecting
another host. A missing remote stop during SSH-to-Local selection alone is not
a parity defect. The earlier stop-on-switch requirement and the associated
product-direction question are withdrawn. No answer to that question is needed
to preserve the reference's background behavior.

Remote stop is best effort. An adopted managed CLI daemon has disconnect
authority under keepRunning=false. A completed cleanup worker does not certify
that remote stop succeeded. The prior bool receipt represented completion;
its mere existence was not a remote-success claim.

The original immutable candidate, source manifest, original handoff and failed
aggregate reproduction remain unchanged. The independent observation that a
local SSH master survived application exit remains a confirmed failure. The
aggregate is not rewritten as passing merely because one premise was wrong.

Do not integrate the older scratch proposals for stop-on-selection, a general
operation-lease framework for selection, permanent close refusal on remote
uncertainty, the proposed 8-second timeout, failure wording or dependency change.
Unit A replaces none of those with another UI policy.

The later terminal preflight must still use the user-requested single aggregate
deadline and prove real quit held beyond 200 ms. Unit A chooses no deadline and
does not provide that proof. The toolkit's final 200 ms cap remains unchanged.

Evidence sources supplied by the coordinator and inspected for Unit A include
the pinned Electron SSH manager's best-effort stop and adopted-daemon ownership,
and the pinned CLI lifecycle JSON at commands-lifecycle.js. CLI stop returns
zero after emitting warning/stopped:false. Existing serve JSON contains port,
PID and launchMode, so Unit A can capture acknowledged launch identity without
introducing a remote protocol.
