# Security policy

## Reporting a vulnerability

Please use [GitHub private vulnerability reporting](https://github.com/erayd176/mintPod/security/advisories/new). Do not open a public issue containing credentials, pod URLs, resource identifiers, configuration contents, or an exploitable bypass.

This project is pre-release. Security fixes target the current `main` branch until signed public releases exist.

## Threat model

RunPod's HTTP proxy is publicly reachable. A pod ID or proxy URL is not treated as a secret or an authentication boundary.

mintPod therefore uses two independent credentials:

1. A stable local bearer token protects `127.0.0.1:11435` and is written only where enabled local tools require it.
2. A fresh per-session bearer token protects port `8000` on the remote runtime. The local gateway translates the local token to this remote token.

Raw Ollama listens only on `127.0.0.1:11434` inside the pod. The remote gateway removes its Authorization header before forwarding to Ollama.

## Runtime image trust

Official builds send RunPod an immutable `ghcr.io/erayd176/mintpod-runtime@sha256:...` reference. The digest is recorded in `src-tauri/src/runpod.rs`; the corresponding gateway source, Dockerfile, and publishing workflow are part of this repository. The Dockerfile pins its Go builder and Ollama base images by digest as well.

Forks can compile in their own runtime with the `MINTPOD_RUNTIME_IMAGE` build environment variable. The runtime workflow publishes to the current repository owner's GHCR namespace and reports the resulting immutable reference in its workflow summary.

This override is deliberately build-time only. A runtime image is trusted code inside the pod: it can see inference traffic, access the attached model volume, and make outbound network requests. Do not build mintPod against an image you do not trust, and prefer `image@sha256:<digest>` over any mutable tag.

## Credential storage

- RunPod API keys, the local gateway token, and remote session tokens use macOS Keychain, Windows Credential Manager, or Linux Secret Service through `keyring`.
- RunPod API keys are not sent to the pod and are not written to mintPod JSON files or coding-tool configuration.
- Pi and OpenCode configuration necessarily contain the local gateway token so those local processes can authenticate. A process or account that can read those user-owned files can use the active local endpoint.
- “Copy safe diagnostics” excludes credentials, resource IDs, paths, environment variables, and user configuration contents.

Use a dedicated, least-privilege RunPod key where possible. Revoke it in RunPod if the workstation or keychain account is compromised.

## Lifecycle limits

mintPod journals ownership before paid mutations, retries pod termination, and forces recovery before another launch after an interrupted session. This protects common cancellation, UI-close, and process-crash cases.

Budget and idle enforcement still run on the workstation. Power loss, forced termination, sleep behavior, or prolonged network failure can leave a pod running until mintPod reconnects or the user intervenes. Users must retain access to the RunPod console and confirm cleanup after abnormal machine or network failure.

Persistent Network Volumes survive pod termination by design and may continue to incur storage charges.

## Out of scope

mintPod does not attempt to defend against:

- an attacker with access to the unlocked user account or OS keychain;
- a compromised coding tool that can read its own provider configuration;
- a compromised RunPod account, base image, host, or control plane;
- arbitrary hostile models or model-generated tool actions;
- denial of service against the public RunPod proxy.
