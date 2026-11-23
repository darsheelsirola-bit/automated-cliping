import { invoke } from "@tauri-apps/api/core";

export type Creator = {
  id: number;
  username: string;
  channelUrl: string;
  enabled: boolean;
  status: string;
  streamTitle: string | null;
  lastLiveAt: number | null;
  lastError: string | null;
  highlights: number;
  clips: number;
  published: number;
};

export type Settings = {
  storageDirectory: string;
  savedBudgetGb: number;
  minClipSeconds: number;
  maxClipSeconds: number;
  minimumConfidence: number;
  detectionThreshold: number;
  autoPublish: boolean;
  monitoringEnabled: boolean;
  startWithWindows: boolean;
  keepAwake: boolean;
};

export type Highlight = {
  id: number;
  username: string;
  triggeredAt: number;
  score: number;
  confidence: number | null;
};

export type Clip = {
  id: number;
  username: string;
  status: string;
  durationMs: number | null;
  createdAt: number;
  outputPath: string | null;
  thumbnailPath: string | null;
  error: string | null;
  transcriptionStatus: string | null;
  renderStatus: string | null;
  publishingStatus: string | null;
  publishingError: string | null;
  uploadedBytes: number | null;
  videoId: string | null;
  title: string | null;
  publishedAt: number | null;
  reviewDecision: "hold" | "approved" | null;
  editorialPass: boolean | null;
  editorialNotes: string | null;
  retryAt: number | null;
  reviewLocked: boolean;
  canReedit: boolean;
};

export type ErrorEntry = {
  id: number;
  subsystem: string;
  message: string;
  occurredAt: number;
};

export type ChatActivity = { creatorId: number; sampledAt: number; messages: number; uniqueChatters: number };

export type Dashboard = {
  creators: Creator[];
  settings: Settings;
  highlights: Highlight[];
  chatActivity: ChatActivity[];
  clips: Clip[];
  errors: ErrorEntry[];
  highlightCount: number;
  queuedCount: number;
  publishedCount: number;
  savedBytes: number;
  temporaryBytes: number;
  availableBytes: number | null;
};

export const api = {
  dashboard: () => invoke<Dashboard>("dashboard"),
  addCreator: (value: string) => invoke<void>("add_creator", { value }),
  setCreatorEnabled: (id: number, enabled: boolean) => invoke<void>("set_creator_enabled", { id, enabled }),
  removeCreator: (id: number) => invoke<void>("remove_creator", { id }),
  showClip: (id: number) => invoke<void>("show_clip", { id }),
  reeditClip: (id: number) => invoke<void>("reedit_clip", { id }),
  previewClip: (id: number) => invoke<string>("preview_clip", { id }),
  setClipReview: (id: number, approved: boolean) => invoke<void>("set_clip_review", { id, approved }),
  trimClip: (id: number, startMs: number, endMs: number) => invoke<void>("trim_clip", { id, startMs, endMs }),
  clipThumbnail: (id: number) => invoke<string | null>("clip_thumbnail", { id }),
  saveSettings: (settings: Settings) => invoke<void>("save_settings", { settings }),
};
export type TwitchStatus = { clientId: string | null; connected: boolean; monitoring: boolean };
export type TwitchVerification = { userCode: string; verificationUri: string };

export const twitchApi = {
  status: () => invoke<TwitchStatus>("twitch_status"),
  begin: (clientId: string) => invoke<TwitchVerification>("twitch_begin", { clientId }),
  finish: () => invoke<void>("twitch_finish"),
  setMonitoring: (monitoring: boolean) => invoke<void>("set_monitoring", { monitoring }),
};


export type YouTubeStatus = { connected: boolean; channelTitle: string | null; uploadRetryAt: number | null };

export const productionApi = {
  modelReady: () => invoke<boolean>("model_ready"),
  modelProgress: () => invoke<{ ready: boolean; weightBytes: number; totalWeightBytes: number }>("model_progress"),
  setupModel: () => invoke<void>("setup_model"),
  youtubeStatus: () => invoke<YouTubeStatus>("youtube_status"),
  youtubeConnect: (clientJson: string) => invoke<void>("youtube_connect", { clientJson }),
};
