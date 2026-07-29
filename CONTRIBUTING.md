# Contributing to mintPod

mintPod is deliberately narrow. Changes should make the launch-to-agent path safer, clearer, or easier to maintain without introducing accounts, telemetry, chat UI, cloud sync, free-form GPU selection, or another configuration layer.

## Before changing code

Install the prerequisites and run the development checks from the [README](README.md#development). Keep orchestration in Rust. Svelte should display backend state and submit explicit user intent; it should not call RunPod, edit harness files, calculate stop conditions, or invent progress.

Keep patches small enough to review. Commit messages use the imperative mood and describe one complete scope.

## Add or update a curated preset

Curated presets are source-controlled product decisions, not a mirror of the Ollama library. A candidate should be useful for coding-agent work, fit the default hobby range, and have a GPU fallback list that has been exercised on RunPod.

1. Pull the exact `ollamaTag` with a current Ollama release.
2. Record the downloaded weight size shown by `ollama list`; do not estimate from parameter count.
3. Measure minimum practical VRAM with the configured context, not just the theoretical weight size.
4. Copy exact RunPod GPU type IDs and rank them from preferred to last fallback.
5. Record a realistic observed hourly rate for the preferred tier.
6. Add one file under `presets/` and add its `include_str!` entry to `CURATED` in `src-tauri/src/presets.rs`.
7. Run the validation and build checks below.

Use this shape:

```json
{
  "id": "model-slug",
  "label": "Human model name",
  "ollamaTag": "namespace/model:tag",
  "sizeGb": 6.4,
  "minVramGb": 12,
  "gpuTypeIds": [
    "NVIDIA GeForce RTX 4090",
    "NVIDIA RTX A5000"
  ],
  "estCostPerHr": 0.34,
  "tags": ["coding"]
}
```

The schema rejects unknown fields. IDs use lowercase kebab case. Tags use lowercase kebab case. Curated model files over 16 GB are rejected at startup; larger personal presets belong in `presets.user.json`.

## Verify a preset

Run:

```sh
cargo test --manifest-path src-tauri/Cargo.toml --no-default-features --lib
npm run check
npm run build
```

Then perform one real RunPod verification:

- The first launch selects one of the ranked GPUs.
- The pod mounts the expected Network Volume at `/root/.ollama`.
- Pulling reports byte progress from Ollama rather than a timer.
- The model answers an OpenAI-compatible request through `127.0.0.1:8080`.
- `pi`, followed by `/models`, lists the `mintpod` provider and the exact Ollama tag.
- A stop releases the GPU without deleting the Network Volume.
- A second launch reports the model as cached and reaches warm state without pulling it again.
- Idle timeout and the selected budget both stop the pod when tested with short safe values.

Include the RunPod GPU type used, observed rate, pull size, and first/second launch result in the pull request. Do not include API keys, proxy tokens, pod IDs, or the contents of Pi credential files.

## Change a GPU tier

GPU tiers live in `src-tauri/src/presets.rs`. They are ranked lists. Reordering them changes provisioning behavior for every custom preset using that tier, so verify availability, VRAM, and rate before changing the order. Do not add a free-form GPU picker to the UI.

## Pull request checklist

- The change stays within mintPod's stated scope.
- New JSON persists atomically and malformed user data fails with a useful error.
- The RunPod key never leaves the OS keychain.
- Existing Pi providers survive a wiring update unchanged.
- Rust tests, Svelte checks, and the production frontend build pass.
- Platform-specific behavior is either tested on that platform or called out explicitly.
