# Spikes

Before the app was written, four small experiments ("spikes") checked whether its key promises could be kept on
Windows, and what they would cost. The design in [ARCHITECTURE.md](../ARCHITECTURE.md) follows from what they measured.
They are kept so the measurements can be repeated, for example on another machine or another Windows version.

None of this is part of the app. The Rust spikes are in `examples\` and build only with `--features spikes`. Raw results
go to `spikes\out\`, which git ignores.

All of them work in their own folders under `%TEMP%` and only read from GitHub. Run them from a normal (not elevated)
PowerShell.

| Spike | Question | Files |
| --- | --- | --- |
| S1a | Which NTFS change-tracking features can a standard user use? | `s1-usn-permissions.ps1` |
| S1b | What does a per-file fingerprint cost in Rust, and when does it miss a change? | `..\examples\s1b_fingerprint.rs` |
| S2 | How does GitHub answer conditional polls, with and without a token? | `s2-github-probe.ps1`, `s2b-304-cost.ps1`, `s2c-observe.ps1` |
| S3 | What does a native Rust tray app cost to start, keep idle and poll? | `..\examples\s3_tray.rs`, `..\examples\s3_floor.rs`, `s3-build-variants.ps1`, `s3-measure.ps1` |

## Running them

```powershell
# S1a: standard-user access to the change journal and per-file USNs
.\spikes\s1-usn-permissions.ps1 -Volume C

# S1b: fingerprint cost and behaviour (everything under %TEMP%\tss-s1b)
cargo run --release --features spikes --example s1b_fingerprint -- corpus medium
cargo run --release --features spikes --example s1b_fingerprint -- bench medium
cargo run --release --features spikes --example s1b_fingerprint -- race
cargo run --release --features spikes --example s1b_fingerprint -- cleanup

# S2: GitHub polling (read-only; anonymous requests count against 60 per hour per IP)
.\spikes\s2-github-probe.ps1 -Owner anthropics -Repo skills -Branch main -Path skills

# S3: build the tray spike in each variant, then measure it
.\spikes\s3-build-variants.ps1
.\spikes\s3-measure.ps1
```

Each script's header lists its options. `s2-github-probe.ps1 -Auth token`, `s2b-304-cost.ps1` and `s2c-observe.ps1` use
the token from `gh auth token` and keep it in memory only.

## What they found

**S1a and S1b: finding edits made while the app was not running.**

- Size and last-write time alone missed 4 of 6 kinds of edit (an edit with the old time put back, a move over the file,
  a replace, a copy). The NTFS change time and the file's USN caught all of them.
- In 4,800 same-size edits made 0 to 32 ms after the fingerprint was taken, the USN never missed one; the time stamps
  alone did (up to 88% left the change time unchanged).
- Listing a folder is not evidence that a file is unchanged: while a writer held a file open, the listing kept reporting
  the old size and times. Each file must be asked through its own handle.
- A write through a memory-mapped view moves nothing until it is flushed. Hence the full check before every update
  or delete.
- A standard user cannot read the change journal, and it wraps within minutes on a busy disk, so it cannot be the
  history. Comparing each file's own USN with the stored one does not need the history.
- A folder watcher overflows on bursts and knows nothing about the time the app was stopped. It can be a hint, never
  the source of truth.
- Cost: about 45 µs per file to fingerprint (mostly opening the file), at 84, 2,000 and 20,000 files.

**S2: polling GitHub.**

- One conditional request on `git/trees/BRANCH:FOLDER` is the whole steady-state poll. Its ETag follows the folder's
  content, so commits elsewhere in the repository still get `304 Not Modified`.
- Anonymous requests share 60 per hour per IP, even when the answer is 304; with a token they cost nothing. Downloads
  from `raw.githubusercontent.com` are not counted.
- ETags depend on the token and on request headers, so the app stores the ETag from the 200 answer and sends it back
  exactly.
- A 404 can mean "gone" or "no access", so it can only ever mean "unknown".
- GitHub accepts links, submodules, case clashes, reserved Windows names and very long paths in a repository, so the
  client must check every path itself.
- Running `gh` for every poll would cost about 0.6 s; `gh auth token` costs 37 ms. So `gh` is only a token source.

**S3: the cost of a native tray app.**

- Warm start to the tray icon took 12 to 16 ms, almost all of it Windows (process creation, loading DLLs, and the round
  trip to Explorer); the app's own work was about 1.4 ms.
- Idle for 60 s: 0 CPU, 0 I/O operations, 0 page faults, 0 context switches.
- An unchanged poll over WinHTTP: 16 to 31 ms CPU and no file I/O.
- A native settings window opened in 12 to 22 ms and did not leak.
- A static C runtime is as fast as the dynamic one and removes the Visual C++ redistributable.
- The first start of a new, unsigned exe costs 0.5 to 1 s for the Defender scan, whatever the exe does.

These numbers became the budgets the real app is measured against (see
[CONTRIBUTING.md](../CONTRIBUTING.md#measuring)); the real app's results are in
[README.md](../README.md#measured-cost).
