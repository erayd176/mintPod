# mintPod

Launch a private coding model on RunPod, use it from your local agent tools, and end the paid GPU when the session is idle or reaches its budget.

mintPod is a focused Tauri desktop app for developers who want a larger open model than their machine can run without repeatedly rebuilding RunPod pods, copying endpoint URLs, editing tool configuration, or wondering whether a GPU was left billing.

> **Release status:** pre-release `0.1.0`. The lifecycle and integration paths are covered by local tests. The three shipped model/GPU profiles are deliberately marked **Candidate** until each passes the paid contract matrix on real RunPod capacity.

## The workflow

1. Store a named RunPod API key in the operating-system keychain.
2. Pick a coding model, a time or EUR budget, and the maximum GPU rate you accept.
3. Optionally check current Secure Cloud price and stock before launch.
4. mintPod creates or reuses the model's Network Volume, provisions one GPU, pulls and warms the model, and verifies the requested context window.
5. Pi and OpenCode receive a temporary `mintpod` provider; Aider gets a ready-to-copy command.
6. Use the model through one stable local OpenAI-compatible endpoint.
7. End the session, hit the budget, or become idle. mintPod terminates the pod and removes its tool entries while retaining the model cache.

## Why it is useful

- **One stable local endpoint:** `http://127.0.0.1:11435` stays constant while the remote pod and per-session credential change.
- **Private remote runtime:** the public RunPod proxy exposes mintPod's authenticated gateway, not raw Ollama. Requests without the exact per-session bearer token are rejected.
- **Spend controls start with billing:** the selected budget starts when the pod is created, including boot, download, and warm-up time.
- **No silent price upgrade:** live global inventory is filtered by VRAM, Secure Cloud availability, and your maximum hourly rate. The allocated rate is checked again before model download.
- **Crash-aware ownership:** pod and volume ownership is journaled atomically. On the next start, mintPod offers reconnect, retry, or explicit cleanup before allowing another launch.
- **Persistent downloads:** each profile uses its own Network Volume, so later sessions can reuse model weights.
- **Non-destructive integrations:** mintPod owns only `providers.mintpod` in Pi and `provider.mintpod` in OpenCode. Other configuration is preserved, malformed JSON is never overwritten, and Aider's global config is not changed.

This is intentionally not a general RunPod console, chat application, multi-model router, vLLM manager, or ComfyUI launcher. The first release makes one ephemeral coding-model session dependable.

## Supported tools

| Tool | Behavior |
| --- | --- |
| [Pi](https://github.com/badlogic/pi-mono) | Atomically adds the active model to `~/.pi/agent/models.json` when the `pi` binary is installed. |
| [OpenCode](https://opencode.ai/docs/providers/#custom) | Atomically adds an OpenAI-compatible provider to the platform config directory's `opencode/opencode.json`. |
| [Aider](https://aider.chat/docs/llms/openai-compat.html) | Produces an OS-appropriate command with `OPENAI_API_BASE`, `OPENAI_API_KEY`, and `openai/<model>`; no config file is modified. |
| Other clients | Use **Copy OpenAI-compatible config** during a session. The credential is revealed only by that explicit action and is never included in diagnostics. |

Integrations can be disabled independently under **Manage**. A missing tool is reported as “Not installed” and never prevents the GPU session from becoming ready.

## Shipped model profiles

| Profile | Ollama tag | Weights | Required VRAM | Context | Status |
| --- | --- | ---: | ---: | ---: | --- |
| gpt-oss 20B | `gpt-oss:20b` | 14 GB | 24 GB | 65,536 | Candidate |
| Qwen3-Coder 30B | `qwen3-coder:30b` | 19 GB | 48 GB | 65,536 | Candidate |
| Devstral Small 2 24B | `devstral-small-2:24b` | 15 GB | 48 GB | 65,536 | Candidate |

The catalog is small on purpose. A profile is a tested product contract—exact model tag, ordered RunPod GPU IDs, minimum VRAM, context, output limit, storage size, and expected rate—not merely a link to an Ollama tag. Ollama recommends at least 64K context for coding agents; mintPod sets it explicitly and verifies the loaded value through `/api/ps`.

Personal Ollama presets can be added under **Manage**. They are kept in `presets.user.json` outside the source-controlled catalog and remain candidates for the user's own validation.

## Security and lifecycle guarantees

- RunPod API keys, the stable local gateway token, and per-session remote tokens use the OS keychain.
- The RunPod API key is never placed in the pod, local JSON settings, harness config, diagnostics, or logs by mintPod.
- The pod publishes only the authenticated mintPod runtime on port `8000/http`; Ollama listens on pod loopback.
- The desktop gateway requires its own bearer token and translates it to the current remote token.
- Launch cancellation and normal window close wait for compensating pod cleanup. Termination is retried and a failed cleanup remains in the recovery journal.
- Network Volumes are not deleted when a session ends. They can continue to incur storage charges until removed under **Manage** or in RunPod.

Important limit: time, cost, and idle enforcement run in the desktop process. A process crash is reconciled when mintPod restarts, but a powered-off or disconnected computer cannot guarantee remote termination. Always keep a RunPod console fallback and verify there are no running pods after a machine or network failure. See [SECURITY.md](SECURITY.md) for the threat model.

<img width="426" height="556" alt="image" src="https://github.com/user-attachments/assets/df46d313-19b5-48b2-a696-76543479ac87" />


## Requirements

For end users:

- A RunPod account with billing enabled
- A RunPod API key allowed to read GPU inventory and manage pods and Network Volumes
- At least one supported coding tool, or another OpenAI-compatible client

For source builds:

- Rust 1.88 or newer
- Node.js 20.19 or newer
- Tauri 2 system dependencies for the target platform

On Debian or Ubuntu:

```sh
sudo apt update
sudo apt install \
  libwebkit2gtk-4.1-dev \
  build-essential \
  curl \
  wget \
  file \
  libxdo-dev \
  libssl-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev
```

See the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for other operating systems.

## Build and run

```sh
git clone https://github.com/erayd176/mintPod.git
cd mintPod
npm ci
npm run tauri -- dev
```

The app expects the pinned `ghcr.io/erayd176/mintpod-runtime:0.1.0` image. A release tag must publish that runtime image before its desktop installers are distributed.

## Cost and placement behavior

Each profile gets a Network Volume named `mintpod-<profile-id>` in the selected data center. Volumes created under the earlier `podpilot-` name remain discoverable. The volume data center constrains final GPU placement.

The preflight query reports RunPod's **global** Secure Cloud inventory; it is not data-center scoped. A passing preflight is useful but not a reservation. The REST pod creation request remains the authoritative placement step and can still fail if capacity disappears.

mintPod uses RunPod's allocated hourly rate for telemetry and refreshes it every 30 seconds. USD is converted with the ECB daily reference rate; if neither the ECB nor its local cache is available, the conservative fallback is 1 USD = 1 EUR. Ending a session terminates the pod rather than leaving a stopped pod behind.

## Development

```sh
npm ci
npm run check
npm run build
go test -C runtime ./...
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
npm run tauri -- build
```

The ignored live API contract is read-only and requires explicit opt-in:

```sh
MINTPOD_LIVE_RUNPOD_TESTS=1 \
RUNPOD_API_KEY='...' \
cargo test --manifest-path src-tauri/Cargo.toml live_runpod_read_contract -- --ignored
```

Do not run paid infrastructure checks casually. The complete acceptance matrix and cleanup rules are in [docs/PAID_CONTRACT_TESTS.md](docs/PAID_CONTRACT_TESTS.md).

The Rust core owns RunPod calls, lifecycle, persistence, cost enforcement, the local gateway, and tool integration. Svelte displays backend state and submits user intent; it does not orchestrate infrastructure.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md), especially the paid profile verification requirements. Security reports belong in [private vulnerability reporting](https://github.com/erayd176/mintPod/security/advisories/new), not a public issue.

## License

[MIT](LICENSE)
