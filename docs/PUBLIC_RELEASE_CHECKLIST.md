# Public release checklist

This is the practical maintainer checklist for publishing mintPod to a small group. It intentionally avoids a heavyweight release process.

## Before pushing `main`

- Confirm `git status` contains only intended changes and inspect every commit that is ahead of `origin/main`.
- Search the tracked tree for API keys, bearer tokens, pod IDs, private config, environment files, and local paths. Never paste live diagnostics into the repository.
- Run the local verification commands from [CONTRIBUTING.md](../CONTRIBUTING.md). If Go is unavailable locally, require the **Runtime image / test** GitHub Actions job to pass.
- Confirm the runtime reference in `src-tauri/src/runpod.rs` is an immutable digest and can be pulled without registry authentication.
- Confirm the RunPod console has no unintended mintPod pods. Keep only the Network Volumes you intentionally retain.
- Review the **Unreleased** section of [CHANGELOG.md](../CHANGELOG.md).

## One-time GitHub repository settings

- Set the description to something concrete, for example: “Launch private coding models on RunPod and connect them to local coding agents.”
- Add a small set of discoverable topics: `tauri`, `svelte`, `rust`, `runpod`, `ollama`, `coding-agents`, and `openai-compatible`.
- Keep Issues enabled; the repository contains forms for bug reports and small feature requests.
- Enable GitHub private vulnerability reporting and keep security reports out of public issues.
- Make the `ghcr.io/<owner>/mintpod-runtime` package public. A public source repository does not automatically make its GHCR package public.
- Keep Actions enabled with read access to repository contents. The runtime workflow needs package-write permission only for publishing its image.
- Optional for a one-maintainer hobby project: protect `main` by requiring the cross-platform **CI / build** check before merging outside contributions.

## Versioned release

Publishing the repository does not require creating a GitHub Release. Until installers are tested and signed, it is clearer to call the project “source available as a hobby pre-release” than to imply a polished desktop distribution.

When creating the next release:

1. Choose a new version; do not move or reuse an existing tag.
2. Update the matching version in `package.json`, `package-lock.json`, `src-tauri/Cargo.toml`, `src-tauri/Cargo.lock`, and `src-tauri/tauri.conf.json`.
3. Publish and test the runtime image first, then compile the desktop app against the reported immutable digest.
4. Move the changelog entries from **Unreleased** into the new version and date.
5. Run the full local and GitHub Actions checks.
6. If curated profiles are promoted from **Candidate**, complete and record the paid matrix in [PAID_CONTRACT_TESTS.md](PAID_CONTRACT_TESTS.md).
7. Create an annotated `vX.Y.Z` tag and a GitHub Release whose notes link to the changelog and clearly state which platforms were manually exercised.
8. Attach desktop bundles only after installing and launching those exact artifacts on their target OS. State plainly when an artifact is unsigned.

## Public smoke-test flow

- A clean checkout follows the README without undocumented steps.
- First start explains keychain or port failures instead of exiting silently.
- The RunPod key validates and remains named in the key selector.
- **Check GPU availability** shows names, stock, VRAM, live prices, and rejected reasons.
- Launch either reaches **Ready** or returns a useful placement/capacity error and cleans up the pod.
- At least one OpenAI-compatible request streams through `127.0.0.1:11435`.
- Manual end removes the pod and owned Pi/OpenCode entries while retaining the model volume.
- Restart shows no recovery record after normal cleanup.
