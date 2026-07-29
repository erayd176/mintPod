<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { onMount } from "svelte";
  import { fade } from "svelte/transition";

  type Screen = "loading" | "setup" | "idle" | "launching" | "running";
  type BudgetMode = "time" | "cost";
  type LaunchStage =
    | "requestingPod"
    | "bootingContainer"
    | "pullingModel"
    | "warmingUp"
    | "ready";

  interface Preset {
    id: string;
    label: string;
    ollamaTag: string;
    sizeGb: number;
    minVramGb: number;
    gpuTypeIds: string[];
    estCostPerHr: number;
    tags: string[];
    userDefined: boolean;
  }

  interface Settings {
    storageRegion: string;
    idleTimeoutMinutes: number;
    verifiedStorageRegions: string[];
  }

  interface LaunchEvent {
    stage: LaunchStage;
    detail: string;
    completedBytes: number | null;
    totalBytes: number | null;
    skipped: boolean;
  }

  interface Session {
    podId: string;
    presetId: string;
    modelLabel: string;
    ollamaTag: string;
    startedAtEpochMs: number;
    costPerHr: number;
    wiring: {
      harness: string;
      command: string;
      configPath: string;
    };
  }

  interface StageState {
    id: LaunchStage;
    label: string;
    state: "pending" | "active" | "done" | "skipped";
    detail: string;
    completedBytes: number | null;
    totalBytes: number | null;
  }

  const stageOrder: Array<{ id: LaunchStage; label: string }> = [
    { id: "requestingPod", label: "Requesting pod" },
    { id: "bootingContainer", label: "Booting container" },
    { id: "pullingModel", label: "Pulling model" },
    { id: "warmingUp", label: "Warming up" },
    { id: "ready", label: "Ready" }
  ];

  let screen: Screen = "loading";
  let presets: Preset[] = [];
  let settings: Settings | null = null;
  let selectedId = "";
  let apiKey = "";
  let setupBusy = false;
  let launchBusy = false;
  let errorMessage = "";
  let alwaysOnTop = false;
  let budgetMode: BudgetMode = "time";
  let timeBudgetMinutes = 60;
  let costBudgetEur = 1;
  let session: Session | null = null;
  let now = Date.now();
  let copied = false;
  let holdingStop = false;
  let stopBusy = false;
  let stopTimer: ReturnType<typeof setTimeout> | null = null;
  let stages: StageState[] = freshStages();

  $: selectedPreset = presets.find((preset) => preset.id === selectedId) ?? null;
  $: elapsedSeconds = session
    ? Math.max(0, Math.floor((now - session.startedAtEpochMs) / 1000))
    : 0;
  $: accruedCost = session ? (elapsedSeconds / 3600) * session.costPerHr : 0;
  $: budgetRemaining =
    budgetMode === "time"
      ? Math.max(0, timeBudgetMinutes * 60 - elapsedSeconds)
      : Math.max(
          0,
          session?.costPerHr
            ? ((costBudgetEur - accruedCost) / session.costPerHr) * 3600
            : 0
        );

  onMount(() => {
    let unlisten: UnlistenFn | undefined;
    const clock = window.setInterval(() => (now = Date.now()), 1000);

    void listen<LaunchEvent>("launch-progress", ({ payload }) => {
      applyLaunchEvent(payload);
    }).then((dispose) => {
      unlisten = dispose;
    });
    void initialize();

    return () => {
      window.clearInterval(clock);
      unlisten?.();
      cancelStop();
    };
  });

  function freshStages(): StageState[] {
    return stageOrder.map((stage) => ({
      ...stage,
      state: "pending",
      detail: "",
      completedBytes: null,
      totalBytes: null
    }));
  }

  async function initialize() {
    try {
      const [hasKey, availablePresets, currentSettings] = await Promise.all([
        invoke<boolean>("api_key_status"),
        invoke<Preset[]>("list_presets"),
        invoke<Settings>("get_settings")
      ]);
      presets = availablePresets;
      settings = currentSettings;
      selectedId =
        availablePresets.find((preset) => preset.tags.includes("recommended"))?.id ??
        availablePresets[0]?.id ??
        "";
      screen = hasKey ? "idle" : "setup";
    } catch (error) {
      errorMessage = messageFrom(error);
      screen = "setup";
    }
  }

  async function saveApiKey() {
    if (!apiKey.trim()) return;
    setupBusy = true;
    errorMessage = "";
    try {
      await invoke("save_api_key", { apiKey: apiKey.trim() });
      apiKey = "";
      screen = "idle";
    } catch (error) {
      errorMessage = messageFrom(error);
    } finally {
      setupBusy = false;
    }
  }

  async function launch() {
    if (!selectedPreset || launchBusy) return;
    launchBusy = true;
    errorMessage = "";
    stages = freshStages();
    screen = "launching";
    try {
      session = await invoke<Session>("launch_preset", {
        presetId: selectedPreset.id
      });
      now = Date.now();
      screen = "running";
    } catch (error) {
      errorMessage = messageFrom(error);
      screen = "idle";
    } finally {
      launchBusy = false;
    }
  }

  function applyLaunchEvent(event: LaunchEvent) {
    const activeIndex = stageOrder.findIndex((stage) => stage.id === event.stage);
    stages = stages.map((stage, index) => {
      if (index < activeIndex) {
        return { ...stage, state: stage.state === "skipped" ? "skipped" : "done" };
      }
      if (index > activeIndex) return stage;
      return {
        ...stage,
        state: event.skipped ? "skipped" : event.stage === "ready" ? "done" : "active",
        detail: event.detail,
        completedBytes: event.completedBytes,
        totalBytes: event.totalBytes
      };
    });
  }

  async function toggleAlwaysOnTop() {
    const next = !alwaysOnTop;
    try {
      await getCurrentWindow().setAlwaysOnTop(next);
      alwaysOnTop = next;
    } catch (error) {
      errorMessage = messageFrom(error);
    }
  }

  async function copyCommand() {
    if (!session) return;
    await navigator.clipboard.writeText(session.wiring.command);
    copied = true;
    window.setTimeout(() => (copied = false), 1400);
  }

  function beginStop() {
    if (stopBusy || holdingStop) return;
    holdingStop = true;
    stopTimer = setTimeout(() => void stopNow(), 1000);
  }

  function cancelStop() {
    if (stopTimer) clearTimeout(stopTimer);
    stopTimer = null;
    holdingStop = false;
  }

  async function stopNow() {
    stopTimer = null;
    stopBusy = true;
    try {
      await invoke("stop_session");
      session = null;
      screen = "idle";
    } catch (error) {
      errorMessage = messageFrom(error);
    } finally {
      stopBusy = false;
      holdingStop = false;
    }
  }

  function handleStopKeydown(event: KeyboardEvent) {
    if ((event.key === " " || event.key === "Enter") && !event.repeat) {
      event.preventDefault();
      beginStop();
    }
  }

  function handleStopKeyup(event: KeyboardEvent) {
    if (event.key === " " || event.key === "Enter") cancelStop();
  }

  function formatMoney(value: number, digits = 2) {
    return `€${value.toFixed(digits)}`;
  }

  function formatDuration(totalSeconds: number) {
    const seconds = Math.max(0, Math.floor(totalSeconds));
    const hours = Math.floor(seconds / 3600);
    const minutes = Math.floor((seconds % 3600) / 60);
    const remainder = seconds % 60;
    return hours > 0
      ? `${hours}:${minutes.toString().padStart(2, "0")}:${remainder
          .toString()
          .padStart(2, "0")}`
      : `${minutes}:${remainder.toString().padStart(2, "0")}`;
  }

  function formatBytes(bytes: number) {
    if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(0)} MB`;
    return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
  }

  function messageFrom(error: unknown) {
    return error instanceof Error ? error.message : String(error);
  }
</script>

<main class="panel">
  <header class="titlebar" data-tauri-drag-region>
    <div class="brand" data-tauri-drag-region>
      <span class="brand-mark"></span>
      <span data-tauri-drag-region>PodPilot</span>
    </div>
    <div class="window-actions">
      <button
        class:active={alwaysOnTop}
        class="window-button pin"
        type="button"
        aria-label="Toggle always on top"
        title="Always on top"
        onclick={toggleAlwaysOnTop}
      >
        <svg viewBox="0 0 16 16" aria-hidden="true">
          <path d="m5.2 2.5 5.6 5.6M9.7 2l4.3 4.3-2.2 1.1-2.5 2.5.2 3-1 1-2.8-3.7L2 7.4l1-1 3 .2 2.5-2.5L9.7 2Z" />
        </svg>
      </button>
      <button
        class="window-button"
        type="button"
        aria-label="Minimize"
        title="Minimize"
        onclick={() => getCurrentWindow().minimize()}
      >
        <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 8h8" /></svg>
      </button>
      <button
        class="window-button close"
        type="button"
        aria-label="Close"
        title="Close"
        onclick={() => getCurrentWindow().close()}
      >
        <svg viewBox="0 0 16 16" aria-hidden="true">
          <path d="m5 5 6 6m0-6-6 6" />
        </svg>
      </button>
    </div>
  </header>

  <div class="viewport">
    {#key screen}
      <section class="screen" in:fade={{ duration: 150 }} out:fade={{ duration: 100 }}>
        {#if screen === "loading"}
          <div class="center-state">
            <span class="loader"></span>
            <p>Reading local configuration</p>
          </div>
        {:else if screen === "setup"}
          <div class="setup">
            <div>
              <p class="eyebrow">RunPod access</p>
              <h1>Connect your account</h1>
              <p class="lede">
                PodPilot validates the key once, then keeps it in the operating system keychain.
              </p>
            </div>
            <form onsubmit={(event) => { event.preventDefault(); void saveApiKey(); }}>
              <label for="runpod-key">API key</label>
              <input
                id="runpod-key"
                bind:value={apiKey}
                type="password"
                autocomplete="off"
                spellcheck="false"
                placeholder="Paste RunPod API key"
              />
              {#if settings}
                <label for="storage-region">Model storage region</label>
                <select
                  id="storage-region"
                  bind:value={settings.storageRegion}
                  onchange={() =>
                    invoke("set_storage_region", { region: settings?.storageRegion })}
                >
                  {#each settings.verifiedStorageRegions as region}
                    <option value={region}>{region}</option>
                  {/each}
                </select>
              {/if}
              {#if errorMessage}<p class="inline-error">{errorMessage}</p>{/if}
              <button class="primary" type="submit" disabled={setupBusy || !apiKey.trim()}>
                {setupBusy ? "Validating key" : "Save API key"}
              </button>
            </form>
          </div>
        {:else if screen === "idle"}
          <div class="idle-layout">
            <div class="screen-heading">
              <div>
                <p class="eyebrow">GPU offline</p>
                <h1>Select a model</h1>
              </div>
              <span class="status-dot">Idle</span>
            </div>

            <div class="preset-list" role="radiogroup" aria-label="Model preset">
              {#each presets as preset}
                <button
                  type="button"
                  role="radio"
                  aria-checked={selectedId === preset.id}
                  class:selected={selectedId === preset.id}
                  class="preset-card"
                  onclick={() => (selectedId = preset.id)}
                >
                  <span class="radio-mark"></span>
                  <span class="preset-copy">
                    <span class="preset-title">
                      {preset.label}
                      {#if preset.tags.includes("recommended")}
                        <span class="tag">Default</span>
                      {/if}
                    </span>
                    <span class="preset-meta">{preset.sizeGb} GB · {preset.minVramGb} GB VRAM</span>
                  </span>
                  <span class="preset-cost">{formatMoney(preset.estCostPerHr)}<small>/hr</small></span>
                </button>
              {/each}
            </div>

            <div class="launch-controls">
              <div class="budget-row">
                <span class="field-label">Stop budget</span>
                <div class="segmented">
                  <button
                    class:active={budgetMode === "time"}
                    type="button"
                    onclick={() => (budgetMode = "time")}>Time</button
                  >
                  <button
                    class:active={budgetMode === "cost"}
                    type="button"
                    onclick={() => (budgetMode = "cost")}>Cost</button
                  >
                </div>
                {#if budgetMode === "time"}
                  <div class="number-field">
                    <input
                      aria-label="Time budget in minutes"
                      type="number"
                      min="5"
                      max="720"
                      step="5"
                      bind:value={timeBudgetMinutes}
                    />
                    <span>min</span>
                  </div>
                {:else}
                  <div class="number-field money">
                    <span>€</span>
                    <input
                      aria-label="Cost budget in euros"
                      type="number"
                      min="0.1"
                      max="100"
                      step="0.1"
                      bind:value={costBudgetEur}
                    />
                  </div>
                {/if}
              </div>
              {#if errorMessage}<p class="inline-error compact">{errorMessage}</p>{/if}
              <div class="storage-row">
                <span>Persistent storage</span>
                <button type="button" class="text-button">Manage models</button>
              </div>
              <button
                class="primary launch-button"
                type="button"
                disabled={!selectedPreset || launchBusy}
                onclick={launch}
              >
                <span>Launch {selectedPreset?.label ?? "model"}</span>
                <svg viewBox="0 0 16 16" aria-hidden="true">
                  <path d="M3.5 8h9m-3.3-3.3L12.5 8l-3.3 3.3" />
                </svg>
              </button>
            </div>
          </div>
        {:else if screen === "launching"}
          <div class="launching-layout">
            <div>
              <p class="eyebrow">Starting session</p>
              <h1>{selectedPreset?.label}</h1>
              <p class="lede">RunPod capacity, persistent cache, then VRAM.</p>
            </div>
            <ol class="stage-list">
              {#each stages as stage}
                <li class:active={stage.state === "active"} class:done={stage.state === "done"}>
                  <span class="stage-icon">
                    {#if stage.state === "done" || stage.state === "skipped"}
                      <svg viewBox="0 0 16 16" aria-hidden="true"><path d="m4 8.2 2.4 2.4L12 5" /></svg>
                    {:else if stage.state === "active"}
                      <span class="stage-spinner"></span>
                    {/if}
                  </span>
                  <span class="stage-copy">
                    <span>{stage.label}</span>
                    {#if stage.detail}<small>{stage.detail}</small>{/if}
                  </span>
                  {#if stage.state === "skipped"}
                    <span class="stage-value">Cached</span>
                  {:else if stage.completedBytes !== null && stage.totalBytes}
                    <span class="stage-value"
                      >{formatBytes(stage.completedBytes)} / {formatBytes(stage.totalBytes)}</span
                    >
                  {/if}
                </li>
              {/each}
            </ol>
            <p class="launch-note">Closing PodPilot during launch does not stop the remote pod.</p>
          </div>
        {:else if screen === "running" && session}
          <div class="running-layout">
            <div class="running-head">
              <div>
                <p class="eyebrow live">Session live</p>
                <h1>{session.modelLabel}</h1>
                <p class="model-tag">{session.ollamaTag}</p>
              </div>
              <span class="live-indicator"><span></span>Running</span>
            </div>

            <div class="metric-grid">
              <div class="metric">
                <span>Elapsed</span>
                <strong>{formatDuration(elapsedSeconds)}</strong>
              </div>
              <div class="metric">
                <span>Accrued</span>
                <strong>{formatMoney(accruedCost, 3)}</strong>
                <small>{formatMoney(session.costPerHr)}/hr</small>
              </div>
              <div class="metric wide">
                <span>{budgetMode === "time" ? "Time budget" : "Cost budget"}</span>
                <strong>{formatDuration(budgetRemaining)}</strong>
                <small>
                  {budgetMode === "time"
                    ? `${timeBudgetMinutes} min limit`
                    : `${formatMoney(costBudgetEur)} limit`}
                </small>
              </div>
              <div class="metric wide">
                <span>Idle auto-stop</span>
                <strong>{settings?.idleTimeoutMinutes ?? 10}:00</strong>
                <small>resets on local API traffic</small>
              </div>
            </div>

            <button class="wire-line" type="button" onclick={copyCommand}>
              <span class="wire-check">
                <svg viewBox="0 0 16 16" aria-hidden="true"><path d="m4 8.2 2.4 2.4L12 5" /></svg>
              </span>
              <span>
                <strong>Wired into {session.wiring.harness}</strong>
                <small>{copied ? "Command copied" : session.wiring.command}</small>
              </span>
              <svg class="copy-icon" viewBox="0 0 16 16" aria-hidden="true">
                <rect x="5.5" y="5.5" width="7" height="7" rx="1" />
                <path d="M3.5 10.5h-1v-7h7v1" />
              </svg>
            </button>

            {#if errorMessage}<p class="inline-error compact">{errorMessage}</p>{/if}
            <button
              class:holding={holdingStop}
              class="stop-button"
              type="button"
              disabled={stopBusy}
              onpointerdown={beginStop}
              onpointerup={cancelStop}
              onpointerleave={cancelStop}
              onpointercancel={cancelStop}
              onkeydown={handleStopKeydown}
              onkeyup={handleStopKeyup}
              onblur={cancelStop}
            >
              <span>{stopBusy ? "Stopping pod" : holdingStop ? "Keep holding" : "Hold to stop"}</span>
            </button>
          </div>
        {/if}
      </section>
    {/key}
  </div>
</main>
