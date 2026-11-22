import type { ReactNode } from "react";
import { Icon, type IconName } from "./Icon";

export function Empty({ icon, title, children }: { icon: IconName; title: string; children: ReactNode }) {
  return <div className="empty">
    <span className="empty-icon"><Icon name={icon} size={22} /></span>
    <strong>{title}</strong>
    <p>{children}</p>
  </div>;
}

export function Stamp({ value }: { value: number | null }) {
  if (!value) return <span className="muted">—</span>;
  const date = new Date(value < 10_000_000_000 ? value * 1000 : value);
  return <time dateTime={date.toISOString()}>{date.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" })}</time>;
}

export function Status({ value }: { value: string }) {
  const current = value.toLowerCase();
  return <span className="status" data-status={current}>
    <span className="status-dot" />
    {current === "idle" ? "Idle" : current.replaceAll("_", " ")}
  </span>;
}

export function StorageValue({ bytes }: { bytes: number }) {
  if (bytes === 0) return <>0 B</>;
  const scale = ["B", "KB", "MB", "GB", "TB"];
  const unit = Math.min(scale.length - 1, Math.floor(Math.log10(bytes) / 3));
  return <>{new Intl.NumberFormat(undefined, { maximumFractionDigits: 2 }).format(bytes / 1000 ** unit)} {scale[unit]}</>;
}
