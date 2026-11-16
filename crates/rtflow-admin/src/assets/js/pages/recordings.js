import { api } from "../api.js";
import { bytes, empty, esc, num } from "../format.js";
import { toast } from "../ui.js";

export async function render(_params, ctx) {
  const recordings = await api("/recordings");
  return {
    title: "录制",
    subtitle: "当前录制任务；按路径的录制开关在“路径”中设置。",
    actions: `<button class="btn small" data-action="refresh">刷新</button>`,
    html: `<section class="panel">${recordings.length ? `<div class="table-wrap"><table class="data"><thead><tr><th>路径</th><th>状态</th><th>分段</th><th>大小</th><th>错误</th><th></th></tr></thead><tbody>
      ${recordings.map((item) => `<tr><td class="mono">${esc(item.path)}</td><td>${item.active ? '<span class="badge ok">录制中</span>' : '<span class="badge">已停止</span>'}</td>
        <td class="mono">${num(item.segments)}</td><td class="mono">${bytes(item.bytes)}</td><td class="cell-sub">${esc(item.last_error || "—")}</td>
        <td><div class="row-actions"><button class="btn small" data-action="${item.active ? "stop" : "start"}" data-path="${esc(item.path)}">${item.active ? "停止" : "开始"}</button></div></td></tr>`).join("")}
    </tbody></table></div>` : empty("当前没有录制任务", "在路径中开启录制，或等待发布者上线")}</section>`,
    mount(root) {
      root.querySelector("[data-action=refresh]").onclick = () => ctx.reload();
      root.addEventListener("click", async (event) => {
        const target = event.target.closest("[data-action]");
        if (!target || !["start", "stop"].includes(target.dataset.action)) return;
        try { await api(`/recordings/${target.dataset.action}`, { method: "POST", body: { path: target.dataset.path } }); toast("操作已提交", "ok"); ctx.reload(); }
        catch (error) { toast(error.message, "bad"); }
      });
      const timer = setInterval(() => ctx.reload({ keepFocus: true }), 2000);
      return () => clearInterval(timer);
    },
  };
}
