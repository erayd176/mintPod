# Contributing to mintPod

mintPod is deliberately narrow. Changes should make the path from “choose a model” to “use it from a local coding agent” safer, clearer, or more reliable. Accounts, telemetry, chat UI, general pod administration, multiple concurrent sessions, and unrelated workload types are outside the `0.1.x` scope.

## Architecture boundaries

- Rust owns RunPod calls, durable ownership, cleanup, budgets, the local gateway, keychain access, atomic file writes, and coding-tool integration.
- Svelte renders backend state and submits explicit user intent. It must not calculate stop conditions, mutate tool files, or call RunPod directly.
- `runtime/` is the only public pod service. Raw Ollama must remain bound to pod loopback.
- npm and `package-lock.json` are the canonical frontend package-manager state.

Keep patches small enough to review. Commit messages use the imperative mood and cover one complete scope.

## Add or update a curated profile

Curated profiles are compatibility contracts, not a mirror of the Ollama library.

1. Pull the exact `ollamaTag` with the Ollama version pinned by `runtime/Dockerfile`.
2. Record the downloaded weight size from `ollama list`.
3. Choose a context of at least 64,000 tokens and measure practical VRAM at that context.
4. Copy exact RunPod GPU type IDs and rank the acceptable fallbacks.
5. Record a conservative expected hourly rate in USD, matching RunPod's own quotes.
6. Add one JSON file under `presets/` and its `include_str!` entry to `CURATED` in `src-tauri/src/presets.rs`.
7. Start with `"verification": "candidate"`.
8. Run all local checks and the paid matrix before changing the status to `manuallyTested`.

Example:

```json
{
  "id": "model-slug",
  "label": "Human model name",
  "ollamaTag": "namespace/model:tag",
  "sizeGb": 15.0,
  "minVramGb": 48,
  "gpuTypeIds": [
    "NVIDIA A40",
    "NVIDIA RTX A6000"
  ],
  "estCostPerHr": 0.55,
  "tags": ["agentic", "coding", "tools"],
  "contextLength": 65536,
  "maxOutputTokens": 16384,
  "verification": "candidate"
}
```

The schema rejects unknown fields. IDs and tags use lowercase kebab case. Curated profiles over 20 GB or below 64K context are rejected at startup. Larger personal models belong in `presets.user.json`.

## Local verification

Run from the repository root:

```sh
npm ci
npm run check
npm run build
go test -C runtime ./...
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

The default Rust feature set is intentional: integration and command tests must compile with the same desktop runtime used in release builds.

## Live verification

The ignored live contract checks only authenticated REST and GraphQL reads and creates no resources:

```sh
MINTPOD_LIVE_RUNPOD_TESTS=1 \
RUNPOD_API_KEY='...' \
cargo test --manifest-path src-tauri/Cargo.toml live_runpod_read_contract -- --ignored
```

A profile is not `manuallyTested` until it passes [the paid contract matrix](docs/PAID_CONTRACT_TESTS.md). Record:

- operating system and mintPod commit;
- exact Ollama tag and runtime image;
- requested and allocated GPU;
- data center and observed hourly rate;
- loaded context from the app verification;
- first-launch pull and second-launch cache result;
- Pi, OpenCode, and Aider outcomes;
- cancellation, recovery, idle, budget, and final cleanup outcomes.

Never post API keys, gateway tokens, pod IDs, machine IDs, raw diagnostics containing user-added data, or tool configuration contents.

## Integration rules

- Pi and OpenCode writes must be atomic and own only the `mintpod` entry.
- Existing providers and unknown fields must survive publish and unpublish unchanged.
- Invalid existing JSON must produce a useful error and remain byte-for-byte unchanged.
- Aider remains command-only unless there is a separate design decision for reversible ownership.
- A missing or broken tool integration must not keep the paid GPU from becoming usable.
- All mintPod-owned entries are removed after normal end or recovery cleanup.

## Pull request checklist

- The change stays inside the current product scope.
- Paid mutations have durable ownership before or immediately after the remote mutation.
- Failure and cancellation paths have compensating cleanup.
- Secrets do not enter logs, settings, diagnostics, pod names, or source-controlled fixtures.
- JSON persistence is atomic and malformed user data fails safely.
- Rust tests, runtime tests, Svelte checks, linting, and the production frontend build pass.
- Platform-specific behavior was tested on that platform or called out explicitly.
