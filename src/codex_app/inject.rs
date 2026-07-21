//! Codex.app renderer 注入脚本生成器（React fiber state 注入路线）。
//!
//! ⚠️ 机制由 cx 自行通过 CDP（Chrome DevTools Protocol）逆向 Codex.app（149.x）renderer 得出：
//! 模型选择器**不是独立按钮**，而是 reasoning effort 下拉里的一个子菜单项；
//! 模型列表由 React 组件的 props/state 持有（含 `model` 字段的对象数组），下拉打开时才挂载到 DOM。
//! 此 App 为 ESM 打包（无 webpack），无 `sendRequest`/`list-models-for-host` 协议。
//!
//! 因此注入点选在 **React fiber 对象图**：
//! 1. 用 MutationObserver 监听 DOM，每当弹层（`[role=menu]/[role=listbox]/[data-radix-popper-content-wrapper]`）
//!    出现（用户点开下拉）就立即重跑 patch；并配兜底定时器周期重跑。
//! 2. patch：从 `window.__codexRoot._internalRoot.current` fiber 根**整树遍历**（沿 child/sibling），对每个
//!    fiber 的 `memoizedProps`/`memoizedState` 深度遍历对象图（depth≤5）——CDP 实测：models 数组在树里可达，
//!    但不在打开弹层的 DOM 节点 fiber 子树下，故必须整树遍历而非只扫弹层 DOM。
//!    对任何含 `models`/`data`/`availableModels`/`available_models` 字段的对象，把模型数组**原地替换**为
//!    我们的自定义模型（splice 清空原生、填入自定义，保留数组引用）。CDP 实测子菜单不按 hidden 过滤、
//!    且疑似只取前几项，故必须替换而非「追加+隐藏」。react 下次渲染即显示。
//! 3. 默认模型：改 Statsig dynamic config（`_memoCache` 里含 `new_thread_model` / `available_models` 的条目，
//!    实测 config key `c|107580212`），并 hook `getDynamicConfig` 防止服务端刷新覆盖。
//!
//! 模型描述符字段取自 CDP 实测的 GPT-5.5 描述符结构（含 slug/name/serviceTiers/supportedReasoningEfforts 等）。
//! cx 启动时已知模型清单，直接内联烘焙进脚本（无需 bridge HTTP）。
//!
//! 已知限制：外部 CDP 注入改的是 React 已渲染的底层数组，无法干净触发 re-render，
//! 故子菜单**首帧**可能仍显示陈旧模型，用户交互一次（切换/重开）后即刷新为自定义模型。

use crate::ResolvedModel;
use serde_json::json;

/// reasoning effort 集合（与实测 GPT-5.5 描述符一致的 4 档）。每档结构 `{reasoningEffort, description}`。
const REASONING_EFFORTS: &[(&str, &str)] = &[
    ("low", "Fast responses with lighter reasoning"),
    (
        "medium",
        "Balances speed and reasoning depth for everyday tasks",
    ),
    ("high", "Greater reasoning depth for complex problems"),
    ("xhigh", "Extra high reasoning depth for complex problems"),
];

/// 生成完整注入脚本。`default_reasoning_effort` 应与 config.toml 的 `model_reasoning_effort` 一致。
pub fn build_injection_script(models: &[ResolvedModel], default_reasoning_effort: &str) -> String {
    let default_model = models.first().map(|m| m.id.clone()).unwrap_or_default();
    let descriptors = model_descriptors(models, &default_model);
    let models_json = serde_json::to_string(&descriptors).unwrap_or_else(|_| "[]".into());
    let names: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    let names_json = serde_json::to_string(&names).unwrap_or_else(|_| "[]".into());
    let default_json = serde_json::to_string(&default_model).unwrap_or_else(|_| "\"\"".into());
    let effort_json =
        serde_json::to_string(default_reasoning_effort).unwrap_or_else(|_| "\"medium\"".into());

    format!(
        "window.__cxModels = {models_json};\n\
         window.__cxModelNames = {names_json};\n\
         window.__cxDefaultModel = {default_json};\n\
         window.__cxDefaultEffort = {effort_json};\n\
         {PATCH_BODY}"
    )
}

/// 构建 Codex renderer 期望的完整模型描述符（字段取自 CDP 实测的 GPT-5.5 描述符结构）。
fn model_descriptors(models: &[ResolvedModel], default_model: &str) -> serde_json::Value {
    let efforts: Vec<_> = REASONING_EFFORTS
        .iter()
        .map(|(e, d)| json!({ "reasoningEffort": e, "description": d }))
        .collect();
    let arr: Vec<_> = models
        .iter()
        .map(|m| {
            json!({
                "model": m.id,
                "id": m.id,
                "slug": m.id,
                "name": m.id,
                "displayName": m.id,
                "description": if m.desc.is_empty() { "Custom model".to_string() } else { m.desc.clone() },
                "hidden": false,
                "isDefault": m.id == default_model,
                "upgrade": null,
                "upgradeInfo": null,
                "availabilityNux": null,
                "defaultReasoningEffort": "medium",
                "supportedReasoningEfforts": efforts,
                "inputModalities": ["text"],
                "supportsPersonality": false,
                "additionalSpeedTiers": [],
                "serviceTiers": [],
                "defaultServiceTier": null,
            })
        })
        .collect();
    serde_json::Value::Array(arr)
}

/// 注入脚本主体：fiber 对象图整树替换 models 数组 + Statsig config patch + MutationObserver 重跑。
const PATCH_BODY: &str = r#"(function () {
  if (window.__cxModelPatchInstalled === true) return;
  const CX_MODELS = window.__cxModels || [];
  const CX_NAMES = window.__cxModelNames || [];
  const CX_DEFAULT = window.__cxDefaultModel || (CX_NAMES[0]) || "";
  if (!CX_MODELS.length) return;
  window.__cxModelPatchInstalled = true;

  function descriptorFor(name) {
    for (const m of CX_MODELS) if (m.model === name || m.id === name) return m;
    return null;
  }

  // 一个数组是否「看起来是模型描述符数组」（元素含 string model 字段）。
  function modelArrayPatchable(v, allowEmpty) {
    return Array.isArray(v) && (allowEmpty || v.length > 0) &&
      v.every(function (x) { return x && typeof x === "object" && typeof x.model === "string"; });
  }
  // 把一个模型描述符数组**原地替换**为我们的模型列表（保留同一数组引用，使 React 持有的引用仍指向它）。
  // CDP 实测：子菜单 UI 渲染时不按 `hidden` 过滤，且似乎只取数组前几项——因此「追加+隐藏」无效，
  // 必须清空原生、只填自定义模型（用 splice 原地改，不能换引用）。
  function patchModelArray(models, allowEmpty) {
    if (!modelArrayPatchable(models, allowEmpty)) return false;
    // 已经是「仅我们的模型」就跳过（避免重复 splice 触发无谓变更）。
    const already = models.length === CX_MODELS.length &&
      models.every(function (it) { return CX_NAMES.indexOf(it.model) >= 0; });
    if (already) return false;
    const replacement = CX_MODELS.map(function (m) { return m; });
    Array.prototype.splice.apply(models, [0, models.length].concat(replacement));
    return true;
  }
  // 字符串名数组（available_models 等）。
  function patchNameArray(arr) {
    if (!Array.isArray(arr) || !arr.every(function (x) { return typeof x === "string"; })) return false;
    let changed = false;
    CX_NAMES.forEach(function (n) { if (arr.indexOf(n) < 0) { arr.push(n); changed = true; } });
    return changed;
  }
  // 对一个对象的常见模型字段做 patch。
  function patchModelContainer(v) {
    if (!v || typeof v !== "object") return false;
    let changed = false;
    if (patchModelArray(v.models, ("defaultModel" in v) || ("availableModels" in v))) changed = true;
    if (patchNameArray(v.models)) changed = true;
    if (patchModelArray(v.data)) changed = true;
    if (patchModelArray(v.result)) changed = true;
    if (v.pages && patchModelArray(v.pages[0] && v.pages[0].data)) changed = true;
    if (v.result && patchModelArray(v.result.data)) changed = true;
    if (v.result && patchModelArray(v.result.models)) changed = true;
    for (const f of ["availableModels", "available_models"]) {
      const av = v[f];
      if (av instanceof Set) { CX_NAMES.forEach(function (n) { if (!av.has(n)) { av.add(n); changed = true; } }); }
      else if (Array.isArray(av)) { if (patchNameArray(av)) changed = true; }
    }
    return changed;
  }
  // 深度遍历对象图（depth≤5），对每个对象尝试 patchModelContainer。
  function walkGraph(root, visited, depth) {
    if (!root || typeof root !== "object" || visited.has(root) || depth > 5) return false;
    visited.add(root);
    let changed = patchModelContainer(root);
    if (root instanceof Element || root === window || root === document) return changed;
    let keys;
    try { keys = Object.keys(root); } catch (e) { return changed; }
    for (const k of keys) {
      if (k === "ownerDocument" || k === "parentElement" || k === "parentNode" || k === "children" || k === "childNodes") continue;
      let v;
      try { v = root[k]; } catch (e) { continue; }
      if (v && typeof v === "object" && walkGraph(v, visited, depth + 1)) changed = true;
    }
    return changed;
  }
  // 遍历整棵 fiber 树（从 __codexRoot 根沿 child/sibling），对每个 fiber 的
  // memoizedProps/memoizedState 做 walkGraph。CDP 实测：models 数组在树里可达，
  // 但不在打开弹层的 DOM 节点 fiber 子树下，故必须从 fiber 根整树遍历，而非只扫弹层 DOM。
  function patchReactModelState() {
    const root = window.__codexRoot &&
      window.__codexRoot._internalRoot &&
      window.__codexRoot._internalRoot.current;
    if (!root) return false;
    const visited = new WeakSet();
    let changed = false;
    let count = 0;
    function walkFiber(f, depth) {
      if (!f || depth <= 0 || count > 60000) return;
      count++;
      for (const pn of ["memoizedProps", "memoizedState"]) {
        let st;
        try { st = f[pn]; } catch (e) { continue; }
        if (st && typeof st === "object") {
          try { if (walkGraph(st, visited, 0)) changed = true; } catch (e) {}
        }
      }
      try { walkFiber(f.child, depth - 1); } catch (e) {}
      try { walkFiber(f.sibling, depth - 1); } catch (e) {}
    }
    walkFiber(root, 1000);
    return changed;
  }

  // Statsig dynamic config patch。CDP 实测：模型选择器**子菜单**的可选模型列表来自
  // Statsig config `c|107580212` 的 `available_models`（字符串数组）+ `default_model` + `use_hidden_models`，
  // 而非 React props.models（那个只喂主菜单/trigger）。因此必须同时 patch：
  //   - `available_models` ← 我们的模型名（替换原生 GPT，使子菜单只列自定义模型）
  //   - `default_model` ← 默认模型
  //   - `use_hidden_models` ← false
  //   - `new_thread_model` / `new_thread_reasoning_effort`（新对话默认）
  // 并 hook `getDynamicConfig`：服务端刷新/重新请求会返回原始 config，hook 保证每次取用都被覆盖。
  function patchStatsigConfigValue(val) {
    if (!val || typeof val !== "object") return;
    try {
      if (Object.prototype.hasOwnProperty.call(val, "available_models")) {
        val.available_models = CX_NAMES.slice();
        val.default_model = CX_DEFAULT;
        if (Object.prototype.hasOwnProperty.call(val, "use_hidden_models")) val.use_hidden_models = false;
      }
      if (Object.prototype.hasOwnProperty.call(val, "new_thread_model")) {
        val.new_thread_model = CX_DEFAULT;
        if (window.__cxDefaultEffort) val.new_thread_reasoning_effort = window.__cxDefaultEffort;
      }
    } catch (e) {}
  }

  function statsigClients() {
    const root = window.__STATSIG__ || (typeof globalThis !== "undefined" && globalThis.__STATSIG__);
    if (!root || typeof root !== "object") return [];
    const list = [root.firstInstance, typeof root.instance === "function" ? root.instance() : null];
    if (root.instances && typeof root.instances === "object") {
      for (const k of Object.keys(root.instances)) list.push(root.instances[k]);
    }
    return list.filter(function (c, i, a) { return c && typeof c === "object" && a.indexOf(c) === i; });
  }

  function patchStatsig() {
    statsigClients().forEach(function (c) {
      // hook getDynamicConfig（幂等），让每次取 config 都被覆盖。
      if (typeof c.getDynamicConfig === "function" && !c.__cxStatsigHook) {
        try {
          const orig = c.getDynamicConfig.bind(c);
          c.getDynamicConfig = function (name, options) {
            const cfg = orig(name, options);
            if (cfg) patchStatsigConfigValue(cfg.value || cfg.__value);
            return cfg;
          };
          c.__cxStatsigHook = true;
        } catch (e) {}
      }
      // 主动取一次已知的模型 config，触发覆盖（含 disableExposureLog 变体）。
      if (typeof c.getDynamicConfig === "function") {
        try { const cfg = c.getDynamicConfig("107580212", { disableExposureLog: true }); if (cfg) patchStatsigConfigValue(cfg.value || cfg.__value); } catch (e) {}
      }
      // 直接覆盖 memoCache 里所有含模型字段的 config（已缓存的条目 hook 不一定再经过）。
      try {
        const cache = c._memoCache;
        if (cache) {
          for (const k of Object.keys(cache)) {
            let entry; try { entry = cache[k]; } catch (e) { continue; }
            const val = entry && (entry.value || entry.__value);
            if (val && typeof val === "object" &&
                (Object.prototype.hasOwnProperty.call(val, "available_models") ||
                 Object.prototype.hasOwnProperty.call(val, "new_thread_model"))) {
              patchStatsigConfigValue(val);
            }
          }
        }
      } catch (e) {}
    });
  }

  function runPass() {
    try { patchReactModelState(); } catch (e) {}
    try { patchStatsig(); } catch (e) {}
  }

  // 立即跑一次。
  runPass();

  // MutationObserver：弹层（下拉/菜单）出现时重跑 patch——这是模型出现在 reasoning 子菜单的关键。
  try {
    const sel = "[role='menu'], [role='dialog'], [role='listbox'], [data-radix-popper-content-wrapper]";
    let pending = false;
    const obs = new MutationObserver(function (mutations) {
      const relevant = mutations.some(function (m) {
        return Array.prototype.some.call(m.addedNodes, function (n) {
          return n.nodeType === 1 && (n.matches && n.matches(sel) || n.querySelector && n.querySelector(sel));
        });
      });
      if (relevant) {
        // 弹层（下拉/子菜单）刚挂载：立即同步 patch，并在随后两帧补跑，
        // 尽量赶在子菜单首帧渲染前把底层数组改好，减少「首帧显示陈旧模型」。
        runPass();
        if (!pending) {
          pending = true;
          requestAnimationFrame(function () { runPass(); });
          setTimeout(function () { pending = false; runPass(); }, 60);
        }
      }
    });
    obs.observe(document.body, { childList: true, subtree: true });
  } catch (e) {}

  // 兜底定时器：前 ~30s 周期性重跑（覆盖首屏挂载、Statsig 刷新覆盖等）。
  let attempts = 0;
  const timer = setInterval(function () {
    attempts++;
    runPass();
    if (attempts > 30) clearInterval(timer);
  }, 1000);
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CopilotAuth, ResolvedModel, WireApi};
    use std::collections::BTreeMap;

    fn rm(id: &str) -> ResolvedModel {
        ResolvedModel {
            id: id.into(),
            desc: String::new(),
            wire_api: WireApi::Responses,
            model_wire_apis: vec![WireApi::Responses],
            provider_name: "DashScope".into(),
            endpoint_url: "https://example.com/v1".into(),
            visible_agents: vec!["Codex.app".into()],
            copilot_auth: CopilotAuth::ApiKey,
            env: BTreeMap::new(),
            apikey_source: None,
            max_output_tokens: None,
            max_tokens: None,
            supports_tools: true,
            supports_images: false,
        }
    }

    #[test]
    fn injection_script_bakes_models_and_default() {
        let models = vec![rm("qwen3.6-plus"), rm("qwen3.7-max")];
        let script = build_injection_script(&models, "high");
        assert!(script.contains("window.__cxModels = "));
        assert!(script.contains("window.__cxModelNames = "));
        assert!(script.contains("\"qwen3.6-plus\""));
        assert!(script.contains("window.__cxDefaultModel = \"qwen3.6-plus\""));
        assert!(script.contains("window.__cxDefaultEffort = \"high\""));
        // fiber 注入 + MutationObserver + Statsig（available_models + 默认模型）
        assert!(script.contains("__codexRoot"));
        assert!(script.contains("walkFiber"));
        assert!(script.contains("MutationObserver"));
        assert!(script.contains("available_models"));
        assert!(script.contains("new_thread_model"));
        assert!(script.contains("getDynamicConfig"));
        // 描述符字段
        assert!(script.contains("\"slug\""));
        assert!(script.contains("supportedReasoningEfforts"));
        assert!(script.contains("\"isDefault\":true"));
    }

    #[test]
    fn injection_script_empty_models_is_safe() {
        let script = build_injection_script(&[], "medium");
        assert!(script.contains("window.__cxModels = []"));
        assert!(script.contains("window.__cxDefaultModel = \"\""));
    }
}
