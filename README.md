# mintPod

mintPod is a small desktop control panel for running a coding model on RunPod without turning pod lifecycle management into a second job. Pick a curated model, set a hard time or EUR budget, and launch. mintPod provisions the GPU, keeps the model on a persistent Network Volume, waits for Ollama to become genuinely ready, and exposes the model to Pi through an authenticated loopback proxy.

## What it does

- Creates a RunPod pod from a preset-owned, ranked GPU list.
- Mounts a persistent Network Volume at `/root/.ollama`.
- Pulls the model once, reports real byte progress, and reuses the cached weights later.
- Keeps the selected model resident with `OLLAMA_KEEP_ALIVE=-1`.
- Serves the pod locally at `http://127.0.0.1:8080` behind a random bearer token.
- Merges the active model into Pi's configuration without replacing other providers.
- Keeps named RunPod key profiles in the OS keychain and lets you switch the active account.
- Stops on the selected budget or on real proxy inactivity, then terminates the pod after a five-minute grace period.
- Keeps the RunPod API key in the operating system keychain. There is no telemetry, account layer, or cloud sync.

## Requirements

- A RunPod account with billing enabled
- A RunPod API key allowed to manage pods and Network Volumes
- Rust 1.88 or newer
- Node.js 20.19 or newer
- Tauri 2 system dependencies for the target platform

On Debian or Ubuntu, install the native dependencies before building:

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

See the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for macOS, Windows, and other Linux distributions.

## Setup

```sh
git clone <your-fork-or-clone-url>
cd mintpod
npm ci
cargo install tauri-cli --version "^2" --locked
cargo tauri dev
```

On first launch:

1. Create or copy an API key from the RunPod console.
2. Give the key a local name and paste it into mintPod.
3. Select the Network Volume region closest to the GPU region you intend to use.
4. Save the key.

The key is validated against RunPod before it is stored. mintPod uses macOS Keychain, Windows Credential Manager, or the Linux Secret Service through the Rust `keyring` crate. It is never written to a JSON file. Add, replace, remove, or switch named keys under **Manage**; the compact selector on the launch screen shows which profile is active.

## Launch and verify Pi

Select a preset, choose either a time budget or a cost budget, then press **Launch**. “Ready” means all of the following have completed: the pod reports `RUNNING`, Ollama answers its health endpoint, the model exists or has finished pulling, the model is loaded into VRAM, the local proxy is listening, and Pi has been updated.

To verify the wiring from Pi:

```text
$ pi
> /models
```

Choose the `mintpod` provider and the model shown in mintPod. The running screen also copies a direct command:

```sh
pi --provider mintpod --model qwen2.5-coder:7b
```

mintPod merges the provider into `~/.pi/agent/models.json`, which current Pi releases read, and maintains the requested `~/.pi/agent/local-models.json` endpoint contract for compatibility. Existing providers and unknown fields are preserved. Both files contain only the short-lived local proxy token, never the RunPod key.

## Storage and cost behavior

Each preset gets its own Network Volume named `mintpod-<preset-id>`. Volumes created by versions released under the previous name remain compatible and are reused automatically. Stopping or terminating a pod leaves that volume intact, which is why the next launch skips the model download. Network Volumes can continue to incur storage charges while no GPU is running; use **Manage models** to delete a cache you no longer want.

RunPod reports the live hourly GPU rate. mintPod resyncs it every 30 seconds and converts USD to EUR using the ECB daily reference rate. If the ECB is unavailable and no cached rate exists, it uses a conservative 1:1 conversion so a EUR cost limit stops early rather than late.

An automatic stop releases the GPU immediately. The stopped pod remains resumable for five minutes, then mintPod terminates it. The Network Volume is not touched. Closing mintPod during a launch or session first completes the safe stop/termination path.

## Add a model

Curated presets live in [`presets/`](presets/) and must validate against [`presets/schema.json`](presets/schema.json). Add one JSON file per model:

```json
{
  "id": "coder-8b",
  "label": "Qwen2.5-Coder 8B",
  "ollamaTag": "qwen2.5-coder:8b",
  "sizeGb": 5.2,
  "minVramGb": 8,
  "gpuTypeIds": [
    "NVIDIA RTX 4090",
    "NVIDIA RTX A5000",
    "NVIDIA RTX 3090"
  ],
  "estCostPerHr": 0.34,
  "tags": ["coding", "recommended"]
}
```

Curated model files must be 16 GB or smaller. GPU IDs are ordered fallback choices, not suggestions; use exact RunPod type IDs and put the preferred option first. Update the embedded `CURATED` list in `src-tauri/src/presets.rs`, then follow the verification checklist in [CONTRIBUTING.md](CONTRIBUTING.md).

Personal presets should be added through **Manage models**. They are written to the application config directory as `presets.user.json`, separate from the shipped catalog. Models above 16 GB are allowed after a soft warning.

## Development

```sh
npm run check
npm run build
cargo test --manifest-path src-tauri/Cargo.toml --no-default-features --lib
cargo tauri build
```

The pure Rust suite does not require a desktop webview. A complete Tauri build must run on the target operating system with its native prerequisites installed. Release bundles are written below `src-tauri/target/release/bundle/`.

The Rust core owns RunPod calls, polling, volume lifecycle, cost enforcement, the local proxy, file writes, and harness integration. Svelte renders state and sends user intent; it does not orchestrate infrastructure.

## License

[MIT](LICENSE)
