import { describe, expect, it } from "vitest";
import type { GrokSubagentRoute, Provider } from "@/types";
import {
  extractGrokModelIds,
  extractGrokSubagentModels,
  GROK_DEFAULT_SUBAGENT_ROLES,
  GROK_MANAGED_CROSS_PROVIDER_MODEL_PREFIX,
  mergeGrokSubagentRoutesForEdit,
  normalizeGrokSubagentRoutesForSave,
  setGrokSubagentModelsInConfig,
  validateGrokSubagentRoutes,
} from "@/utils/grokConfigUtils";

const localConfig = `[endpoints]
models_base_url = "https://api.x.ai/v1"

[models]
default = "fast"
web_search = "search"

[subagents.models]
explore = "fast"
plan = "search"

[model.fast]
model = "grok-4.5"
base_url = "https://api.x.ai/v1"
api_backend = "responses"

[model.search]
model = "grok-4.5"
base_url = "https://api.x.ai/v1"
api_backend = "responses"
`;

const foreignConfig = `[endpoints]
models_base_url = "https://foreign.example/v1"

[models]
default = "cli"

[model.cli]
model = "gpt-proxy"
base_url = "https://foreign.example/v1"
api_backend = "chat_completions"
`;

function makeProvider(
  id: string,
  name: string,
  config: string,
  category = "third_party",
): Provider {
  return {
    id,
    name,
    category: category as Provider["category"],
    settingsConfig: {
      auth: { OPENAI_API_KEY: `${id}-secret-key` },
      config,
    },
  };
}

describe("Grok cross-provider subagent routes (frontend)", () => {
  it("suggests only actual Grok Build built-in agent roles", () => {
    expect([...GROK_DEFAULT_SUBAGENT_ROLES]).toEqual([
      "general-purpose",
      "explore",
      "plan",
    ]);
  });

  it("extracts same-provider models and ignores managed prefix entries", () => {
    const withManaged = `${localConfig}
[model.${GROK_MANAGED_CROSS_PROVIDER_MODEL_PREFIX}other__cli]
model = "should-ignore"
`;
    expect(extractGrokModelIds(withManaged)).toEqual(["fast", "search"]);
    expect(extractGrokSubagentModels(localConfig)).toEqual({
      explore: "fast",
      plan: "search",
    });
  });

  it("merges TOML same-provider routes with meta foreign routes", () => {
    const meta: Record<string, GrokSubagentRoute> = {
      plan: { providerId: "foreign", modelId: "cli" },
    };
    const merged = mergeGrokSubagentRoutesForEdit(localConfig, meta, "xai");
    expect(merged.explore).toEqual({ providerId: "xai", modelId: "fast" });
    expect(merged.plan).toEqual({ providerId: "foreign", modelId: "cli" });
  });

  it("normalizes same-provider routes without providerId for save", () => {
    const normalized = normalizeGrokSubagentRoutesForSave(
      {
        explore: { providerId: "xai", modelId: "fast" },
        plan: { providerId: "foreign", modelId: "cli" },
      },
      "xai",
    );
    expect(normalized.explore).toEqual({ modelId: "fast" });
    expect(normalized.plan).toEqual({
      providerId: "foreign",
      modelId: "cli",
    });
  });

  it("writes only same-provider routes into Profile TOML", () => {
    const next = setGrokSubagentModelsInConfig(
      localConfig,
      {
        explore: { modelId: "search" },
        plan: { providerId: "foreign", modelId: "cli" },
      },
      "xai",
    );
    const models = extractGrokSubagentModels(next);
    expect(models).toEqual({ explore: "search" });
    expect(next).not.toContain("foreign");
    expect(next).not.toContain("cli");
    expect(next).not.toContain("secret");
  });

  it("preserves inline and standalone comments for surviving [subagents.models] routes", () => {
    const withComments = `[endpoints]
models_base_url = "https://api.x.ai/v1"

# keep nearby section comment
[models]
default = "fast"

[subagents.models]
# standalone: explore routing
explore = "fast" # inline: explore uses fast
# standalone: plan routing
plan = "search" # inline: plan uses search
# standalone: general-purpose
general-purpose = "fast"

[model.fast]
model = "grok-4.5"
# model table comment must survive
`;
    // plan 改为异源 → 仅 meta；explore 更新值；general-purpose 保留
    const next = setGrokSubagentModelsInConfig(
      withComments,
      {
        explore: { modelId: "search" },
        plan: { providerId: "foreign", modelId: "cli" },
        "general-purpose": { modelId: "fast" },
      },
      "xai",
    );

    expect(extractGrokSubagentModels(next)).toEqual({
      explore: "search",
      "general-purpose": "fast",
    });
    // 存活 route 的独立注释与行内注释保留
    expect(next).toContain("# standalone: explore routing");
    expect(next).toMatch(/explore\s*=\s*"search".*# inline: explore uses fast/);
    expect(next).toContain("# standalone: general-purpose");
    expect(next).toMatch(/general-purpose\s*=\s*"fast"/);
    // 邻近无关段落注释与格式
    expect(next).toContain("# keep nearby section comment");
    expect(next).toContain("# model table comment must survive");
    expect(next).toContain('[models]\ndefault = "fast"');
    // 异源 route 不得写入 TOML
    expect(next).not.toMatch(/^\s*plan\s*=/m);
    expect(next).not.toContain("foreign");
    expect(next).not.toContain('"cli"');
  });

  it("removes [subagents.models] section when no same-provider routes remain", () => {
    const next = setGrokSubagentModelsInConfig(
      localConfig,
      {
        explore: { providerId: "foreign", modelId: "cli" },
      },
      "xai",
    );
    expect(next).not.toContain("[subagents.models]");
    expect(next).toContain("[models]");
    expect(next).toContain("[model.fast]");
    expect(extractGrokSubagentModels(next)).toEqual({});
  });

  it("adds new same-provider roles without rewriting unrelated sections", () => {
    const sparse = `[models]
default = "fast"

[subagents.models]
explore = "fast" # keep

[model.fast]
model = "grok-4.5"
`;
    const next = setGrokSubagentModelsInConfig(
      sparse,
      {
        explore: { modelId: "fast" },
        plan: { modelId: "fast" },
      },
      "xai",
    );
    expect(extractGrokSubagentModels(next)).toEqual({
      explore: "fast",
      plan: "fast",
    });
    expect(next).toMatch(/explore\s*=\s*"fast".*# keep/);
    expect(next).toContain('plan = "fast"');
    expect(next).toContain('[models]\ndefault = "fast"');
  });

  it("validates foreign provider/model presence without leaking secrets", () => {
    const providers = [
      makeProvider("xai", "xAI", localConfig),
      makeProvider("foreign", "Foreign", foreignConfig),
    ];
    const ok = validateGrokSubagentRoutes(
      {
        explore: { modelId: "fast" },
        plan: { providerId: "foreign", modelId: "cli" },
      },
      "xai",
      localConfig,
      providers,
    );
    expect(ok).toEqual([]);

    const missingProvider = validateGrokSubagentRoutes(
      { explore: { providerId: "gone", modelId: "fast" } },
      "xai",
      localConfig,
      providers,
    );
    expect(missingProvider[0]?.code).toBe("provider_missing");
    expect(JSON.stringify(missingProvider)).not.toContain("secret");

    // 绝不能把 foreign 缺失的模型静默当成本地同名模型
    const missingForeignModel = validateGrokSubagentRoutes(
      { explore: { providerId: "foreign", modelId: "fast" } },
      "xai",
      localConfig,
      providers,
    );
    expect(missingForeignModel[0]?.code).toBe("model_missing");
    // 使用供应商显示名（Foreign），不得回退为本地同名模型
    expect(missingForeignModel[0]?.message).toMatch(/Foreign|foreign/);
  });

  it("rejects official OAuth as a foreign source", () => {
    const providers = [
      makeProvider("xai", "xAI", localConfig),
      makeProvider("official", "Official", "", "official"),
    ];
    const issues = validateGrokSubagentRoutes(
      { explore: { providerId: "official", modelId: "anything" } },
      "xai",
      localConfig,
      providers,
    );
    expect(issues[0]?.code).toBe("official_source");
  });

  it("ignores managed live IDs when building edit state from TOML", () => {
    const liveLike = `[subagents.models]
explore = "${GROK_MANAGED_CROSS_PROVIDER_MODEL_PREFIX}foreign__cli"
plan = "search"

[model.fast]
model = "grok-4.5"

[model.search]
model = "grok-4.5"
`;
    const merged = mergeGrokSubagentRoutesForEdit(
      liveLike,
      {
        explore: { providerId: "foreign", modelId: "cli" },
      },
      "xai",
    );
    expect(merged.explore).toEqual({
      providerId: "foreign",
      modelId: "cli",
    });
    expect(merged.plan).toEqual({ providerId: "xai", modelId: "search" });
  });
});
