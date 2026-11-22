const designs = {
  overview: "M3 10l9-7 9 7v10a1 1 0 0 1-1 1h-5v-7H9v7H4a1 1 0 0 1-1-1z",
  creators: "M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2m20 0v-2a4 4 0 0 0-3-4M13 7a4 4 0 1 1-8 0 4 4 0 0 1 8 0zm6 4a4 4 0 0 0 0-8",
  monitor: "M12 12h.01M8 8a5.7 5.7 0 0 0 0 8m8-8a5.7 5.7 0 0 1 0 8M5 5a10 10 0 0 0 0 14M19 5a10 10 0 0 1 0 14",
  clips: "M5 4h14a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2zm5 4 6 4-6 4z",
  queue: "m12 3 10 6-10 6L2 9zm-9 10 9 5 9-5M3 17l9 5 9-5",
  publishing: "M12 8v5l3 2M3 12a9 9 0 1 1 2.7 6.4M3 4v8h7",
  analytics: "M4 20v-5m5 5V9m6 11v-8m5 8V4",
  settings: "M9 3h6l.5 3 2 .9 2.8-1.1 2 3.4-2.3 2v2.6l2.3 2-2 3.4-2.8-1.1-2 .9-.5 3H9l-.5-3-2-.9-2.8 1.1-2-3.4 2.3-2v-2.6l-2.3-2 2-3.4 2.8 1.1 2-.9zM15 12a3 3 0 1 1-6 0 3 3 0 0 1 6 0z",
  highlight: "M5 3h14v18H5zM5 8h14M5 16h14M9 3v18m6-18v18",
  upload: "M12 15V3m-5 5 5-5 5 5M4 14v5a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-5",
  chevron: "m9 5 7 7-7 7",
  plus: "M12 5v14m-7-7h14",
  play: "m8 5 12 7-12 7z",
  pause: "M9 5v14m6-14v14",
  close: "m6 6 12 12m0-12L6 18",
  refresh: "M20 9a8.5 8.5 0 0 0-14.5-3.5L3 8m0-5v5h5m-4 7a8.5 8.5 0 0 0 14.5 3.5L21 16m0 5v-5h-5",
  drive: "M4 14 7 5h10l3 9M4 14v5h16v-5zm3 2.5h.01",
  external: "M14 5h5v5m0-5-8 8m-2-6H5v12h12v-4",
} as const;

export type IconName = keyof typeof designs;

export function Icon({ name, size = 18 }: { name: IconName; size?: number }) {
  return <svg aria-hidden="true" width={size} height={size} viewBox="0 0 24 24" fill="none"
    stroke="currentColor" strokeWidth="1.65" strokeLinecap="round" strokeLinejoin="round">
    <path d={designs[name]} />
  </svg>;
}
