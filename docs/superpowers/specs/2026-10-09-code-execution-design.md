# Code execution with Piston – design

Date: 2026-10-09
Status: approved in chat, awaiting spec review

## Goal

Let people in a pad run the pad's code and see the output together. Users are
third parties who join by invitation, so the code they run is untrusted.
Execution must stay isolated from the Rustpad server, the host and the internal
network. Abuse by volume (rate limiting) is out of scope.

The work borrows its design from the Piston integration in
`gagbo/rustpad` (cloned at `~/Code/Public/rustpad`). The code is written
fresh against this fork.

## Decisions

| Topic | Decision |
|---|---|
| Runtime | [Piston](https://github.com/engineer-man/piston), self-hosted |
| Integration | Rustpad server calls Piston for the client; the browser never reaches Piston |
| Piston client | In-house, thin, on `reqwest` with rustls (no `piston_rs`) |
| Run model | Shared: one person runs, everyone in the pad sees the result |
| Language | Follows the editor language; no separate runtime picker |
| Run button | Sidebar, under the Language select |
| Production | Kubernetes (EKS/GKE); manifests live in the infra repo, not here |
| This repo ships | Server, frontend, local docker-compose, runtime install script |

## Server

### `rustpad-server/src/piston.rs` (new)

`PistonClient` holds a `reqwest::Client` and a base URL, and exposes:

- `runtimes() -> Result<Vec<PistonRuntime>>`, which calls `GET {base}/api/v2/runtimes`.
- `execute(language, version, source) -> Result<RunResult>`, which calls
  `POST {base}/api/v2/execute`.

Behaviour:

- Connect timeout 2 s, total request timeout 20 s. Both are constructor fields
  so tests can shrink them.
- The response body is capped at 1 MiB; a larger body is an error.
- `stdout` and `stderr` of each stage are cut to 64 KiB on a UTF-8 character
  boundary, with `truncated: true` set on the stage.
- Errors map to a fixed set of user-facing messages ("Code runner is
  unavailable", "Code runner timed out", "Code runner returned an invalid
  response"). Full detail goes to the server log only.
- The client calls only `runtimes` and `execute`. Nothing in Rustpad can reach
  Piston's `/api/v2/packages`.

Each execute request sends our own limits: `run_timeout` 3000 ms,
`compile_timeout` 10000 ms, `run_memory_limit` and `compile_memory_limit`
256 MiB. Piston's own config sets the ceiling.

### Runtime resolution

A background task fetches `/runtimes` at startup and every 5 minutes and stores
the result in `ServerState`. On failure it logs, keeps the previous list, and
tries again on the next tick.

Each Monaco language ID from `src/languages.json` resolves to a Piston runtime
as follows:

1. If the override table has an entry, use that Piston language name.
   Initial table: `cpp` → `c++`, `shell` → `bash`. Add entries as gaps appear.
2. Otherwise, match the Monaco ID against each runtime's `language` and
   `aliases`.
3. If several versions match, take the newest by semver (`3.10` > `3.9`).

The resolved list (Monaco language, Piston runtime, version) is what clients
receive. When the list changes after a refresh, the server broadcasts it to
every open document.

### Configuration

- `main.rs` reads optional `PISTON_URL`, for example `http://piston:2000`.
- `ServerConfig` gains `piston: Option<PistonClient>`, `None` by default.
- When `PISTON_URL` is unset, the server sends no runtime message and ignores
  `Run`.

### Running code

`Rustpad` gains, in memory only:

- `running: Option<u64>`, the user ID of the run in progress.
- `last_run: Option<ServerMsg>`, the last `RunFinished` or `RunFailed`.

On `ClientMsg::Run` from user `id`:

1. If Piston is off, or a run is in progress, log and ignore.
2. Under the existing state lock, take a snapshot of the text and language.
3. If the language has no runtime, store and broadcast `RunFailed`
   ("No runtime for <language>"), then stop.
4. Set `running`, broadcast `RunStarted { id }`.
5. In a spawned task, call `execute`, then store and broadcast `RunFinished`
   or `RunFailed`, and clear `running`.

Results never go to SQLite and vanish when the document expires.

## Wire protocol

The new messages use the same externally tagged serde JSON as the existing
`ClientMsg` and `ServerMsg`.

Client → server:

```json
"Run"
```

Server → client:

```json
{ "Runtimes": [{ "language": "cpp", "runtime": "c++", "version": "10.2.0" }] }
{ "RunStarted": { "id": 3 } }
{ "RunFinished": {
    "id": 3, "language": "python", "runtime": "python", "version": "3.12.0",
    "compile": null,
    "run": { "stdout": "hi\n", "stderr": "", "code": 0, "signal": null, "truncated": false },
    "duration_ms": 412 } }
{ "RunFailed": { "id": 3, "error": "Code runner timed out" } }
```

- `Runtimes` comes on connect (in `send_initial`) and again whenever the list
  changes. It is never sent when Piston is off. The client hides the feature
  until it receives `Runtimes`.
- `compile` has the same shape as `run`, or is `null` for interpreted
  languages. `run` is `null` when compilation fails.
- `send_initial` replays `RunStarted` if a run is in progress, and
  `last_run` if one exists, so late joiners see the current state.
- A worst-case `RunFinished` is about 260 KiB, which fits warp's frame limit and
  the existing broadcast channel.

## Frontend

### `src/rustpad.ts`

- Types `Runtime`, `RunStage`, `RunResult` matching the protocol.
- Option callbacks `onRuntimes`, `onRunStarted`, `onRunFinished`,
  `onRunFailed`, following the `onChangeLanguage` pattern.
- `run()` sends `"Run"`.

### `src/App.tsx`

- `runtimes: Runtime[] | null`. `null` means the feature is off; a reconnect
  resets it to `null` until a new `Runtimes` arrives.
- `run: { status: "idle" | "running" | "finished" | "failed"; by?: number; result?: RunResult; error?: string }`.
- `Ctrl/Cmd+Enter` in Monaco triggers a run, through `editor.addCommand`.
- Renders `RunOutput` below the editor.

### `src/Sidebar.tsx`

- New props: `runtimes`, `runStatus`, `onRun`.
- Under the Language select: a full-width green Run button, shown only when
  `runtimes !== null`, with a spinner while running.
- When the current language has no runtime, the button is disabled and a hint
  line below reads "No runtime for <language>".
- The sidebar hides below the `sm` breakpoint (480px), so Run isn't available
  on phones.

### `src/RunOutput.tsx` (new)

- A panel below the editor, about 30% of the height, with its own scroll area
  and a close button. It opens on `RunStarted` or a replayed result, and
  reopens on each new run.
- Header: "Run by <name> · python 3.12.0 · 412 ms · exit 0", or the signal name
  (for example `SIGKILL`) instead of the exit code. The name falls back to
  "someone" when that user has left.
- Body, in the editor's monospace font: compile output (when the compile failed
  or printed anything), then stdout, then stderr in a red tint, then
  `[output truncated]` when needed. `RunFailed` shows its error here.
- Colours follow the dark and light values already used in `Sidebar.tsx`.
- Output renders only as React text children: no `dangerouslySetInnerHTML`, no
  ANSI-to-HTML, no auto-linking.

## Deployment and security

### Threat model

An invited user runs hostile code. They may try to escape the sandbox, reach
the internal network or cloud metadata, attack the Rustpad server, or attack
other users through the output. Layers of defence:

| Threat | Defence |
|---|---|
| Sandbox escape to other jobs | Piston `isolate`: per-job user, cgroup limits |
| Escape to the node | Dedicated node pool with no cloud permissions |
| Network access | Piston sandbox networking off, plus NetworkPolicy denying all egress |
| Abuse of Piston's package API | Rustpad never forwards to it; only Rustpad pods reach Piston |
| Resource exhaustion | Per-request limits, Piston ceilings, pod resource limits |
| Attacks through output | Size caps; inert text rendering |

### Local dev: `docker-compose/docker-compose.yml` (new)

- `rustpad`: builds from the repo's Dockerfile, `PISTON_URL=http://piston:2000`.
- `piston`: `ghcr.io/engineer-man/piston` pinned by digest, `privileged: true`
  (with a comment: `isolate` needs cgroups), port not published, packages on a
  named volume, tmpfs for `/piston/jobs` and `/tmp` as in the reference.
- Piston limits set explicitly in env: run and compile timeouts, output size,
  process count, memory, networking off. Verify variable names against the
  pinned Piston version during implementation.
- `docker-compose/install-runtimes.sh` installs the default set through the
  packages API from inside the compose network: Python, Node, TypeScript, Go,
  Rust, Java, GCC (C/C++), .NET or Mono (C#), Bash. Dev-only.

### Requirements for the Kubernetes setup (infra repo)

1. Dedicated node pool for Piston, tainted so only Piston schedules there.
   No cloud permissions on those nodes: no IRSA or Workload Identity; IMDSv2
   with hop limit 1 on EKS; metadata concealment on GKE.
2. Own namespace, the only one at PodSecurity level `privileged`. Pod spec:
   `privileged: true`, `automountServiceAccountToken: false`, no hostPath, CPU
   and memory limits on the pod.
3. NetworkPolicy: ingress on port 2000 only from Rustpad pods; all egress
   denied, DNS included.
4. Runtimes baked into a custom image built from the pinned Piston image, in
   the infra repo or CI.
5. ClusterIP Service only. No Ingress or LoadBalancer for Piston.

Possible later spike, not required: GKE Sandbox (gVisor) or Kata for the Piston
pool. It's unknown whether `isolate` works under gVisor.

## Testing

### Unit tests (`piston.rs`)

- Runtime resolution: override table, `language` match, `aliases` match,
  newest version by semver.
- Truncation at 64 KiB on a UTF-8 boundary; `truncated` flag.
- Error mapping for connection refused, timeout, non-2xx status, bad JSON and
  oversized body, each to a fixed user-facing message.

### Integration tests (`rustpad-server/tests/run.rs`, new)

They use a fake Piston: a warp server on `127.0.0.1:0` inside the test with
scripted responses. They need no network or Docker.

- Without `PISTON_URL`, the existing tests pass unchanged.
- `Runtimes` arrives on connect.
- Two clients, one sends `Run`: both get `RunStarted { id }` then
  `RunFinished`. The fake checks it got the snapshot text and our limits.
- A compile error returns with `compile` set.
- A second `Run` during a slow run is ignored; the fake sees one request.
- An unsupported language gives `RunFailed`.
- Piston 500, timeout and oversized output give `RunFailed` or a truncated
  result.
- A late joiner gets the replayed `RunStarted` or last result.

### Frontend

`npm run check` passes. No test runner is added.

### Manual and security checklist (compose stack)

- [ ] Hello world in each default language.
- [ ] C++ compile error shows compile output.
- [ ] `while True: pass` is killed at about 3 s.
- [ ] A fork bomb is contained.
- [ ] Allocating 2 GB hits the memory limit.
- [ ] Printing 10 MB is truncated.
- [ ] Printing `<img src=x onerror=alert(1)>` and ANSI escapes shows literal
      text.
- [ ] Opening a socket or running `curl` from user code fails.
- [ ] Two browsers see the same result; a late joiner sees the last output.
- [ ] Stopping the Piston container gives a clean error.
- [ ] Markdown shows a disabled Run button with the hint.

## Out of scope

- Rate limiting and quotas.
- stdin, command-line arguments, multiple files.
- Kubernetes manifests (infra repo).
- Saving run results.
- Running code on phones.
