# SDK/runtime compatibility gate

This gate tests the SDK/runtime boundary, not source-level API compatibility. It is called by `check.yml` for code-changing PRs, and `Check Summary` requires it to finish successfully. It publishes no packages and changes no release automation.

## Matrix

The support window is the candidate's minor line and its immediately previous minor line: 0.N.x against 0.N.x and 0.(N-1).x, in both directions. The baseline job reads the candidate version from `Cargo.toml`, not the globally latest release. The PR gate samples the two newest stable patches in the current line and the newest stable patch in the previous line. Each tag, peeled commit, runtime/firmware hash and Go FFI hash is recorded once. Drafts, prereleases and SDK-specific tags are excluded. There is no permanent historical baseline.

On a minor-bump PR, the current line may have no published release yet. Candidate/candidate controls cover that new line, alongside the newest published patch of the previous line; the gate never falls back to a release two minor lines behind. A missing previous-line release fails selection. Major-version policy requires a deliberate update rather than silently reusing the 0.N.x rule. `policy.json` records the candidate version and selected sample.

| SDK | Runtime | Purpose |
| --- | --- | --- |
| Candidate | Released | New SDK with independently installed older `msb` |
| Released | Candidate | Existing application with updated `msb` |
| Released | Released | Baseline control |
| Candidate | Candidate | Candidate control using the same scenarios |

For each pinned baseline, each pairing runs twice: explicit `MSB_PATH` with a fresh home, then installed-runtime discovery without `MSB_PATH`/`MSB_LIBKRUNFW_PATH` in a home initialized by the released CLI. That second lane starts with an empty released catalog. Existing catalog schemas and migration histories must remain unchanged after SDK access. The CLI inspects SDK-created records. Bundled-runtime substitution is a failure, not evidence for the requested pairing.

Rust, Python, Node and Go run lifecycle, environment, multiple mount cardinalities, persistent disk restart, network isolation and disk snapshot restore scenarios. Rust, Python and Node also exercise archive restore. Node is tested with Node.js, not Bun. Ruby runs its exposed lifecycle/filesystem and disabled-network operations; custom mounts and snapshot restore are not exposed by its current public API and are reported as not applicable rather than substituted with CLI calls.

For selected pre-0.7 baselines, Rust/Python/Node/Go use a common public-API fixture covering create, 0/1/3 tmpfs mounts, guest environment, persistent disk writes, stop/start, removal and actual runtime identity. This lane does not claim network-policy or snapshot-restore coverage: those SDKs predate the dedicated restore API. Rust uses the release's `net,prebuilt` features and published Agentd, not the newer `local` feature or candidate guest payload. Modern baselines retain the wider existing suite.

As of this change, RubyGems has only the unrelated `0.1.0` SDK. Candidate Ruby is tested against both runtimes, but released-Ruby coverage is explicitly unavailable. Baselines preceding the first modern gem retain that exception. At or after that first modern version, a missing matching gem fails provisioning instead of silently dropping the reverse lane.

## Upgrade transitions

Each baseline also seeds populated homes with its real CLI, including a persistent disk marker, environment and tmpfs mount:

- **Already-running old VM:** retain one old VM, atomically replace the installed binary/firmware, and use the candidate CLI to list, execute, stop and start sandboxes. Starting another stopped sandbox must work without replacing the old VM or changing its catalog schema underneath it. A stop-all migration refusal fails this workflow; diagnostic commands and stop/retry recovery are still exercised afterward. This PR does not weaken any runtime or migration guard.
- **After CLI upgrade, candidate SDK:** let the candidate CLI prepare the catalog and start/stop the retained sandbox, then use the candidate SDK to restart it twice and verify disk data, environment, mount and runtime identity.
- **After CLI upgrade, released SDK:** repeat with the published SDK. Refusing a newer catalog is a compatibility failure, not a pass merely because it avoids corruption. SDK access must preserve the CLI-prepared schema/history.

When both releases share a schema, CLI preparation may be a no-op; otherwise the same transition exercises migration. Mixed-version process checks permit only the specifically named old/new executables, not arbitrary binaries in the fixture home.

Bump PRs are included: manifest/lockfile changes are code changes, and candidate artifacts are built after the bump. Automatically generated bump PRs may require workflow approval. This PR does not change release creation or publication automation.

## Avoiding false passes

- Published Python wheels, npm packages/platform bindings, Go modules/FFI, Rust crates and Ruby gems stay separate from candidate artifacts. Version strings alone do not establish provenance when development and published versions match.
- Native bindings are identified and hashed. The Rust dependency graph and Go module resolution are recorded. The runner verifies its SDK artifact inventory before testing.
- After each VM launch, a helper checks `/proc` for the fixture's exact `MSB_HOME` and requested sandbox name, and verifies the actual runtime executable hash. Selecting an environment variable without observing the launched runtime is insufficient.
- Default/deny-all network checks use a local positive control where the SDK exposes one. Ruby checks that its disabled-network contract exposes no external interface.
- Missing KVM, required packages, reports, successful cases or runtime evidence are failures, not skips. The matrix continues after a case fails so other pairings still produce evidence.
- Homes are short, isolated temporary directories. Cleanup addresses catalog names only within those homes, preserves primary failures when teardown also fails, and never signals an unverified historical PID. Failed fixture data is retained for diagnosis; hosted runners are discarded after the job.

## Running and inspecting it

The live jobs use fresh GitHub-hosted Ubuntu 24.04 x86-64 runners. They require and probe `/dev/kvm`; they do not fall back to emulation or a persistent self-hosted runner. Compilation/package preparation happens separately and reuses candidate Python, Node and Go CI artifacts. Ruby and the standalone Rust fixture are built from the candidate checkout.

Run the infrastructure checks without VMs or network access:

```sh
python3 -W error -m unittest discover -s scripts/compatibility -p 'test_*.py'
```

The reusable workflow shows the exact provisioning and live commands. Each baseline/language uploads `compatibility-results-<tag>-<language>` with `results.json`, SDK reports, runtime identities, per-cell durations, and setup/test/cleanup logs. A successful candidate-Ruby lane is not a released-Ruby pass.

## Boundaries

This is a sampled, two-minor-line Linux x86-64 gate, with up to three published baselines. It does not claim every patch in the support window, macOS/HVF, Windows/WHP, Linux ARM64, full-memory/branch snapshots, arbitrary cross-version archives or exhaustive crash recovery. Separate fixed catalog regression fixtures are not the SDK/runtime compatibility policy and are unchanged here. Additional patch coverage within the rolling window remains follow-up work, not implemented coverage.

For a 0.7.x candidate with releases through 0.7.1 and 0.6.18, selection is 0.7.1, 0.7.0 and 0.6.18. A later 0.6.19 replaces 0.6.18. A 0.8.0 bump with no published 0.8.x selects only 0.7.1 as its published baseline; once 0.8.0 and 0.8.1 ship, a 0.8.x candidate selects those two plus the latest 0.7.x patch. Package-publication availability remains fail-closed: no fallback silently replaces a baseline whose package is unavailable.
