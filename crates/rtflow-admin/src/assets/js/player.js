import { openDialog, closeDialog } from "./ui.js";

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

export function playUrl(url, title, options = {}) {
  let hls = null;
  let video = null;
  let closed = false;
  openDialog({
    title,
    body: `<video id="admin-video" controls autoplay muted playsinline></video><div class="player-status-row"><span class="note" id="player-status">正在加载…</span><button class="btn small ghost" id="player-retry" hidden>重试</button></div>`,
    footer: `<button class="btn" data-close-dialog>关闭</button>`,
    onClose() {
      closed = true;
      hls?.destroy?.();
      if (window.__rtflowHls === hls) window.__rtflowHls = null;
      if (video) {
        video.pause();
        video.removeAttribute("src");
        try { video.load(); } catch (_) {}
      }
    },
  });
  video = document.getElementById("admin-video");
  const status = document.getElementById("player-status");
  const retry = document.getElementById("player-retry");
  const start = () => {
    if (closed) return;
    hls?.destroy?.();
    hls = null;
    status.textContent = "正在加载…";
    retry.hidden = true;
    loadHls().then((Hls) => {
      if (closed) return;
      if (Hls && Hls.isSupported()) {
        const vod = options.vod === true;
        hls = new Hls({
          liveDurationInfinity: !vod,
          lowLatencyMode: !vod,
          liveSyncDurationCount: 3,
          backBufferLength: 10,
          startPosition: vod ? 0 : -1,
        });
        hls.on(Hls.Events.ERROR, (_event, data) => {
          if (data.fatal) {
            status.textContent = "播放失败：" + (data.details || "未知错误") + (data.reason ? " (" + data.reason + ")" : "");
            retry.hidden = false;
          }
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
    }).catch((error) => { status.textContent = error.message; retry.hidden = false; });
  };
  retry.onclick = start;
  start();
}
