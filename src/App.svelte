<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { onMount } from "svelte";
  import { fade } from "svelte/transition";

  type Screen = "loading" | "setup" | "idle" | "manage" | "launching" | "running";
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

  interface ApiKeyProfile {
    id: string;
    label: string;
    active: boolean;
  }

  interface LaunchEvent {
    stage: LaunchStage;
    detail: string;
    completedBytes: number | null;
    totalBytes: number | null;
    skipped: boolean;
  }

  interface GpuTier {
    id: string;
    label: string;
    gpuTypeIds: string[];
    estCostPerHr: number;
  }

  interface CachedModel {
    volumeId: string;
    presetId: string;
    label: string;
    ollamaTag: string;
    modelSizeGb: number;
    allocatedGb: number;
    dataCenterId: string;
  }

  interface CacheSummary {
    models: CachedModel[];
    totalAllocatedGb: number;
  }

  interface AddPresetResult {
    requiresConfirmation: boolean;
    warning: string | null;
    preset: Preset | null;
  }

  interface Session {
    podId: string;
    presetId: string;
    modelLabel: string;
    ollamaTag: string;
    gpuName: string;
    dataCenterId: string;
    startedAtEpochMs: number;
    costPerHrUsd: number;
    costPerHrEur: number;
    idleTimeoutMinutes: number;
    budget:
      | { kind: "time"; minutes: number }
      | { kind: "cost"; eur: number };
    wiring: {
      harness: string;
      command: string;
      configPath: string;
    };
  }

  interface SessionTelemetry {
    elapsedSeconds: number;
    accruedCostEur: number;
    costPerHrEur: number;
    budgetKind: BudgetMode;
    budgetRemainingSeconds: number | null;
    budgetRemainingEur: number | null;
    idleRemainingSeconds: number;
  }

  interface SessionStoppedEvent {
    reason: "manual" | "timeBudget" | "costBudget" | "idleTimeout" | "remoteStopped";
    historyError: string | null;
  }

  interface HistoryEntry {
    presetId: string;
    modelLabel: string;
    startedAtEpochMs: number;
    durationSeconds: number;
    finalCostEur: number;
    stopReason: string;
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
  let apiKeyLabel = "Default";
  let apiKeyProfiles: ApiKeyProfile[] = [];
  let activeApiKeyId = "";
  let newApiKey = "";
  let newApiKeyLabel = "";
  let replacementApiKey = "";
  let replacingProfileId = "";
  let keyBusy = "";
  let setupBusy = false;
  let launchBusy = false;
  let errorMessage = "";
  let alwaysOnTop = false;
  let budgetMode: BudgetMode = "time";
  let timeBudgetMinutes = 60;
  let costBudgetEur = 1;
  let session: Session | null = null;
  let copied = false;
  let holdingStop = false;
  let stopBusy = false;
  let stopTimer: ReturnType<typeof setTimeout> | null = null;
  let stages: StageState[] = freshStages();
  let cachedModels: CachedModel[] = [];
  let totalAllocatedGb = 0;
  let cacheBusy = false;
  let deletingVolumeId = "";
  let gpuTiers: GpuTier[] = [];
  let customTag = "";
  let customSizeGb = 5;
  let customMinVramGb = 12;
  let customGpuTierId = "balanced";
  let customBusy = false;
  let customWarning = "";
  let telemetry: SessionTelemetry | null = null;
  let recentSessions: HistoryEntry[] = [];

  $: selectedPreset = presets.find((preset) => preset.id === selectedId) ?? null;
  $: selectedGpuTier =
    gpuTiers.find((tier) => tier.id === customGpuTierId) ?? gpuTiers[0] ?? null;

  onMount(() => {
    const disposers: UnlistenFn[] = [];

    void listen<LaunchEvent>("launch-progress", ({ payload }) => {
      applyLaunchEvent(payload);
    }).then((dispose) => {
      disposers.push(dispose);
    });
    void listen<SessionTelemetry>("session-telemetry", ({ payload }) => {
      telemetry = payload;
    }).then((dispose) => {
      disposers.push(dispose);
    });
    void listen<SessionStoppedEvent>("session-stopped", ({ payload }) => {
      session = null;
      telemetry = null;
      screen = "idle";
      if (payload.historyError) errorMessage = payload.historyError;
      void refreshHistory();
      void refreshCache();
    }).then((dispose) => {
      disposers.push(dispose);
    });
    void listen<string>("session-cleanup-error", ({ payload }) => {
      errorMessage = payload;
    }).then((dispose) => {
      disposers.push(dispose);
    });
    void initialize();

    return () => {
      for (const dispose of disposers) dispose();
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
      const [keyProfiles, availablePresets, currentSettings, history] = await Promise.all([
        invoke<ApiKeyProfile[]>("list_api_keys"),
        invoke<Preset[]>("list_presets"),
        invoke<Settings>("get_settings"),
        invoke<HistoryEntry[]>("session_history")
      ]);
      apiKeyProfiles = keyProfiles;
      activeApiKeyId = keyProfiles.find((profile) => profile.active)?.id ?? "";
      presets = availablePresets;
      settings = currentSettings;
      recentSessions = history;
      selectedId =
        availablePresets.find((preset) => preset.tags.includes("recommended"))?.id ??
        availablePresets[0]?.id ??
        "";
      screen = keyProfiles.length ? "idle" : "setup";
      if (keyProfiles.length) void refreshCache();
    } catch (error) {
      errorMessage = messageFrom(error);
      screen = "setup";
    }
  }

  async function refreshHistory() {
    try {
      recentSessions = await invoke<HistoryEntry[]>("session_history");
    } catch (error) {
      errorMessage = messageFrom(error);
    }
  }

  async function updateStorageRegion() {
    if (!settings) return;
    try {
      await invoke("set_storage_region", { region: settings.storageRegion });
      await refreshCache();
    } catch (error) {
      errorMessage = messageFrom(error);
    }
  }

  async function updateIdleTimeout() {
    if (!settings) return;
    try {
      await invoke("set_idle_timeout", { minutes: settings.idleTimeoutMinutes });
    } catch (error) {
      errorMessage = messageFrom(error);
    }
  }

  async function saveApiKey() {
    if (!apiKey.trim() || !apiKeyLabel.trim()) return;
    setupBusy = true;
    errorMessage = "";
    try {
      await invoke<ApiKeyProfile>("add_api_key", {
        label: apiKeyLabel.trim(),
        apiKey: apiKey.trim()
      });
      apiKey = "";
      await refreshApiKeys();
      screen = "idle";
      void refreshCache();
    } catch (error) {
      errorMessage = messageFrom(error);
    } finally {
      setupBusy = false;
    }
  }

  async function refreshApiKeys() {
    apiKeyProfiles = await invoke<ApiKeyProfile[]>("list_api_keys");
    activeApiKeyId = apiKeyProfiles.find((profile) => profile.active)?.id ?? "";
  }

  async function addApiKey() {
    if (!newApiKey.trim() || !newApiKeyLabel.trim() || keyBusy) return;
    keyBusy = "add";
    errorMessage = "";
    try {
      await invoke<ApiKeyProfile>("add_api_key", {
        label: newApiKeyLabel.trim(),
        apiKey: newApiKey.trim()
      });
      newApiKey = "";
      newApiKeyLabel = "";
      await refreshApiKeys();
      await refreshCache();
    } catch (error) {
      errorMessage = messageFrom(error);
    } finally {
      keyBusy = "";
    }
  }

  async function selectApiKey(profileId: string) {
    if (!profileId || keyBusy) return;
    keyBusy = `select:${profileId}`;
    errorMessage = "";
    try {
      await invoke("select_api_key", { profileId });
      await refreshApiKeys();
      await refreshCache();
    } catch (error) {
      errorMessage = messageFrom(error);
      await refreshApiKeys();
    } finally {
      keyBusy = "";
    }
  }

  function beginReplaceApiKey(profileId: string) {
    replacingProfileId = profileId;
    replacementApiKey = "";
    errorMessage = "";
  }

  async function replaceApiKey(profileId: string) {
    if (!replacementApiKey.trim() || keyBusy) return;
    keyBusy = `replace:${profileId}`;
    errorMessage = "";
    try {
      await invoke("replace_api_key", {
        profileId,
        apiKey: replacementApiKey.trim()
      });
      replacementApiKey = "";
      replacingProfileId = "";
      await refreshCache();
    } catch (error) {
      errorMessage = messageFrom(error);
    } finally {
      keyBusy = "";
    }
  }

  async function removeApiKey(profileId: string) {
    if (keyBusy) return;
    keyBusy = `remove:${profileId}`;
    errorMessage = "";
    try {
      await invoke("remove_api_key", { profileId });
      if (replacingProfileId === profileId) {
        replacingProfileId = "";
        replacementApiKey = "";
      }
      await refreshApiKeys();
      if (apiKeyProfiles.length === 0) {
        apiKeyLabel = "Default";
        screen = "setup";
      } else {
        await refreshCache();
      }
    } catch (error) {
      errorMessage = messageFrom(error);
    } finally {
      keyBusy = "";
    }
  }

  async function refreshCache() {
    cacheBusy = true;
    try {
      const cache = await invoke<CacheSummary>("list_cached_models");
      cachedModels = cache.models;
      totalAllocatedGb = cache.totalAllocatedGb;
    } catch (error) {
      errorMessage = messageFrom(error);
    } finally {
      cacheBusy = false;
    }
  }

  async function openManage() {
    screen = "manage";
    errorMessage = "";
    try {
      const [tiers] = await Promise.all([
        gpuTiers.length ? Promise.resolve(gpuTiers) : invoke<GpuTier[]>("list_gpu_tiers"),
        refreshApiKeys(),
        refreshCache()
      ]);
      gpuTiers = tiers;
      if (!tiers.some((tier) => tier.id === customGpuTierId)) {
        customGpuTierId = tiers[0]?.id ?? "";
      }
    } catch (error) {
      errorMessage = messageFrom(error);
    }
  }

  async function deleteCachedModel(volumeId: string) {
    deletingVolumeId = volumeId;
    errorMessage = "";
    try {
      await invoke("delete_cached_model", { volumeId });
      await refreshCache();
    } catch (error) {
      errorMessage = messageFrom(error);
    } finally {
      deletingVolumeId = "";
    }
  }

  async function addCustomPreset(confirmOutsideRange = false) {
    if (!selectedGpuTier || customBusy || !customTag.trim()) return;
    customBusy = true;
    errorMessage = "";
    try {
      const result = await invoke<AddPresetResult>("add_custom_preset", {
        input: {
          ollamaTag: customTag.trim(),
          sizeGb: customSizeGb,
          minVramGb: customMinVramGb,
          gpuTypeIds: selectedGpuTier.gpuTypeIds
        },
        confirmOutsideRange
      });
      if (result.requiresConfirmation) {
        customWarning = result.warning ?? "outside default hobby range, continue anyway?";
        return;
      }
      if (result.preset) {
        presets = [...presets, result.preset];
        selectedId = result.preset.id;
      }
      customTag = "";
      customSizeGb = 5;
      customMinVramGb = 12;
      customWarning = "";
    } catch (error) {
      errorMessage = messageFrom(error);
    } finally {
      customBusy = false;
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
        presetId: selectedPreset.id,
        budget:
          budgetMode === "time"
            ? { kind: "time", minutes: timeBudgetMinutes }
            : { kind: "cost", eur: costBudgetEur }
      });
      telemetry = {
        elapsedSeconds: 0,
        accruedCostEur: 0,
        costPerHrEur: session.costPerHrEur,
        budgetKind: budgetMode,
        budgetRemainingSeconds:
          budgetMode === "time" ? timeBudgetMinutes * 60 : null,
        budgetRemainingEur: budgetMode === "cost" ? costBudgetEur : null,
        idleRemainingSeconds: session.idleTimeoutMinutes * 60
      };
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
      telemetry = null;
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

  function stopReasonLabel(reason: string) {
    return reason === "manual" ? "manual" : reason;
  }

  function messageFrom(error: unknown) {
    return error instanceof Error ? error.message : String(error);
  }
</script>

<main class="panel">
  <header class="titlebar" data-tauri-drag-region>
    <div class="brand" data-tauri-drag-region>
      <span class="brand-mark"></span>
      <span data-tauri-drag-region>mintPod</span>
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
                mintPod validates the key once, then keeps it in the operating system keychain.
              </p>
            </div>
            <form onsubmit={(event) => { event.preventDefault(); void saveApiKey(); }}>
              <label for="runpod-key-label">Key name</label>
              <input
                id="runpod-key-label"
                bind:value={apiKeyLabel}
                type="text"
                maxlength="32"
                autocomplete="off"
                spellcheck="false"
                placeholder="Personal"
              />
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
                  onchange={updateStorageRegion}
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

            <div class="idle-scroll">
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
                    <span class="preset-cost" title="Estimated hourly compute cost"
                      >~{formatMoney(preset.estCostPerHr)}<small>/hr</small></span
                    >
                  </button>
                {/each}
              </div>
              {#if recentSessions.length > 0}
                <div class="history-block">
                  <div class="section-label"><span>Recent sessions</span><span>Last 5</span></div>
                  {#each recentSessions as entry}
                    <div class="history-row">
                      <span>
                        <strong>{entry.modelLabel}</strong>
                        <small>{formatDuration(entry.durationSeconds)} · {stopReasonLabel(entry.stopReason)}</small>
                      </span>
                      <span>{formatMoney(entry.finalCostEur, 3)}</span>
                    </div>
                  {/each}
                </div>
              {/if}
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
              <div class="account-row">
                <span>RunPod key</span>
                <select
                  aria-label="Active RunPod API key"
                  bind:value={activeApiKeyId}
                  disabled={Boolean(keyBusy)}
                  onchange={() => void selectApiKey(activeApiKeyId)}
                >
                  {#each apiKeyProfiles as profile}
                    <option value={profile.id}>{profile.label}</option>
                  {/each}
                </select>
                <button type="button" class="text-button" onclick={openManage}>Keys</button>
              </div>
              <div class="storage-row">
                <span
                  >Persistent storage · {cacheBusy ? "reading" : `${totalAllocatedGb} GB allocated`}</span
                >
                <button type="button" class="text-button" onclick={openManage}>Manage models</button>
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
        {:else if screen === "manage"}
          <div class="manage-layout">
            <div class="manage-heading">
              <button class="back-button" type="button" aria-label="Back" onclick={() => (screen = "idle")}>
                <svg viewBox="0 0 16 16" aria-hidden="true">
                  <path d="M12.5 8h-9m3.3-3.3L3.5 8l3.3 3.3" />
                </svg>
              </button>
              <div>
                <p class="eyebrow">Models and access</p>
                <h1>Manage</h1>
              </div>
              <span class="storage-total">{totalAllocatedGb} GB</span>
            </div>

            <div class="manage-scroll">
              <div class="section-label">
                <span>RunPod API keys</span>
                <span>{apiKeyProfiles.length}</span>
              </div>
              <div class="key-list">
                {#each apiKeyProfiles as profile}
                  <div class:active={profile.active} class="key-row">
                    <span class="key-copy">
                      <strong>{profile.label}</strong>
                      <small>{profile.active ? "Active" : "Stored in OS keychain"}</small>
                    </span>
                    <span class="key-actions">
                      {#if !profile.active}
                        <button
                          type="button"
                          disabled={Boolean(keyBusy)}
                          onclick={() => void selectApiKey(profile.id)}
                        >
                          {keyBusy === `select:${profile.id}` ? "Switching" : "Use"}
                        </button>
                      {/if}
                      <button
                        type="button"
                        disabled={Boolean(keyBusy)}
                        onclick={() => beginReplaceApiKey(profile.id)}
                      >
                        Replace
                      </button>
                      <button
                        class="danger"
                        type="button"
                        disabled={Boolean(keyBusy)}
                        onclick={() => void removeApiKey(profile.id)}
                      >
                        {keyBusy === `remove:${profile.id}` ? "Removing" : "Remove"}
                      </button>
                    </span>
                    {#if replacingProfileId === profile.id}
                      <form
                        class="replace-key-form"
                        onsubmit={(event) => {
                          event.preventDefault();
                          void replaceApiKey(profile.id);
                        }}
                      >
                        <input
                          aria-label={`Replacement API key for ${profile.label}`}
                          bind:value={replacementApiKey}
                          type="password"
                          autocomplete="off"
                          spellcheck="false"
                          placeholder="Paste replacement key"
                        />
                        <button
                          type="submit"
                          disabled={!replacementApiKey.trim() || Boolean(keyBusy)}
                        >
                          {keyBusy === `replace:${profile.id}` ? "Validating" : "Save"}
                        </button>
                        <button
                          type="button"
                          disabled={Boolean(keyBusy)}
                          onclick={() => {
                            replacingProfileId = "";
                            replacementApiKey = "";
                          }}
                        >
                          Cancel
                        </button>
                      </form>
                    {/if}
                  </div>
                {/each}
              </div>
              <form
                class="add-key-form"
                onsubmit={(event) => {
                  event.preventDefault();
                  void addApiKey();
                }}
              >
                <input
                  aria-label="New API key name"
                  bind:value={newApiKeyLabel}
                  type="text"
                  maxlength="32"
                  autocomplete="off"
                  spellcheck="false"
                  placeholder="Key name"
                />
                <input
                  aria-label="New RunPod API key"
                  bind:value={newApiKey}
                  type="password"
                  autocomplete="off"
                  spellcheck="false"
                  placeholder="RunPod API key"
                />
                <button
                  class="secondary-button"
                  type="submit"
                  disabled={!newApiKeyLabel.trim() || !newApiKey.trim() || Boolean(keyBusy)}
                >
                  {keyBusy === "add" ? "Validating" : "Add key"}
                </button>
              </form>

              <div class="section-label add-label">
                <span>Cached models</span>
                <span>{cachedModels.length}</span>
              </div>
              <div class="cache-list">
                {#if cacheBusy}
                  <div class="empty-cache"><span class="loader"></span>Reading RunPod volumes</div>
                {:else if cachedModels.length === 0}
                  <div class="empty-cache">No mintPod model volumes in this account.</div>
                {:else}
                  {#each cachedModels as model}
                    <div class="cache-row">
                      <span class="cache-icon">
                        <svg viewBox="0 0 16 16" aria-hidden="true">
                          <ellipse cx="8" cy="4" rx="5" ry="2" />
                          <path d="M3 4v4c0 1.1 2.2 2 5 2s5-.9 5-2V4M3 8v4c0 1.1 2.2 2 5 2s5-.9 5-2V8" />
                        </svg>
                      </span>
                      <span class="cache-copy">
                        <strong>{model.label}</strong>
                        <small
                          >~{model.modelSizeGb} GB weights · {model.allocatedGb} GB volume · {model.dataCenterId}</small
                        >
                      </span>
                      <button
                        class="delete-button"
                        type="button"
                        aria-label={`Delete ${model.label} cache`}
                        title="Delete cached model"
                        disabled={deletingVolumeId === model.volumeId}
                        onclick={() => deleteCachedModel(model.volumeId)}
                      >
                        {#if deletingVolumeId === model.volumeId}
                          <span class="stage-spinner"></span>
                        {:else}
                          <svg viewBox="0 0 16 16" aria-hidden="true">
                            <path d="M3.5 5h9M6 5V3.5h4V5m1.5 0-.5 8H5L4.5 5m2 2v4m3-4v4" />
                          </svg>
                        {/if}
                      </button>
                    </div>
                  {/each}
                {/if}
              </div>

              <div class="section-label add-label">
                <span>Add custom preset</span>
                <span>Ollama</span>
              </div>
              <form
                class="custom-form"
                onsubmit={(event) => {
                  event.preventDefault();
                  void addCustomPreset(false);
                }}
              >
                <label for="custom-tag">Ollama tag</label>
                <input
                  id="custom-tag"
                  type="text"
                  bind:value={customTag}
                  placeholder="qwen2.5-coder:7b"
                  autocomplete="off"
                  spellcheck="false"
                  oninput={() => (customWarning = "")}
                />
                <div class="form-pair">
                  <label>
                    <span>Model size</span>
                    <span class="number-field form-number">
                      <input
                        type="number"
                        min="0.1"
                        max="4000"
                        step="0.1"
                        bind:value={customSizeGb}
                        oninput={() => (customWarning = "")}
                      />
                      <span>GB</span>
                    </span>
                  </label>
                  <label>
                    <span>Minimum VRAM</span>
                    <span class="number-field form-number">
                      <input
                        type="number"
                        min="1"
                        max="1024"
                        step="1"
                        bind:value={customMinVramGb}
                        oninput={() => (customWarning = "")}
                      />
                      <span>GB</span>
                    </span>
                  </label>
                </div>
                <label for="gpu-tier">GPU priority tier</label>
                <select id="gpu-tier" bind:value={customGpuTierId} onchange={() => (customWarning = "")}>
                  {#each gpuTiers as tier}
                    <option value={tier.id}>{tier.label} · ~{formatMoney(tier.estCostPerHr)}/hr</option>
                  {/each}
                </select>
                {#if selectedGpuTier}
                  <p class="gpu-list">{selectedGpuTier.gpuTypeIds.join(" → ")}</p>
                {/if}
                {#if customWarning}
                  <div class="soft-warning">
                    <span>{customWarning}</span>
                    <button type="button" onclick={() => addCustomPreset(true)}>Continue anyway</button>
                  </div>
                {/if}
                {#if errorMessage}<p class="inline-error compact">{errorMessage}</p>{/if}
                <button
                  class="secondary-button"
                  type="submit"
                  disabled={customBusy || !customTag.trim() || !selectedGpuTier}
                >
                  {customBusy ? "Validating preset" : "Add preset"}
                </button>
              </form>

              {#if settings}
                <div class="section-label add-label">
                  <span>Runtime defaults</span>
                  <span>Local</span>
                </div>
                <div class="runtime-settings">
                  <label>
                    <span>Storage region</span>
                    <select bind:value={settings.storageRegion} onchange={updateStorageRegion}>
                      {#each settings.verifiedStorageRegions as region}
                        <option value={region}>{region}</option>
                      {/each}
                    </select>
                  </label>
                  <label>
                    <span>Idle timeout</span>
                    <span class="number-field form-number">
                      <input
                        type="number"
                        min="1"
                        max="240"
                        step="1"
                        bind:value={settings.idleTimeoutMinutes}
                        onchange={updateIdleTimeout}
                      />
                      <span>min</span>
                    </span>
                  </label>
                </div>
              {/if}
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
            <p class="launch-note">Closing mintPod during launch does not stop the remote pod.</p>
          </div>
        {:else if screen === "running" && session}
          <div class="running-layout">
            <div class="running-head">
              <div>
                <p class="eyebrow live">Session live</p>
                <h1>{session.modelLabel}</h1>
                <p class="model-tag">{session.ollamaTag}</p>
                <p class="allocation-line" title={`${session.gpuName} in ${session.dataCenterId}`}>
                  <strong>{session.gpuName}</strong>
                  <span>{session.dataCenterId}</span>
                </p>
              </div>
              <span class="live-indicator"><span></span>Running</span>
            </div>

            <div class="metric-grid">
              <div class="metric">
                <span>Elapsed</span>
                <strong>{formatDuration(telemetry?.elapsedSeconds ?? 0)}</strong>
              </div>
              <div class="metric">
                <span>Accrued</span>
                <strong>{formatMoney(telemetry?.accruedCostEur ?? 0, 3)}</strong>
                <small
                  >Actual rate · {formatMoney(
                    telemetry?.costPerHrEur ?? session.costPerHrEur
                  )}/hr</small
                >
              </div>
              <div class="metric wide">
                <span>{budgetMode === "time" ? "Time budget" : "Cost budget"}</span>
                <strong>
                  {budgetMode === "time"
                    ? formatDuration(telemetry?.budgetRemainingSeconds ?? timeBudgetMinutes * 60)
                    : formatMoney(telemetry?.budgetRemainingEur ?? costBudgetEur, 3)}
                </strong>
                <small>
                  {budgetMode === "time"
                    ? `${timeBudgetMinutes} min limit`
                    : `${formatMoney(costBudgetEur)} limit`}
                </small>
              </div>
              <div class="metric wide">
                <span>Idle auto-stop</span>
                <strong
                  >{formatDuration(
                    telemetry?.idleRemainingSeconds ?? (settings?.idleTimeoutMinutes ?? 10) * 60
                  )}</strong
                >
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
