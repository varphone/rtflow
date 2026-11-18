import { openDialog, closeDialog } from "./ui.js";

let hlsLoading;
let dashLoading;

function loadHls() {
  if (window.Hls) return Promise.resolve(window.Hls);
  if (hlsLoading) return hlsLoading;
  hlsLoading = new Promise((resolve, reject) => {
    const script = document.createElement("script");
    script.src = (window.__ADMIN_BASE__ || "/admin") + "/assets/hls.min.js";
    script.onload = () => resolve(window.Hls);
    script.onerror = () => reject(new Error("无法加载 HLS 播放器"));
    document.head.appendChild(script);
  });
  return hlsLoading;
}

function loadDash() {
  if (window.dashjs) return Promise.resolve(window.dashjs);
  if (dashLoading) return dashLoading;
  dashLoading = new Promise((resolve, reject) => {
    const script = document.createElement("script");
    script.src = (window.__ADMIN_BASE__ || "/admin") + "/assets/dash.all.min.js";
    script.onload = () => resolve(window.dashjs);
    script.onerror = () => reject(new Error("无法加载 DASH 播放器"));
    document.head.appendChild(script);
  });
  return dashLoading;
}

export function playUrl(url, title, options = {}) {
  let hls = null;
  let dashPlayer = null;
  let video = null;
  let closed = false;
  const isDash = options.dash === true || /\.mpd(?:$|\?)/i.test(url);
  openDialog({
    title,
    body: `<video id="admin-video" controls autoplay muted playsinline></video><div class="player-status-row"><span class="note" id="player-status">正在加载…</span><button class="btn small ghost" id="player-retry" hidden>重试</button></div>`,
    footer: `<button class="btn" data-close-dialog>关闭</button>`,
    onClose() {
      closed = true;
      hls?.destroy?.();
      dashPlayer?.reset?.();
      if (window.__rtflowHls === hls) window.__rtflowHls = null;
      if (window.__rtflowDash === dashPlayer) window.__rtflowDash = null;
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
    dashPlayer?.reset?.();
    dashPlayer = null;
    status.textContent = "正在加载…";
    retry.hidden = true;
    if (isDash) {
      loadDash().then((dashjs) => {
        if (closed) return;
        dashPlayer = dashjs.MediaPlayer().create();
        dashPlayer.updateSettings({
          streaming: {
            delay: { liveDelay: options.vod === true ? 0 : 4 },
            buffer: { fastSwitchEnabled: true, bufferTimeAtTopQuality: 8 },
            liveCatchup: { enabled: options.vod !== true },
          },
        });
        dashPlayer.on(dashjs.MediaPlayer.events.ERROR, (event) => {
          status.textContent = "播放失败：" + (event?.error?.message || event?.error || "未知错误");
          retry.hidden = false;
        });
        dashPlayer.initialize(video, url, true);
        status.textContent = "正在播放";
        status.title = "dash.js";
        window.__rtflowDash?.reset?.();
        window.__rtflowDash = dashPlayer;
        video.play().catch(() => {});
      }).catch((error) => { status.textContent = error.message; retry.hidden = false; });
      return;
    }
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
        status.textContent = "正在播放";
        status.title = "hls.js";
        window.__rtflowHls?.destroy?.();
        window.__rtflowHls = hls;
      } else {
        video.src = url;
        status.textContent = "正在播放";
        status.title = "浏览器原生 HLS";
      }
      video.play().catch(() => {});
    }).catch((error) => { status.textContent = error.message; retry.hidden = false; });
  };
  retry.onclick = start;
  start();
}
