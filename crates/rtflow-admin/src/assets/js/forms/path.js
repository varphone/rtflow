import { openDrawer, field, textInput, checkbox, toast } from "../ui.js";
import { apiV1 } from "../api.js";
import { esc } from "../format.js";

export function openPathDrawer(existing, onSaved, options = {}) {
  const clone = options.clone === true && Boolean(existing);
  const source = existing || { name: "", enabled: true, publish: true, read: true, record: false, hls: true, dash: true, flv: true, webrtc: true, max_readers: 0, tenant: null };
  const item = clone ? { ...source, name: `${source.name}-copy` } : source;
  const editing = Boolean(existing) && !clone;
  openDrawer({
    title: clone ? "复制路径" : (existing ? "编辑路径" : "新建路径"),
    description: "路径策略、输出协议与访问限制。",
    body: `
      <div class="form-section">
        <h3>基础</h3>
        ${field("路径名", textInput("name", item.name, `required ${editing ? "disabled" : ""} placeholder="live/cam"`), { hint: "由字母、数字、- _ . ~ / 组成，不能以 / 开头或结尾。" })}
        ${checkbox("enabled", "启用该路径", item.enabled !== false)}
      </div>
      <div class="form-section">
        <h3>允许的操作</h3>
        <div style="display:flex;flex-wrap:wrap;gap:10px 16px">
          ${checkbox("publish", "发布", item.publish)}
          ${checkbox("read", "观看", item.read)}
          ${checkbox("record", "录制", item.record)}
          ${checkbox("hls", "HLS", item.hls)}
          ${checkbox("dash", "DASH", item.dash)}
          ${checkbox("flv", "HTTP-FLV", item.flv)}
          ${checkbox("webrtc", "WebRTC", item.webrtc)}
        </div>
      </div>
      <div class="form-section">
        <h3>限制与归属</h3>
        <div class="field-row">
          ${field("最大观看数", `<input class="input" type="number" min="0" name="max_readers" value="${Number(item.max_readers || 0)}">`, { hint: "0 表示不限制。" })}
          ${field("租户", textInput("tenant", item.tenant || "", "placeholder=\"可选\""))}
        </div>
      </div>`,
    submitLabel: editing ? "保存修改" : (clone ? "创建副本" : "创建路径"),
    async onSubmit(data, { close }) {
      const body = {
        enabled: data.get("enabled") === "on",
        publish: data.get("publish") === "on",
        read: data.get("read") === "on",
        record: data.get("record") === "on",
        hls: data.get("hls") === "on",
        dash: data.get("dash") === "on",
        flv: data.get("flv") === "on",
        webrtc: data.get("webrtc") === "on",
        max_readers: Number(data.get("max_readers") || 0),
        tenant: data.get("tenant") || null,
      };
      if (editing) {
        await apiV1("/paths/" + encodeURIComponent(item.name), { method: "PATCH", body });
      } else {
        await apiV1("/paths", { method: "POST", body: Object.assign({ name: data.get("name") }, body) });
      }
      close();
      toast(editing ? "路径已保存" : "路径已创建", "ok");
      onSaved();
    },
  });
}
