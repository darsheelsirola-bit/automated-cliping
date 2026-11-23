import { convertFileSrc } from "@tauri-apps/api/core";
import { useCallback, useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { api, productionApi, twitchApi, type ChatActivity, type Clip, type Creator, type Dashboard, type TwitchStatus } from "./api";
import { Icon, type IconName } from "./Icon";
import { Preferences, type RunAction } from "./Preferences";
import { Empty, Stamp, Status, StorageValue } from "./ui";

const navigation = [
  { title: "Overview", icon: "overview" },
  { title: "Creators", icon: "creators" },
  { title: "Live Monitor", icon: "monitor" },
  { title: "Clip Library", icon: "clips" },
  { title: "Processing Queue", icon: "queue" },
  { title: "Publishing History", icon: "publishing" },
  { title: "Analytics", icon: "analytics" },
  { title: "Settings", icon: "settings" },
] as const;

type Page = typeof navigation[number]["title"];

const descriptions: Record<Page, string> = {
  Overview: "Track Twitch streams, recent highlights, and Shorts.",
  Creators: "Choose which Twitch channels ClipForge can monitor.",
  "Live Monitor": "Follow active streams, recording status, and highlight detections.",
  "Clip Library": "Browse Shorts rendered on this computer.",
  "Processing Queue": "Track each clip through transcription, rendering, and upload.",
  "Publishing History": "See YouTube uploads and delivery errors.",
  Analytics: "Review local detection, processing, and publishing totals.",
  Settings: "Manage connections, local storage, and publishing preferences.",
};

function App() {
  const [page, setPage] = useState<Page>("Overview");
  const [data, setData] = useState<Dashboard | null>(null);
  const [twitch, setTwitch] = useState<TwitchStatus | null>(null);
  const [error, setError] = useState("");
  const [loadError, setLoadError] = useState("");
  const [busy, setBusy] = useState(false);
  const [showAdd, setShowAdd] = useState(false);
  const request = useRef(0);
  const latest = useRef(0);

  const refresh = useCallback(async () => {
    const id = ++request.current;
    const [dashboard, status] = await Promise.allSettled([api.dashboard(), twitchApi.status()]);
    if (id < latest.current) return;
    latest.current = id;
    if (dashboard.status === "fulfilled") {
      setData(dashboard.value);
      setLoadError("");
    } else {
      setLoadError(String(dashboard.reason));
    }
    setTwitch(status.status === "fulfilled" ? status.value : null);
  }, []);

  useEffect(() => {
    void refresh();
    const interval = window.setInterval(() => void refresh(), 3000);
    return () => window.clearInterval(interval);
  }, [refresh]);

  const run: RunAction = async (action) => {
    setBusy(true);
    setError("");
    try {
      await action();
      await refresh();
      return true;
    } catch (reason) {
      setError(String(reason));
      return false;
    } finally {
      setBusy(false);
    }
  };

  const active = data?.creators.filter((creator) => creator.enabled && ["monitoring", "recording"].includes(creator.status)).length;
  const needsTwitch = twitch?.connected !== true;
  const toggleMonitoring = () => {
    if (needsTwitch) {
      setPage("Settings");
      return;
    }
    void run(() => twitchApi.setMonitoring(!twitch.monitoring));
  };

  return <div className="shell">
    <aside className="sidebar">
      <div className="brand"><span className="brand-icon"><Icon name="clips" size={20} /></span><span>ClipForge Live</span></div>
      <nav aria-label="Workspace">
        {navigation.map(({ title, icon }) => <button key={title} type="button" className="nav-button"
          aria-current={page === title ? "page" : undefined} onClick={() => setPage(title)}>
          <Icon name={icon} /><span>{title}</span>
        </button>)}
      </nav>
    </aside>
    <div className="workspace">
      <header className="toolbar">
        <div className="flex items-center gap-2">
          <button type="button" className="button primary" disabled={busy || !data} onClick={toggleMonitoring}>
            <Icon name={twitch?.monitoring ? "pause" : "play"} size={15} />
            {needsTwitch ? "Connect Twitch" : twitch.monitoring ? "Pause monitoring" : "Start monitoring"}
          </button>
          <button type="button" className="button toolbar-link" onClick={() => setPage("Processing Queue")}><Icon name="queue" size={16} />Queue</button>
          <button type="button" className="button toolbar-link" onClick={() => setPage("Publishing History")}><Icon name="upload" size={16} />Publishing</button>
        </div>
        <div className="flex items-center gap-3">
          <Status value={twitch?.monitoring ? "Monitoring enabled" : "Idle"} />
          <button type="button" className="button icon-button" aria-label="Refresh dashboard" onClick={() => void refresh()}><Icon name="refresh" size={16} /></button>
        </div>
      </header>
      <main>
        <div className="page-heading">
          <div><h1>{page}</h1><p>{descriptions[page]}</p></div>
          {data && (page === "Overview" || page === "Creators") &&
            <button className="button" type="button" disabled={busy} onClick={() => setShowAdd(true)}><Icon name="plus" size={17} />Add creator</button>}
        </div>

        {(error || loadError) && <div className="notice" role="alert">
          <span>{error || loadError}</span>
          {error && <button className="button icon-button" type="button" aria-label="Dismiss error" onClick={() => setError("")}><Icon name="close" size={14} /></button>}
        </div>}

        {!data && <section className="panel">
          <Empty icon="drive" title={loadError ? "Local workspace unavailable" : "Opening local workspace"}>
            {loadError ? "Open ClipForge Live from the Windows desktop to access its database." : "Reading settings and saved activity."}
          </Empty>
        </section>}

        {data && <>
          {page === "Overview" && <Overview data={data} active={active ?? 0} twitch={twitch}
            navigate={setPage} add={() => setShowAdd(true)} toggleMonitoring={toggleMonitoring} />}
          {page === "Creators" && <CreatorTable creators={data.creators} busy={busy} run={run} onAdd={() => setShowAdd(true)} />}
          {page === "Live Monitor" && <LiveMonitor data={data} />}
          {page === "Clip Library" && <ClipTable clips={data.clips.filter((clip) => clip.outputPath)} mode="library" automatic={data.settings.autoPublish} />}
          {page === "Processing Queue" && <ClipTable clips={data.clips} mode="queue" />}
          {page === "Publishing History" && <ClipTable clips={data.clips.filter((clip) => clip.publishingStatus || clip.videoId || clip.status === "discarded" || clip.status === "discarding")} mode="publishing" automatic={data.settings.autoPublish} />}
          {page === "Analytics" && <Analytics data={data} />}
          {page === "Settings" && <Preferences data={data} twitch={twitch} busy={busy} run={run} />}
        </>}
      </main>
      <footer className="statusbar">
        <span><i className="status-dot" /> Monitoring {active ?? "—"}</span>
        <span>In queue {data?.queuedCount ?? "—"}</span>
        <span>Published {data?.publishedCount ?? "—"}</span>
        <span className="status-right">Twitch {twitch?.connected ? "connected" : "disconnected"}</span>
      </footer>
    </div>
    {showAdd && <AddCreator busy={busy} run={run} close={() => setShowAdd(false)} />}
  </div>;
}

function Panel({ title, children }: { title: string; children: ReactNode }) {
  return <section className="panel"><header className="panel-heading"><h2>{title}</h2></header>{children}</section>;
}

function Metrics({ values }: { values: [IconName, string, number][] }) {
  return <dl className="metrics">
    {values.map(([icon, name, value]) => <div key={name}><dt><Icon name={icon} />{name}</dt><dd>{value.toLocaleString()}</dd></div>)}
  </dl>;
}

function Overview({ data, active, twitch, navigate, add, toggleMonitoring }: {
  data: Dashboard; active: number; twitch: TwitchStatus | null; navigate: (page: Page) => void; add: () => void; toggleMonitoring: () => void;
}) {
  const [modelReady, setModelReady] = useState(false);
  const [youtubeConnected, setYoutubeConnected] = useState(false);
  useEffect(() => {
    let mounted = true;
    void productionApi.modelReady().then((ready) => { if (mounted) setModelReady(ready); }, () => {});
    void productionApi.youtubeStatus().then((status) => { if (mounted) setYoutubeConnected(status.connected); }, () => {});
    return () => { mounted = false; };
  }, []);
  const events = [
    ...data.highlights.map((item) => ({ id: `highlight-${item.id}`, at: item.triggeredAt, icon: "highlight" as const,
      title: "Highlight detected", summary: `${item.username} · Score ${Math.round(item.score)}` })),
    ...data.errors.map((item) => ({ id: `error-${item.id}`, at: item.occurredAt, icon: "settings" as const,
      title: item.subsystem, summary: item.message })),
    ...data.clips.flatMap((item) => item.publishedAt ? [{ id: `publish-${item.id}`, at: item.publishedAt,
      icon: "upload" as const, title: "Short published", summary: item.title ?? item.username }] : []),
  ].sort((a, b) => (b.at < 1e10 ? b.at * 1000 : b.at) - (a.at < 1e10 ? a.at * 1000 : a.at)).slice(0, 8);

  const setupSteps: [IconName, string, string, () => void, boolean][] = [
    ["monitor", "Connect Twitch", "Authorize ClipForge to read Twitch chat.", () => navigate("Settings"), twitch?.connected === true],
    ["drive", "Install Whisper Large-v3", "Download the local model and media tools.", () => navigate("Settings"), modelReady],
    ["creators", "Add a creator", "Save a Twitch username or channel URL.", add, data.creators.length > 0],
    ["upload", "Connect YouTube", "Authorize the channel for automatic publishing.", () => navigate("Settings"), youtubeConnected],
    ["settings", "Publishing preferences", "Choose clip limits and enable automatic uploads.", () => navigate("Settings"), data.settings.autoPublish],
    ["play", "Start monitoring", "Watch your enabled creators for live activity.", modelReady ? toggleMonitoring : () => navigate("Settings"), twitch?.monitoring === true],
  ];

  return <>
    <Metrics values={[
      ["monitor", "Monitoring", active], ["creators", "Creators", data.creators.length], ["highlight", "Highlights", data.highlightCount],
      ["queue", "In queue", data.queuedCount], ["upload", "Published", data.publishedCount],
    ]} />
    <div className="overview-grid">
      <Panel title="Recent activity">
        {events.length ? <ul className="activity">
          {events.map((event) => <li key={event.id}>
            <Icon name={event.icon} /><div><strong>{event.title}</strong><span>{event.summary}</span></div><Stamp value={event.at} />
          </li>)}
        </ul> : <Empty icon="monitor" title="No activity yet">Detected highlights and publishing updates will appear here.</Empty>}
      </Panel>
      <Panel title="Setup">
        <div className="setup-list">
          {setupSteps.map(([icon, label, detail, action, checked]) => <button type="button" key={label} onClick={action} className="setup-item">
            <span className="setup-icon"><Icon name={icon} size={17} /></span>
            <span><strong>{label}</strong><small>{checked ? "Complete" : detail}</small></span>
            <Icon name="chevron" size={15} />
          </button>)}
        </div>
      </Panel>
    </div>
  </>;
}

function CreatorTable({ creators, busy, run, onAdd }: { creators: Creator[]; busy: boolean; run: RunAction; onAdd: () => void }) {
  return <Panel title="Monitored channels">
    {creators.length ? <div className="table-scroll"><table>
      <thead><tr><th>Creator</th><th>Status</th><th>Last live</th><th>Highlights</th><th>Clips</th><th>Published</th><th>Actions</th></tr></thead>
      <tbody>{creators.map((creator) => <tr key={creator.id}>
        <th scope="row">
          <button type="button" className="text-link" onClick={() => void run(() => openUrl(creator.channelUrl))}>{creator.username} <Icon name="external" size={13} /></button>
          {creator.lastError && <span className="cell-note">{creator.lastError}</span>}
        </th>
        <td><Status value={creator.enabled ? creator.status : "disabled"} /></td>
        <td><Stamp value={creator.lastLiveAt} /></td>
        <td>{creator.highlights}</td><td>{creator.clips}</td><td>{creator.published}</td>
        <td><div className="row-actions">
          <button type="button" className="button small" disabled={busy} aria-label={`${creator.enabled ? "Disable" : "Enable"} ${creator.username}`} onClick={() => void run(() => api.setCreatorEnabled(creator.id, !creator.enabled))}>
            {creator.enabled ? "Disable" : "Enable"}
          </button>
          <button type="button" className="button small icon-button" disabled={busy} aria-label={`Remove ${creator.username}`} onClick={() => void run(() => api.removeCreator(creator.id))}>
            <Icon name="close" size={15} />
          </button>
        </div></td>
      </tr>)}</tbody>
    </table></div> : <Empty icon="creators" title="No creators yet">
      <button className="button" onClick={onAdd}><Icon name="plus" size={15} />Add creator</button>
    </Empty>}
  </Panel>;
}

function LiveMonitor({ data }: { data: Dashboard }) {
  const online = data.creators.filter((creator) => creator.enabled && ["live", "connecting", "monitoring", "recording", "reconnecting", "error"].includes(creator.status));
  return <div className="page-stack">
    <Panel title="Creator activity">
      {online.length ? <ul className="activity live-list">{online.map((creator) => <li key={creator.id}>
        <Icon name="monitor" /><div><strong>{creator.username}</strong><span>{creator.streamTitle ?? creator.lastError}</span></div><Status value={creator.status} />
        <ChatChart samples={data.chatActivity.filter((sample) => sample.creatorId === creator.id)} />
      </li>)}</ul> : <Empty icon="monitor" title="No active streams">Live creator connections will appear here.</Empty>}
    </Panel>
    <Panel title="Highlight detections">
      {data.highlights.length ? <div className="table-scroll"><table>
        <thead><tr><th>Creator</th><th>Detected</th><th>Trigger score</th><th>Publishing confidence</th></tr></thead>
        <tbody>{data.highlights.map((item) => <tr key={item.id}><th scope="row">{item.username}</th><td><Stamp value={item.triggeredAt} /></td><td>{Math.round(item.score)}</td><td>{item.confidence === null ? "—" : Math.round(item.confidence)}</td></tr>)}</tbody>
      </table></div> : <Empty icon="highlight" title="No highlights detected">Detected chat and audio reactions will appear here.</Empty>}
    </Panel>
  </div>;
}

function ChatChart({ samples }: { samples: ChatActivity[] }) {
  if (!samples.length) return null;
  const last = samples[samples.length - 1];
  const peak = Math.max(1, ...samples.map((sample) => sample.messages));
  const firstTime = samples[0].sampledAt;
  const width = Math.max(30, last.sampledAt - firstTime);
  const points = samples.map((sample) => [
    1 + (sample.sampledAt - firstTime) * 598 / width,
    48 - sample.messages * 44 / peak,
  ].join(",")).join(" ");
  return <figure className="chat-chart">
    <figcaption>{last.messages} messages per 5 s · {last.uniqueChatters} unique chatters</figcaption>
    <svg role="img" aria-label="Recent chat messages per five seconds" viewBox="0 0 600 50" preserveAspectRatio="none">
      <path d="M0 48h600" stroke="#393939" />
      <polyline points={points} vectorEffect="non-scaling-stroke" stroke="#d2d2d2" strokeWidth="1.5" fill="none" />
    </svg>
  </figure>;
}

const clipViews = {
  library: { title: "Rendered clips", empty: "No rendered clips", detail: "Finished Shorts will appear here.", icon: "clips" },
  queue: { title: "Clip jobs", empty: "Queue is empty", detail: "Detected highlights will enter the processing queue.", icon: "queue" },
  publishing: { title: "Upload records", empty: "No upload attempts", detail: "YouTube upload results and errors will appear here.", icon: "publishing" },
} as const;

function ClipTable({ clips, mode, automatic = false }: { clips: Clip[]; mode: keyof typeof clipViews; automatic?: boolean }) {
  const view = clipViews[mode];
  const [error, setError] = useState("");
  const [previewId, setPreviewId] = useState<number | null>(null);
  const [cooldown, setCooldown] = useState<number | null>(null);
  useEffect(() => {
    if (mode !== "publishing") return;
    let mounted = true;
    const refresh = () => void productionApi.youtubeStatus().then(
      (status) => { if (mounted) setCooldown(status.uploadRetryAt); }, () => {},
    );
    refresh();
    const timer = window.setInterval(refresh, 15_000);
    return () => { mounted = false; window.clearInterval(timer); };
  }, [mode]);
  const preview = clips.find((clip) => clip.id === previewId);
  return <Panel title={view.title}>
    {mode === "publishing" && cooldown && cooldown * 1000 > Date.now() ? (
      <p className="notice">Channel upload limit. Automatic uploads resume after <Stamp value={cooldown} />.</p>
    ) : null}
    {preview && <ReelReview key={preview.id} clip={preview} automatic={automatic} close={() => setPreviewId(null)} />}
    {error && <p className="notice" role="alert">{error}</p>}
    {clips.length ? <div className="table-scroll"><table>
      <thead><tr><th>Clip</th><th>Creator</th><th>Status</th><th>{mode === "library" ? "Duration" : mode === "queue" ? "Stages" : "YouTube"}</th><th>Created</th></tr></thead>
      <tbody>{clips.map((clip) => <tr key={clip.id}>
        <th scope="row">{mode === "library" && clip.thumbnailPath && <Thumbnail key={clip.thumbnailPath} id={clip.id} />}
          {clip.title ?? `Clip ${clip.id}`}
          {mode === "library" && clip.outputPath && <>
            <button type="button" className="text-link clip-file" onClick={() => setPreviewId(clip.id)}>{automatic ? "Preview" : "Preview & review"}</button>
            {!automatic && clip.reviewDecision && <span className="cell-note">{clip.reviewDecision === "hold" ? "Held for review" : "Approved"}</span>}
          </>}
          {mode === "publishing" && clip.retryAt && clip.retryAt * 1000 > Date.now() ? <span className="cell-note">Retry after <Stamp value={clip.retryAt} /></span> : null}
          {(clip.error || clip.publishingError) && <span className="cell-note">{clip.error ?? clip.publishingError}</span>}
          {mode === "library" && clip.outputPath && <button type="button" className="text-link clip-file"
            onClick={() => void api.showClip(clip.id).catch((reason: unknown) => setError(String(reason)))}>Show in File Explorer <Icon name="external" size={13} /></button>}
        </th>
        <td>{clip.username}</td><td><Status value={mode === "publishing" && !["discarding", "discarded"].includes(clip.status) ? clip.publishingStatus ?? clip.status : clip.status} /></td>
        <td>{mode === "library" ? (clip.durationMs === null ? "—" : `${Math.round(clip.durationMs / 1000)} s`) :
          mode === "queue" ? [clip.transcriptionStatus && `Transcription ${clip.transcriptionStatus}`, clip.renderStatus && `Render ${clip.renderStatus}`, clip.publishingStatus && `Upload ${clip.publishingStatus}`].filter(Boolean).join(" · ") || "—" :
          clip.videoId ? <button type="button" className="text-link" onClick={() => void openUrl(`https://youtu.be/${encodeURIComponent(clip.videoId!)}`).catch((reason: unknown) => setError(String(reason)))}>Watch <Icon name="external" size={14} /></button> :
          clip.uploadedBytes ? <StorageValue bytes={clip.uploadedBytes} /> : "—"}</td>
        <td><Stamp value={mode === "publishing" ? clip.publishedAt ?? clip.createdAt : clip.createdAt} /></td>
      </tr>)}</tbody>
    </table></div> : <Empty icon={view.icon} title={view.empty}>{view.detail}</Empty>}
  </Panel>;
}

function ReelReview({ clip, close, automatic }: { clip: Clip; close: () => void; automatic: boolean }) {
  const video = useRef<HTMLVideoElement>(null);
  const dialog = useRef<HTMLDialogElement>(null);
  const [source, setSource] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const duration = (clip.durationMs ?? 0) / 1000;
  const [start, setStart] = useState(0);
  const [end, setEnd] = useState(duration);

  useEffect(() => { dialog.current?.showModal(); }, []);
  useEffect(() => {
    let active = true;
    setSource("");
    setStart(0);
    setEnd(duration);
    void api.previewClip(clip.id).then(
      (path) => { if (active) setSource(convertFileSrc(path)); },
      (reason: unknown) => { if (active) setError(String(reason)); },
    );
    return () => { active = false; };
  }, [clip.id, clip.outputPath, duration]);

  const run = async (action: () => Promise<void>) => {
    setBusy(true);
    setError("");
    try { await action(); }
    catch (reason) { setError(String(reason)); }
    finally { setBusy(false); }
  };
  const held = clip.reviewDecision === "hold";
  const originalExport = /[\\/]short\.mp4$/i.test(clip.outputPath ?? "");
  const reedited = /[\\/]short-reedited\.mp4$/i.test(clip.outputPath ?? "");
  const rangeValid = start >= 0 && end <= duration && end - start >= 1;
  return <dialog ref={dialog} className="reel-review" onCancel={(event) => { event.preventDefault(); close(); }}>
    <div className="panel-heading"><h2>{automatic ? "Preview clip" : "Review clip"} {clip.id}</h2><button type="button" onClick={close}>Close</button></div>
    {source && <video ref={video} controls playsInline src={source}
      onError={() => setError("ClipForge could not play this export.")}
      onTimeUpdate={() => { if (video.current && video.current.currentTime >= end && end < duration) video.current.pause(); }} />}
    {error && <p className="notice" role="alert">{error}</p>}
    {!automatic && !clip.reviewLocked && <div className="review-controls">
      <div className="form-actions">
        <button type="button" disabled={busy || held} onClick={() => void run(() => api.setClipReview(clip.id, false))}>Hold</button>
        <button type="button" disabled={busy || clip.reviewDecision === "approved"}
          onClick={() => void run(() => api.setClipReview(clip.id, true))}>Approve for publishing</button>
      </div>
      {clip.canReedit && !reedited && <div className="form-actions">
        <button type="button" disabled={busy} onClick={() => void run(() => api.reeditClip(clip.id))}>Re-edit with new captions</button>
      </div>}
      {busy && <p role="status">Working on this reel…</p>}
      {held && originalExport && <>
        <div className="review-trim">
          <label>Start (seconds)<input type="number" min={0} max={duration} step={0.1} value={start} onChange={(event) => setStart(Number(event.target.value))} /></label>
          <label>End (seconds)<input type="number" min={0} max={duration} step={0.1} value={end} onChange={(event) => setEnd(Number(event.target.value))} /></label>
        </div>
        <div className="form-actions">
          <button type="button" disabled={!source || !rangeValid} onClick={() => {
            if (video.current) { video.current.currentTime = start; void video.current.play().catch((reason: unknown) => setError(String(reason))); }
          }}>Play range</button>
          <button type="button" disabled={busy || !rangeValid || (start === 0 && end === duration)}
            onClick={() => void run(() => api.trimClip(clip.id, Math.round(start * 1000), Math.round(end * 1000)))}>Save trim</button>
        </div>
      </>}
    </div>}
  </dialog>;
}

function Thumbnail({ id }: { id: number }) {
  const [source, setSource] = useState<string | null>(null);
  useEffect(() => {
    let mounted = true;
    void api.clipThumbnail(id).then((data) => { if (mounted) setSource(data); }, () => {});
    return () => { mounted = false; };
  }, [id]);
  return source && <img src={source} width={33} height={58} loading="lazy" alt="" className="clip-thumbnail" />;
}

function Analytics({ data }: { data: Dashboard }) {
  return <div className="page-stack">
    <Metrics values={[["creators", "Creators", data.creators.length], ["highlight", "Highlights", data.highlightCount], ["queue", "In queue", data.queuedCount], ["upload", "Published", data.publishedCount]]} />
    <Panel title="Storage usage">
      <div className="storage-summary">
        <dl><div><dt>Saved clips</dt><dd><StorageValue bytes={data.savedBytes} /> / {data.settings.savedBudgetGb} GB</dd></div>
          <div><dt>Recording buffer</dt><dd><StorageValue bytes={data.temporaryBytes} /></dd></div><div><dt>Available disk space</dt><dd>{data.availableBytes === null ? "Unavailable" : <StorageValue bytes={data.availableBytes} />}</dd></div></dl>
        <progress value={Math.min(data.savedBytes, data.settings.savedBudgetGb * 1e9)} max={data.settings.savedBudgetGb * 1e9} aria-label="Saved clip storage usage" />
      </div>
    </Panel>
    <Panel title="Recent errors">
      {data.errors.length ? <ul className="activity">{data.errors.map((entry) => <li key={entry.id}>
        <Icon name="settings" /><div><strong>{entry.subsystem}</strong><span>{entry.message}</span></div><Stamp value={entry.occurredAt} />
      </li>)}</ul> : <Empty icon="analytics" title="No errors reported">Local service errors will appear here.</Empty>}
    </Panel>
  </div>;
}

function AddCreator({ busy, run, close }: { busy: boolean; run: RunAction; close: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [value, setValue] = useState("");
  const [error, setError] = useState("");
  useEffect(() => {
    dialog.current?.showModal();
  }, []);

  async function submit(event: FormEvent) {
    event.preventDefault();
    setError("");
    const saved = await run(async () => {
      try {
        await api.addCreator(value);
      } catch (reason) {
        setError(String(reason));
        throw reason;
      }
    });
    if (saved) close();
  }

  return <dialog ref={dialog} onCancel={(event) => { event.preventDefault(); close(); }} aria-labelledby="add-creator-title">
    <div className="panel-heading"><h2 id="add-creator-title">Add a Twitch creator</h2>
      <button className="button icon-button" type="button" aria-label="Close" onClick={close}><Icon name="close" size={17} /></button>
    </div>
    <form className="settings-form" onSubmit={(event) => void submit(event)}>
      <label>Username or channel URL<input autoFocus value={value} onChange={(event) => setValue(event.target.value)} spellCheck={false} placeholder="twitch.tv/username" required /></label>
      {error && <p role="alert" className="field-error">{error}</p>}
      <div className="dialog-actions"><button type="button" className="button" onClick={close}>Cancel</button>
        <button className="button primary" disabled={busy || !value.trim()}>Add creator</button></div>
    </form>
  </dialog>;
}

export default App;

