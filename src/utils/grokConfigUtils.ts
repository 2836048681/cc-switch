import { parse as parseToml } from "smol-toml";
import type { GrokSubagentRoute, Provider } from "@/types";
import { normalizeTomlText } from "@/utils/textNormalization";

type TomlObject = Record<string, unknown>;

/** 与后端 `MANAGED_CROSS_PROVIDER_MODEL_PREFIX` 对齐；仅用于 UI 识别，不作所有权判定。 */
export const GROK_MANAGED_CROSS_PROVIDER_MODEL_PREFIX = "ccswitch_xprov_";

/**
 * Grok Build 内置子代理角色（与 `grok inspect` 0.2.99 一致）。
 * 用户仍可输入自定义角色。
 */
export const GROK_DEFAULT_SUBAGENT_ROLES = [
  "general-purpose",
  "explore",
  "plan",
] as const;

function parseGrokConfig(config: string): TomlObject | null {
  try {
    return parseToml(normalizeTomlText(config)) as TomlObject;
  } catch {
    return null;
  }
}

/** 解析 Profile 中全部 `[model.<id>]` 的表名列表。 */
export function extractGrokModelIds(config: string): string[] {
  const parsed = parseGrokConfig(config);
  const modelRoot = parsed?.model as TomlObject | undefined;
  if (!modelRoot || typeof modelRoot !== "object") return [];
  return Object.keys(modelRoot).filter(
    (id) => !id.startsWith(GROK_MANAGED_CROSS_PROVIDER_MODEL_PREFIX),
  );
}

/** 解析 Profile 中 `[subagents.models]` 的 role → modelId 映射。 */
export function extractGrokSubagentModels(
  config: string,
): Record<string, string> {
  const parsed = parseGrokConfig(config);
  const subagents = parsed?.subagents as TomlObject | undefined;
  const models = subagents?.models as TomlObject | undefined;
  if (!models || typeof models !== "object") return {};
  const result: Record<string, string> = {};
  for (const [role, value] of Object.entries(models)) {
    if (typeof value === "string" && value.trim()) {
      result[role] = value.trim();
    }
  }
  return result;
}

/**
 * 将 meta 路由与 TOML 同源路由合并为 UI 编辑状态。
 * meta 优先；TOML 仅补充同源路由（兼容旧数据）。
 */
export function mergeGrokSubagentRoutesForEdit(
  config: string,
  metaRoutes: Record<string, GrokSubagentRoute> | undefined,
  activeProviderId: string | undefined,
): Record<string, GrokSubagentRoute> {
  const fromToml = extractGrokSubagentModels(config);
  const merged: Record<string, GrokSubagentRoute> = {};
  for (const [role, modelId] of Object.entries(fromToml)) {
    if (modelId.startsWith(GROK_MANAGED_CROSS_PROVIDER_MODEL_PREFIX)) {
      // live 物化 ID 不应作为编辑态源；忽略，等待 meta 提供真值
      continue;
    }
    merged[role] = {
      providerId: activeProviderId,
      modelId,
    };
  }
  if (metaRoutes) {
    for (const [role, route] of Object.entries(metaRoutes)) {
      if (!route?.modelId?.trim()) continue;
      merged[role] = {
        providerId: route.providerId?.trim() || activeProviderId,
        modelId: route.modelId.trim(),
      };
    }
  }
  return merged;
}

/**
 * 规范化待保存的路由：
 * - 去掉空 role / 空 modelId
 * - 同源时省略 providerId（或写成当前 id，保存时统一为省略）
 * - 异源必须带 providerId
 */
export function normalizeGrokSubagentRoutesForSave(
  routes: Record<string, GrokSubagentRoute>,
  activeProviderId: string | undefined,
): Record<string, GrokSubagentRoute> {
  const out: Record<string, GrokSubagentRoute> = {};
  for (const [rawRole, route] of Object.entries(routes)) {
    const role = rawRole.trim();
    const modelId = route?.modelId?.trim() ?? "";
    if (!role || !modelId) continue;
    const providerId = route.providerId?.trim() || activeProviderId || "";
    if (!providerId || providerId === activeProviderId) {
      out[role] = { modelId };
    } else {
      out[role] = { providerId, modelId };
    }
  }
  return out;
}

export type GrokSubagentRouteValidationIssue = {
  role: string;
  code:
    | "empty_model"
    | "provider_missing"
    | "model_missing"
    | "official_source";
  message: string;
};

/** 校验跨供应商路由；返回问题列表（空 = 通过）。不泄露密钥。 */
export function validateGrokSubagentRoutes(
  routes: Record<string, GrokSubagentRoute>,
  activeProviderId: string | undefined,
  activeConfig: string,
  allProviders: Provider[],
): GrokSubagentRouteValidationIssue[] {
  const issues: GrokSubagentRouteValidationIssue[] = [];
  const byId = new Map(allProviders.map((p) => [p.id, p]));
  const localModels = new Set(extractGrokModelIds(activeConfig));

  for (const [role, route] of Object.entries(routes)) {
    const modelId = route.modelId?.trim() ?? "";
    if (!modelId) {
      issues.push({
        role,
        code: "empty_model",
        message: `Role "${role}" has an empty model ID`,
      });
      continue;
    }
    const sourceId = route.providerId?.trim() || activeProviderId || "";
    if (!sourceId || sourceId === activeProviderId) {
      if (!localModels.has(modelId)) {
        issues.push({
          role,
          code: "model_missing",
          message: `Role "${role}" refers to missing local model "${modelId}"`,
        });
      }
      continue;
    }
    const source = byId.get(sourceId);
    if (!source) {
      issues.push({
        role,
        code: "provider_missing",
        message: `Role "${role}" refers to missing provider "${sourceId}"`,
      });
      continue;
    }
    if (source.category === "official") {
      issues.push({
        role,
        code: "official_source",
        message: `Role "${role}" cannot route to the official OAuth provider`,
      });
      continue;
    }
    const sourceConfig =
      typeof source.settingsConfig?.config === "string"
        ? source.settingsConfig.config
        : "";
    const sourceModels = extractGrokModelIds(sourceConfig);
    if (!sourceModels.includes(modelId)) {
      issues.push({
        role,
        code: "model_missing",
        message: `Role "${role}" refers to missing model "${modelId}" on provider "${source.name}"`,
      });
    }
  }
  return issues;
}

/**
 * 将同源路由同步进 Profile TOML 的 `[subagents.models]`（异源由 meta 持有）。
 *
 * 对既有段落做定向增删改：存活 role 的赋值行（含行内注释）、独立注释与
 * 邻近无关段落的格式均尽量保留；不再整段删建。
 */
export function setGrokSubagentModelsInConfig(
  config: string,
  routes: Record<string, GrokSubagentRoute>,
  activeProviderId: string | undefined,
): string {
  const sameProvider: Record<string, string> = {};
  for (const [role, route] of Object.entries(routes)) {
    const modelId = route.modelId?.trim();
    if (!modelId) continue;
    const sourceId = route.providerId?.trim() || activeProviderId;
    if (!sourceId || sourceId === activeProviderId) {
      sameProvider[role] = modelId;
    }
  }

  const normalized = normalizeTomlText(config).replace(/\r\n/g, "\n");
  const lines = normalized ? normalized.split("\n") : [];
  const header = "[subagents.models]";
  const sectionStart = lines.findIndex((line) => line.trim() === header);

  if (sectionStart < 0) {
    const entries = Object.entries(sameProvider);
    if (entries.length === 0) {
      return normalized.endsWith("\n") || !normalized
        ? normalized
        : `${normalized}\n`;
    }
    const block = [
      header,
      ...entries.map(
        ([role, modelId]) => `${role} = ${JSON.stringify(modelId)}`,
      ),
      "",
    ].join("\n");
    const prefix = normalized.trimEnd();
    return `${prefix}${prefix ? "\n\n" : ""}${block}`;
  }

  let sectionEnd = lines.length;
  for (let i = sectionStart + 1; i < lines.length; i += 1) {
    if (/^\s*\[[^\]]+\]\s*$/.test(lines[i])) {
      sectionEnd = i;
      break;
    }
  }

  const remaining = { ...sameProvider };
  const nextBody: string[] = [];
  for (let i = sectionStart + 1; i < sectionEnd; i += 1) {
    const line = lines[i];
    const keyMatch = line.match(/^\s*([A-Za-z0-9_-]+)\s*=/);
    if (!keyMatch) {
      // 独立注释、空行、非 role 赋值：原样保留
      nextBody.push(line);
      continue;
    }
    const role = keyMatch[1];
    if (Object.prototype.hasOwnProperty.call(remaining, role)) {
      const modelId = remaining[role];
      delete remaining[role];
      nextBody.push(updateTomlStringAssignment(line, role, modelId));
    }
    // 不在同源 map 中的 role（含旧异源残留）删除赋值行
  }

  for (const [role, modelId] of Object.entries(remaining)) {
    nextBody.push(`${role} = ${JSON.stringify(modelId)}`);
  }

  const hasAssignments = nextBody.some((line) =>
    /^\s*[A-Za-z0-9_-]+\s*=/.test(line),
  );
  if (!hasAssignments) {
    const before = lines.slice(0, sectionStart);
    const after = lines.slice(sectionEnd);
    // 去掉段落后多余空行
    while (before.length > 0 && before[before.length - 1] === "") {
      before.pop();
    }
    while (after.length > 0 && after[0] === "") {
      after.shift();
    }
    const joined = [
      ...before,
      ...(before.length && after.length ? [""] : []),
      ...after,
    ]
      .join("\n")
      .trimEnd();
    return joined ? `${joined}\n` : "";
  }

  const nextLines = [
    ...lines.slice(0, sectionStart),
    header,
    ...nextBody,
    ...lines.slice(sectionEnd),
  ];
  return `${nextLines.join("\n").trimEnd()}\n`;
}

/** 更新 `key = "value"` 赋值，尽量保留行内尾注释与缩进。 */
function updateTomlStringAssignment(
  line: string,
  key: string,
  nextValue: string,
): string {
  const escapedKey = key.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const re = new RegExp(
    `^(\\s*${escapedKey}\\s*=\\s*)(?:"(?:\\\\.|[^"\\\\])*"|'(?:\\\\.|[^'\\\\])*'|[^\\s#]+)(.*)$`,
  );
  const match = line.match(re);
  if (match) {
    return `${match[1]}${JSON.stringify(nextValue)}${match[2]}`;
  }
  return `${key} = ${JSON.stringify(nextValue)}`;
}

export function extractGrokBaseUrl(config: string): string {
  const parsed = parseGrokConfig(config);
  const endpoints = parsed?.endpoints as TomlObject | undefined;
  if (typeof endpoints?.models_base_url === "string") {
    return endpoints.models_base_url;
  }
  const models = parsed?.models as TomlObject | undefined;
  const modelRoot = parsed?.model as TomlObject | undefined;
  const selected =
    typeof models?.default === "string" ? models.default : undefined;
  const selectedModel = selected
    ? (modelRoot?.[selected] as TomlObject | undefined)
    : undefined;
  return typeof selectedModel?.base_url === "string"
    ? selectedModel.base_url
    : "";
}

export function extractGrokApiBackend(config: string): string | undefined {
  const parsed = parseGrokConfig(config);
  const models = parsed?.models as TomlObject | undefined;
  const modelRoot = parsed?.model as TomlObject | undefined;
  const selected =
    typeof models?.default === "string" ? models.default : undefined;
  const selectedModel = selected
    ? (modelRoot?.[selected] as TomlObject | undefined)
    : undefined;
  return typeof selectedModel?.api_backend === "string"
    ? selectedModel.api_backend
    : undefined;
}

function setSectionString(
  config: string,
  section: string,
  key: string,
  nextValue: string,
): string {
  const normalized = normalizeTomlText(config).replace(/\r\n/g, "\n");
  const lines = normalized ? normalized.split("\n") : [];
  const sectionHeader = `[${section}]`;
  const sectionStart = lines.findIndex((line) => line.trim() === sectionHeader);
  const assignment = `${key} = ${JSON.stringify(nextValue)}`;

  if (sectionStart < 0) {
    if (!nextValue) return normalized;
    const prefix = normalized.trimEnd();
    return `${prefix}${prefix ? "\n\n" : ""}${sectionHeader}\n${assignment}\n`;
  }

  let sectionEnd = lines.length;
  for (let index = sectionStart + 1; index < lines.length; index += 1) {
    if (/^\s*\[[^\]]+\]\s*$/.test(lines[index])) {
      sectionEnd = index;
      break;
    }
  }
  const keyPattern = new RegExp(`^\\s*${key}\\s*=`);
  const keyIndex = lines
    .slice(sectionStart + 1, sectionEnd)
    .findIndex((line) => keyPattern.test(line));
  if (keyIndex >= 0) {
    const absoluteIndex = sectionStart + 1 + keyIndex;
    if (nextValue) lines[absoluteIndex] = assignment;
    else lines.splice(absoluteIndex, 1);
  } else if (nextValue) {
    lines.splice(sectionStart + 1, 0, assignment);
  }
  return `${lines.join("\n").trimEnd()}\n`;
}

export function setGrokBaseUrl(config: string, baseUrl: string): string {
  const nextBaseUrl = baseUrl.trim();
  const withEndpoint = setSectionString(
    config,
    "endpoints",
    "models_base_url",
    nextBaseUrl,
  );
  const lines = withEndpoint.replace(/\r\n/g, "\n").split("\n");
  const starts: number[] = [];
  lines.forEach((line, index) => {
    if (/^\s*\[model\.[^\]]+\]\s*$/.test(line)) starts.push(index);
  });
  for (let offset = starts.length - 1; offset >= 0; offset -= 1) {
    const start = starts[offset];
    const nextSection = lines.findIndex(
      (line, index) => index > start && /^\s*\[[^\]]+\]\s*$/.test(line),
    );
    const end = nextSection < 0 ? lines.length : nextSection;
    const keyIndex = lines
      .slice(start + 1, end)
      .findIndex((line) => /^\s*base_url\s*=/.test(line));
    if (keyIndex >= 0) {
      if (nextBaseUrl) {
        lines[start + 1 + keyIndex] =
          `base_url = ${JSON.stringify(nextBaseUrl)}`;
      } else {
        lines.splice(start + 1 + keyIndex, 1);
      }
    } else if (nextBaseUrl) {
      lines.splice(start + 1, 0, `base_url = ${JSON.stringify(nextBaseUrl)}`);
    }
  }
  return `${lines.join("\n").trimEnd()}\n`;
}

export function setGrokApiBackend(
  config: string,
  backend: "responses" | "chat_completions",
): string {
  const lines = normalizeTomlText(config).replace(/\r\n/g, "\n").split("\n");
  const starts: number[] = [];
  lines.forEach((line, index) => {
    if (/^\s*\[model\.[^\]]+\]\s*$/.test(line)) starts.push(index);
  });
  for (let offset = starts.length - 1; offset >= 0; offset -= 1) {
    const start = starts[offset];
    const end =
      lines.findIndex(
        (line, index) => index > start && /^\s*\[[^\]]+\]\s*$/.test(line),
      ) || lines.length;
    const actualEnd = end < 0 ? lines.length : end;
    const keyIndex = lines
      .slice(start + 1, actualEnd)
      .findIndex((line) => /^\s*api_backend\s*=/.test(line));
    const assignment = `api_backend = ${JSON.stringify(backend)}`;
    if (keyIndex >= 0) lines[start + 1 + keyIndex] = assignment;
    else lines.splice(start + 1, 0, assignment);
  }
  return `${lines.join("\n").trimEnd()}\n`;
}
