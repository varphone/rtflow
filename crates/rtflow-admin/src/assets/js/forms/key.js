import { openDrawer, field, textInput, toast } from "../ui.js";
import { apiV1 } from "../api.js";

export function openKeyDrawer(onIssued, defaults = null) {
  const source = defaults || {};
  const roles = new Set(source.roles || ["publish", "read"]);
  openDrawer({
    title: defaults ? "重新签发推流密钥" : "签发推流密钥",
    description: "密钥只在创建后显示一次，服务端仅保存哈希。",
    body: `
      <div class="form-section">
        <h3>标识</h3>
        ${field("名称", textInput("name", source.name ? `${source.name} 副本` : "", `required placeholder="obs"`))}
        ${field("有效期", textInput("ttl", "24h", `required placeholder="24h"`), { hint: "支持 30m、24h、7d 等格式。" })}
      </div>
      <div class="form-section">
        <h3>权限</h3>
        <label class="checkline"><input type="checkbox" name="publish" ${roles.has("publish") ? "checked" : ""}> 发布</label>
        <label class="checkline"><input type="checkbox" name="read" ${roles.has("read") ? "checked" : ""}> 观看</label>
        ${field("协议", textInput("protocols", (source.protocols || []).join(","), "placeholder=\"rtmp,srt（留空全部）\""))}
        ${field("路径范围", textInput("paths", (source.paths || []).join(","), "placeholder=\"live/*（留空全部）\""))}
        ${field("最大连接数", `<input class="input" type="number" min="0" name="max_connections" value="${source.max_connections || 0}">`, { hint: "0 表示不限制。" })}
      </div>`,
    submitLabel: "签发",
    async onSubmit(data, { close }) {
      const list = (value) => String(value || "").split(",").map((part) => part.trim()).filter(Boolean);
      const roles = [];
      if (data.get("publish") === "on") roles.push("publish");
      if (data.get("read") === "on") roles.push("read");
      const result = await apiV1("/keys", { method: "POST", body: {
        name: data.get("name"), roles,
        protocols: list(data.get("protocols")), paths: list(data.get("paths")),
        ttl: data.get("ttl") || "24h",
        max_connections: Number(data.get("max_connections") || 0),
      } });
      close();
      onIssued(result);
    },
  });
}
