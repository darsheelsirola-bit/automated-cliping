import { useEffect, useState, type FormEvent } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { api, productionApi, twitchApi, type Dashboard, type TwitchStatus, type TwitchVerification, type YouTubeStatus } from "./api";
import { Icon } from "./Icon";
import { StorageValue } from "./ui";

export type RunAction = (action: () => Promise<void>) => Promise<boolean>;

export function Preferences({ data, twitch, busy, run }: { data: Dashboard; twitch: TwitchStatus | null; busy: boolean; run: RunAction }) {
  const [draft, setDraft] = useState(data.settings);
  const [client, setClient] = useState(twitch?.clientId ?? "");
  const [verification, setVerification] = useState<TwitchVerification | null>(null);
  const [connecting, setConnecting] = useState(false);
  const [feedback, setFeedback] = useState("");

  function save(event: FormEvent) {
    event.preventDefault();
    setFeedback("");
    void run(async () => {
      await api.saveSettings({ ...draft, monitoringEnabled: data.settings.monitoringEnabled });
      setFeedback("Settings saved.");
    });
  }

  async function connect(event: FormEvent) {
    event.preventDefault();
    setConnecting(true);
    try {
      const started = await run(async () => {
        setVerification(await twitchApi.begin(client.trim()));
      });
      if (started) await run(twitchApi.finish);
    } finally {
      setConnecting(false);
      setVerification(null);
    }
  }

  return <div className="preferences">
    <section className="panel">
      <div className="panel-heading"><h2>Twitch</h2><span className="muted">{twitch?.connected ? "Connected" : "Disconnected"}</span></div>
      <form className="settings-form" onSubmit={connect}>
        {verification ? <div className="verification">
          <p>Enter this device code on Twitch <strong>{verification.userCode}</strong></p>
          <button className="button" type="button" onClick={() => void openUrl(verification.verificationUri)}>Open Twitch <Icon name="external" size={15} /></button>
        </div> : <>
          <label>Developer client ID<input value={client} spellCheck={false} onChange={(event) => setClient(event.target.value)} required /></label>
          <div><button className="button" disabled={busy || connecting}>{connecting ? "Connecting…" : twitch?.connected ? "Reconnect Twitch" : "Connect Twitch"}</button></div>
        </>}
      </form>
    </section>
    <LocalModel />
    <YouTubeConnection />
    <form onSubmit={save}>
      <section className="panel">
        <div className="panel-heading"><h2>Storage</h2><Icon name="drive" /></div>
        <div className="settings-form">
          <div className="storage-readout"><span>Saved clips <strong><StorageValue bytes={data.savedBytes} /></strong></span><span>Recording buffer <strong><StorageValue bytes={data.temporaryBytes} /></strong></span></div>
          <label>Storage folder<input value={draft.storageDirectory} onChange={(event) => setDraft({ ...draft, storageDirectory: event.target.value })} required /></label>
          <label>Saved clip budget (GB)<input type="number" min="1" max="1000" step="0.1" value={draft.savedBudgetGb || ""} onChange={(event) => setDraft({ ...draft, savedBudgetGb: event.target.valueAsNumber })} required /></label>
        </div>
      </section>
      <section className="panel">
        <div className="panel-heading"><h2>Highlights and publishing</h2></div>
        <div className="settings-form">
          <div className="form-columns">
            <label>Minimum duration (seconds)<input type="number" min="5" max="180" value={draft.minClipSeconds || ""} onChange={(event) => setDraft({ ...draft, minClipSeconds: event.target.valueAsNumber })} required /></label>
            <label>Maximum duration (seconds)<input type="number" min="5" max="180" value={draft.maxClipSeconds || ""} onChange={(event) => setDraft({ ...draft, maxClipSeconds: event.target.valueAsNumber })} required /></label>
          </div>
          <label>Highlight threshold (chat and audio reactions)<input type="number" min="0" max="100" value={Number.isNaN(draft.detectionThreshold) ? "" : draft.detectionThreshold} onChange={(event) => setDraft({ ...draft, detectionThreshold: event.target.valueAsNumber })} required /></label>
          <label>Minimum publishing confidence (effective minimum 80)<input type="number" min="0" max="100" value={Number.isNaN(draft.minimumConfidence) ? "" : draft.minimumConfidence} onChange={(event) => setDraft({ ...draft, minimumConfidence: event.target.valueAsNumber })} required /></label>
          <label className="checkbox"><input type="checkbox" checked={draft.autoPublish} onChange={(event) => setDraft({ ...draft, autoPublish: event.target.checked })} />Automatically review and publish Shorts</label>
          <p>ClipForge adds captions and hashtags, then removes local files once it rejects a clip or confirms publication.</p>
        </div>
      </section>
      <section className="panel">
        <div className="panel-heading"><h2>Background operation</h2></div>
        <div className="settings-form">
          <label className="checkbox"><input type="checkbox" checked={draft.startWithWindows} onChange={(event) => setDraft({ ...draft, startWithWindows: event.target.checked })} />Start in the system tray when I sign in to Windows</label>
          <label className="checkbox"><input type="checkbox" checked={draft.keepAwake} onChange={(event) => setDraft({ ...draft, keepAwake: event.target.checked })} />Keep Windows awake while recording</label>
        </div>
      </section>
      <div className="settings-actions"><button className="button primary" disabled={busy}>Save settings</button><span role="status">{feedback}</span></div>
    </form>
  </div>;
}


function LocalModel() {
  const [ready, setReady] = useState<boolean | null>(null);
  const [installing, setInstalling] = useState(false);
  const [error, setError] = useState("");
  const [weights, setWeights] = useState({ weightBytes: 0, totalWeightBytes: 0 });

  useEffect(() => {
    let present = true;
    async function refresh() {
      try {
        const value = await productionApi.modelProgress();
        if (present) {
          setReady(value.ready);
          setWeights(value);
        }
      } catch (reason) {
        if (present) setError(String(reason));
      }
    }
    void refresh();
    const timer = installing ? window.setInterval(() => void refresh(), 1200) : undefined;
    return () => {
      present = false;
      if (timer !== undefined) window.clearInterval(timer);
    };
  }, [installing]);

  async function install() {
    setInstalling(true);
    setError("");
    try {
      await productionApi.setupModel();
      setReady(await productionApi.modelReady());
    } catch (reason) {
      setError(String(reason));
    } finally {
      setInstalling(false);
    }
  }

  return <section className="panel">
    <div className="panel-heading"><h2>Whisper Large-v3</h2><span className="muted">
      {ready ? "Ready" : installing ? "Installing" : ready === false ? "Setup required" : "Checking"}
    </span></div>
    <div className="settings-form">
      <p className="muted">Download the model to transcribe clips on this computer.</p>
      {installing && <div role="status">
        <span>Installing dependencies and verifying model files…</span>
        <progress aria-label="Whisper weight download" max={weights.totalWeightBytes || 1}
          value={weights.weightBytes || undefined} />
        {weights.weightBytes > 0 && <span><StorageValue bytes={weights.weightBytes} /> of <StorageValue bytes={weights.totalWeightBytes} /></span>}
      </div>}
      {ready && <p className="muted">Model weights <StorageValue bytes={weights.weightBytes} /></p>}
      {error && <p role="alert" className="field-error">{error}</p>}
      {!ready && <div><button type="button" className="button" disabled={installing} onClick={() => void install()}>
        {installing ? "Installing…" : "Install Whisper model"}
      </button></div>}
    </div>
  </section>;
}

function YouTubeConnection() {
  const [status, setStatus] = useState<YouTubeStatus | null>(null);
  const [file, setFile] = useState<File | null>(null);
  const [connecting, setConnecting] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    let present = true;
    void productionApi.youtubeStatus().then(
      (value) => { if (present) setStatus(value); },
      (reason: unknown) => { if (present) setError(String(reason)); },
    );
    return () => { present = false; };
  }, []);

  async function connect(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!file) return;
    const form = event.currentTarget;
    setConnecting(true);
    setError("");
    try {
      await productionApi.youtubeConnect(await file.text());
      setStatus(await productionApi.youtubeStatus());
      form.reset();
      setFile(null);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setConnecting(false);
    }
  }

  return <section className="panel">
    <div className="panel-heading"><h2>YouTube</h2><span className="muted">
      {status?.connected ? status.channelTitle || "Connected" : status ? "Disconnected" : "Checking"}
    </span></div>
    <form className="settings-form" onSubmit={(event) => void connect(event)}>
      <label>Google OAuth desktop client JSON
        <input type="file" accept=".json,application/json" onChange={(event) => setFile(event.target.files?.[0] ?? null)} disabled={connecting} required />
      </label>
      {connecting && <p role="status" className="muted">Finish authorizing ClipForge in your browser.</p>}
      {error && <p role="alert" className="field-error">{error}</p>}
      <div><button type="submit" className="button" disabled={!file || connecting}>
        {connecting ? "Connecting…" : status?.connected ? "Reconnect YouTube" : "Connect YouTube"}
      </button></div>
    </form>
  </section>;
}
