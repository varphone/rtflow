export const esc = (value) => String(value ?? "")
  .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
  .replace(/"/g, "&quot;").replace(/'/g, "&#39;");

export const num = (value) => Number(value || 0).toLocaleString("zh-CN");

export function bytes(value) {
  let n = Number(value || 0);
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

export function uptime(seconds) {
  const s = Math.max(0, Number(seconds || 0));
  const d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60);
  if (d) return `${d} 天 ${h} 小时`;
  if (h) return `${h} 小时 ${m} 分`;
  return `${m} 分钟`;
}

export const isoTime = (unix) => unix ? new Date(Number(unix) * 1000).toISOString() : "";
export function dateTime(unix) {
  if (!unix) return "—";
  const date = new Date(Number(unix) * 1000);
  if ((localStorage.getItem("rtflow-time") || "absolute") !== "relative") return date.toLocaleString("zh-CN");
  const seconds = Math.max(0, (Date.now() - date.getTime()) / 1000);
  if (seconds < 45) return "刚刚";
  if (seconds < 3600) return `${Math.floor(seconds / 60)} 分钟前`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} 小时前`;
  if (seconds < 7 * 86400) return `${Math.floor(seconds / 86400)} 天前`;
  return date.toLocaleDateString("zh-CN");
}
export const resolution = (track) => track && track.width && track.height ? `${track.width}×${track.height}` : "—";
export const fps = (track) => track && track.frame_rate ? `${track.frame_rate} fps` : "";
export function duration(ms) {
  const total = Math.max(0, Math.floor(Number(ms || 0) / 1000));
  const h = Math.floor(total / 3600), m = Math.floor((total % 3600) / 60), s = total % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}` : `${m}:${String(s).padStart(2, "0")}`;
}
export function audioLabel(track) {
  if (!track) return "—";
  return `${track.codec}${track.channels ? " · " + track.channels + "ch" : ""}${track.sample_rate ? " · " + Math.round(track.sample_rate / 1000) + "kHz" : ""}`;
}

export function statusBadge(stream) {
  return stream.publisher ? `<span class="badge ok">● 直播中</span>` : `<span class="badge">待机</span>`;
}

export function stateBadge(state) {
  const cls = state === "streaming" ? "ok" : state === "backoff" ? "warn" : state === "idle" ? "" : "codec";
  return `<span class="badge ${cls}">${esc(state || "—")}</span>`;
}

export function empty(title, hint, icon = "inbox", action = "") {
  return `<div class="empty">${iconSvg(icon)}<h3>${esc(title)}</h3><div class="faint">${esc(hint)}</div>${action ? `<div class="actions" style="justify-content:center;margin-top:14px">${action}</div>` : ""}</div>`;
}

export function iconSvg(name) {
  const paths = {
    inbox: '<path d="M4 4h16v16H4z"/><path d="M4 13h4l2 3h4l2-3h4"/>',
    play: '<path d="M8 5v14l11-7z"/>',
    live: '<path d="M4 6h10v12H4z"/><path d="M14 10l6-3v10l-6-3"/>',
    vod: '<rect x="3" y="5" width="18" height="14" rx="2"/><path d="M10 9l5 3-5 3z"/>',
    paths: '<path d="M4 6h7v5H4zM13 13h7v5h-7z"/><path d="M11 8h4v7"/>',
    keys: '<circle cx="8" cy="15" r="4"/><path d="M11 12l8-8M16 4l4 4M14 6l3 3"/>',
    proxy: '<path d="M4 7h16v10H4z"/><path d="M8 11h8"/>',
    srt: '<path d="M4 12h16M8 8l-4 4 4 4M16 8l4 4-4 4"/>',
    record: '<circle cx="12" cy="12" r="8"/><circle cx="12" cy="12" r="3" fill="currentColor"/>',
    audit: '<path d="M5 4h14v16H5z"/><path d="M8 8h8M8 12h8M8 16h5"/>',
    search: '<circle cx="11" cy="11" r="7"/><path d="M20 20l-3.5-3.5"/>',
    copy: '<rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V5a2 2 0 0 1 2-2h8"/>',
    refresh: '<path d="M20 11a8 8 0 1 0-2.3 5.7"/><path d="M20 4v7h-7"/>',
    plus: '<path d="M12 5v14M5 12h14"/>',
    more: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
    edit: '<path d="M4 20h4L20 8l-4-4L4 16z"/>',
    trash: '<path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13"/>',
    power: '<path d="M12 3v9"/><path d="M6.4 6.4a8 8 0 1 0 11.2 0"/>',
  };
  return `<svg class="icon" viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">${paths[name] || paths.inbox}</svg>`;
}


export function exportCsv(filename, header, rows) {
  const quote = (value) => `"${String(value ?? "").replace(/"/g, '""')}"`;
  const lines = [header.map(quote).join(","), ...rows.map((row) => row.map(quote).join(","))];
  const blob = new Blob(["\uFEFF" + lines.join("\r\n")], { type: "text/csv;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = filename;
  document.body.appendChild(link);
  link.click();
  link.remove();
  URL.revokeObjectURL(url);
}
