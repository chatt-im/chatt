type DebugLogEntry = {
  channel: string;
  stage: string;
  t?: number;
  iso: string;
  [key: string]: unknown;
};

type DebugLogStats = {
  count: number;
  dropped: number;
  max: number;
};

declare global {
  interface Window {
    chattDownloadScrollLog?: () => void;
    chattDownloadDebugLog?: () => void;
    chattClearDebugLog?: () => void;
    chattDebugLogStats?: () => DebugLogStats;
  }
}

const MAX_DEBUG_LOG_ENTRIES = 100_000;
const DROP_CHUNK = 5_000;
const debugLogEntries: DebugLogEntry[] = [];
let droppedDebugLogEntries = 0;
let helpersInstalled = false;
let helperNoticePrinted = false;

export function debugFlagEnabled(
  queryParam: string,
  storageKey: string
): boolean {
  if (typeof location === "undefined") return false;
  if (new URLSearchParams(location.search).has(queryParam)) return true;
  try {
    return localStorage.getItem(storageKey) === "1";
  } catch {
    return false;
  }
}

function consoleMirrorEnabled(): boolean {
  return debugFlagEnabled("debugScrollConsole", "chatt.debugScrollConsole");
}

function nowMs(): number | undefined {
  if (typeof performance === "undefined") return undefined;
  return Math.round(performance.now() * 10) / 10;
}

function downloadJsonl(entries: readonly DebugLogEntry[], fileName: string) {
  const header =
    droppedDebugLogEntries > 0
      ? [
          JSON.stringify({
            channel: "meta",
            stage: "dropped",
            iso: new Date().toISOString(),
            dropped: droppedDebugLogEntries,
          }),
        ]
      : [];
  const body = [...header, ...entries.map((entry) => JSON.stringify(entry))]
    .join("\n")
    .concat("\n");
  const blob = new Blob([body], {
    type: "application/x-ndjson;charset=utf-8",
  });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = fileName;
  document.body.append(link);
  link.click();
  link.remove();
  URL.revokeObjectURL(url);
}

function installDebugLogHelpers() {
  if (helpersInstalled || typeof window === "undefined") return;
  helpersInstalled = true;

  window.chattDownloadScrollLog = () => {
    const entries = debugLogEntries.filter(
      (entry) => entry.channel === "scroll" || entry.channel === "virtua"
    );
    downloadJsonl(entries, "chatt-scroll-log.jsonl");
  };
  window.chattDownloadDebugLog = () => {
    downloadJsonl(debugLogEntries, "chatt-debug-log.jsonl");
  };
  window.chattClearDebugLog = () => {
    debugLogEntries.length = 0;
    droppedDebugLogEntries = 0;
  };
  window.chattDebugLogStats = () => ({
    count: debugLogEntries.length,
    dropped: droppedDebugLogEntries,
    max: MAX_DEBUG_LOG_ENTRIES,
  });
}

function printHelperNotice() {
  if (helperNoticePrinted || typeof console === "undefined") return;
  helperNoticePrinted = true;
  console.info(
    "[chatt:debug] recording scroll diagnostics. Run chattDownloadScrollLog() to download JSONL; chattDebugLogStats() shows buffer usage."
  );
}

export function appendDebugLog(
  channel: string,
  stage: string,
  fields: Record<string, unknown> = {}
) {
  installDebugLogHelpers();
  printHelperNotice();

  if (debugLogEntries.length >= MAX_DEBUG_LOG_ENTRIES) {
    const dropped = Math.min(DROP_CHUNK, debugLogEntries.length);
    debugLogEntries.splice(0, dropped);
    droppedDebugLogEntries += dropped;
  }

  const entry: DebugLogEntry = {
    channel,
    stage,
    t: nowMs(),
    iso: new Date().toISOString(),
    ...fields,
  };
  debugLogEntries.push(entry);

  if (consoleMirrorEnabled()) {
    console.debug(`[chatt:${channel}]`, entry);
  }
}
