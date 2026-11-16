import { openDialog, closeDialog } from "./ui.js";
import { esc } from "./format.js";

let loading;

function loadHls() {
  if (window.Hls) return Promise.resolve(window.Hls);
  if (loading) return loading;
  loading = new Promise((resolve, reject) => {
    const script = document.createElement("script");
    script.src = (window.__ADMIN_BASE__ || "/admin") + "/assets/hls.min.js";
    script.onload = () => resolve(window.Hls);
    script.onerror = () => reject(new Error("无法加载 HLS 播放器"));
    document.head.appendChild(script);
  });
  return loading;
}

export function playUrl(url, title) {
  openDialog({
    title,
    body: `<video id="admin-video" controls autoplay muted playsinline></video><div class="note" id="player-status">正在加载…</div>`,
    footer: `<button class="btn" data-close-dialog>关闭</button>`,
  });
  const video = document.getElementById("admin-video");
  const status = document.getElementById("player-status");
  loadHls().then((Hls) => {
    if (Hls && Hls.isSupported()) {
      const hls = new Hls({ liveDurationInfinity: true, lowLatencyMode: true, liveSyncDurationCount: 3, backBufferLength: 10 });
      hls.on(Hls.Events.ERROR, (_event, data) => {
        if (data.fatal) status.textContent = "播放失败：" + (data.details || "未知错误");
      });
      hls.loadSource(url);
      hls.attachMedia(video);
      status.textContent = "hls.js";
      window.__rtflowHls?.destroy?.();
      window.__rtflowHls = hls;
    } else {
      video.src = url;
      status.textContent = "浏览器原生 HLS";
    }
    video.play().catch(() => {});
  }).catch((error) => { status.textContent = error.message; });
}
