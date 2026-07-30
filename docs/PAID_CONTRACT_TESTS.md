# Paid RunPod contract tests

These checks create billable RunPod resources. They are intentionally manual, must use a disposable budget, and must never run in normal CI.

## Before starting

- Publish the exact runtime image pinned in `src-tauri/src/runpod.rs`.
- Use a dedicated RunPod API key and a test account or hard external spending limit.
- Confirm the chosen data center and one candidate GPU are available.
- Set mintPod's maximum GPU rate only slightly above the expected rate.
- Set a five-minute time budget.
- Open the RunPod console before launch so cleanup can be verified independently.
- Record the current pod and Network Volume lists without copying resource IDs into public reports.

Stop immediately and clean up in the RunPod console if the desktop loses network access, cleanup reports an error, or the observed rate exceeds the planned test budget.

## Matrix

Run each row on every release operating system. A single real GPU/model combination is enough for lifecycle release qualification; each curated profile still needs its own model/context/cache row before it can be marked `manuallyTested`.

| Scenario | Expected result |
| --- | --- |
| Live preflight | Reports global Secure Cloud stock, filters GPUs below the profile VRAM, and rejects rates above the accepted maximum. |
| Placement race or unavailable GPU | Launch fails with a useful message, creates no second session, and leaves no running pod. |
| Cancel before pod creation | Launch returns to idle and no pod exists. An already-created model volume may remain. |
| Cancel during boot | The recorded pod is terminated before mintPod returns to idle. |
| Cancel during model pull | The pod is terminated; the partially populated Network Volume remains reusable. |
| First complete launch | The exact runtime image starts, the model pulls, warms, and reports the configured context through `/api/ps`. |
| Remote authentication | The public `-8000.proxy.runpod.net` endpoint returns `401` without a bearer token and succeeds only through the authenticated mintPod path. Raw Ollama port `11434` is not published. |
| Local authentication | `127.0.0.1:11435/v1/models` returns `401` without the local token and the configured coding tools succeed. |
| Pi | Only `providers.mintpod` is added; an unrelated provider survives unchanged; the selected model can stream an agent request. |
| OpenCode | Only `provider.mintpod` is added; unrelated config survives; the selected model can stream an agent request. |
| Aider | The copied command starts Aider against `openai/<tag>` without changing global Aider config. |
| Tool missing | Its status is “Not installed” and the model session still becomes ready. |
| Second launch | Reuses the same Network Volume and reports the model as cached instead of pulling it again. |
| Explicit context | The loaded model context equals the profile's `contextLength`, not an Ollama VRAM-derived default. |
| Maximum-rate check | An allocation above the accepted rate is terminated before model download. |
| Idle timeout | Only generation requests reset activity; health and model-list polling do not. The pod is terminated at the deadline. |
| Time budget | Billing time includes boot, pull, and warm-up. The pod is terminated at the deadline. |
| EUR budget | Accrued cost uses refreshed RunPod rate and terminates at the selected limit. |
| Forced process crash | Kill mintPod without a graceful close. Restart shows recovery before launch and can reconnect to a healthy pod. |
| Recovery cleanup | “End recovered session” terminates the owned pod, removes owned tool entries, and retains the Network Volume. |
| Create-response ambiguity | Interrupt immediately around pod creation. Recovery resolves the deterministic pod name or refuses to discard uncertain ownership without explicit cleanup. |
| Normal end | The pod is terminated, not merely stopped. Pi/OpenCode entries disappear, Aider needs no cleanup, and recent history is recorded. |
| Window close | The window stays open while cleanup is retried; it exits only after termination succeeds. |

## Cleanup gate

Do not declare a run complete until all of these are true:

1. The RunPod console shows no running or stopped mintPod pods.
2. Only the intentionally retained model volumes remain.
3. Pi and OpenCode contain no `mintpod` or legacy `podpilot` provider.
4. Restarting mintPod shows no recovery screen.
5. The test API key is revoked if it was disposable.

Record durations and approximate cost, but do not include API keys, bearer tokens, pod or machine IDs, account identifiers, or complete user configuration.

## Read-only live API contract

The repository also has an ignored, explicitly opted-in test for current REST authentication and GraphQL GPU inventory response shape. It creates no resources:

```sh
MINTPOD_LIVE_RUNPOD_TESTS=1 \
RUNPOD_API_KEY='...' \
cargo test --manifest-path src-tauri/Cargo.toml live_runpod_read_contract -- --ignored
```
