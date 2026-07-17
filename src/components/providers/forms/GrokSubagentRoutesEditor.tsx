import { useMemo } from "react";
import { useTranslation } from "react-i18next";
import { Plus, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type { GrokSubagentRoute, Provider } from "@/types";
import {
  extractGrokModelIds,
  GROK_DEFAULT_SUBAGENT_ROLES,
} from "@/utils/grokConfigUtils";

export interface GrokSubagentRoutesEditorProps {
  /** 当前正在编辑的供应商 ID（新建时可能为空） */
  activeProviderId?: string;
  /** 当前 Profile TOML，用于解析本供应商模型列表 */
  activeConfig: string;
  routes: Record<string, GrokSubagentRoute>;
  onChange: (routes: Record<string, GrokSubagentRoute>) => void;
  /** 全部 Grok 供应商（含当前），用于跨供应商选择 */
  allProviders: Provider[];
  disabled?: boolean;
}

type Row = {
  role: string;
  providerId: string;
  modelId: string;
};

const LOCAL_VALUE = "__local__";

export function GrokSubagentRoutesEditor({
  activeProviderId,
  activeConfig,
  routes,
  onChange,
  allProviders,
  disabled = false,
}: GrokSubagentRoutesEditorProps) {
  const { t } = useTranslation();

  const localModelIds = useMemo(
    () => extractGrokModelIds(activeConfig),
    [activeConfig],
  );

  const selectableProviders = useMemo(
    () =>
      allProviders.filter(
        (p) =>
          p.category !== "official" &&
          p.id !== activeProviderId &&
          typeof p.settingsConfig?.config === "string" &&
          p.settingsConfig.config.trim().length > 0,
      ),
    [allProviders, activeProviderId],
  );

  const rows: Row[] = useMemo(() => {
    const entries = Object.entries(routes);
    if (entries.length === 0) return [];
    return entries.map(([role, route]) => ({
      role,
      providerId: route.providerId?.trim() || activeProviderId || LOCAL_VALUE,
      modelId: route.modelId ?? "",
    }));
  }, [routes, activeProviderId]);

  const emit = (nextRows: Row[]) => {
    const next: Record<string, GrokSubagentRoute> = {};
    for (const row of nextRows) {
      const role = row.role.trim();
      if (!role) continue;
      const isLocal =
        !row.providerId ||
        row.providerId === LOCAL_VALUE ||
        row.providerId === activeProviderId;
      next[role] = isLocal
        ? { modelId: row.modelId }
        : { providerId: row.providerId, modelId: row.modelId };
    }
    onChange(next);
  };

  const updateRow = (index: number, patch: Partial<Row>) => {
    const next = rows.map((row, i) =>
      i === index ? { ...row, ...patch } : row,
    );
    emit(next);
  };

  const removeRow = (index: number) => {
    emit(rows.filter((_, i) => i !== index));
  };

  const addRow = () => {
    const used = new Set(rows.map((r) => r.role));
    const defaultRole =
      GROK_DEFAULT_SUBAGENT_ROLES.find((role) => !used.has(role)) ??
      `role_${rows.length + 1}`;
    const defaultModel = localModelIds[0] ?? "";
    emit([
      ...rows,
      {
        role: defaultRole,
        providerId: activeProviderId || LOCAL_VALUE,
        modelId: defaultModel,
      },
    ]);
  };

  const modelsForProvider = (providerId: string): string[] => {
    if (
      !providerId ||
      providerId === LOCAL_VALUE ||
      providerId === activeProviderId
    ) {
      return localModelIds;
    }
    const provider = allProviders.find((p) => p.id === providerId);
    const config =
      typeof provider?.settingsConfig?.config === "string"
        ? provider.settingsConfig.config
        : "";
    return extractGrokModelIds(config);
  };

  const providerLabel = (providerId: string) => {
    if (
      !providerId ||
      providerId === LOCAL_VALUE ||
      providerId === activeProviderId
    ) {
      return t("grokConfig.subagentRouteLocal", {
        defaultValue: "当前供应商",
      });
    }
    return allProviders.find((p) => p.id === providerId)?.name ?? providerId;
  };

  return (
    <div className="space-y-2 rounded-md border border-border p-3">
      <div className="flex items-center justify-between gap-2">
        <div>
          <p className="text-sm font-medium text-foreground">
            {t("grokConfig.subagentRoutesTitle", {
              defaultValue: "子代理模型路由",
            })}
          </p>
          <p className="text-xs text-muted-foreground">
            {t("grokConfig.subagentRoutesHint", {
              defaultValue:
                "可为每个子代理角色指定本供应商或其它已保存 Grok 供应商中的模型。跨供应商路由会在切换时写入 live config，不会覆盖用户自建模型。",
            })}
          </p>
        </div>
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={addRow}
          disabled={disabled}
        >
          <Plus className="mr-1 h-3.5 w-3.5" />
          {t("grokConfig.subagentRouteAdd", { defaultValue: "添加路由" })}
        </Button>
      </div>

      {rows.length === 0 ? (
        <p className="text-xs text-muted-foreground">
          {t("grokConfig.subagentRoutesEmpty", {
            defaultValue:
              "未配置显式路由时，仍可使用 Profile TOML 中的 [subagents.models]。",
          })}
        </p>
      ) : (
        <div className="space-y-2">
          {rows.map((row, index) => {
            const modelOptions = modelsForProvider(row.providerId);
            return (
              <div
                key={`${row.role}-${index}`}
                className="grid grid-cols-1 gap-2 sm:grid-cols-[minmax(0,1fr)_minmax(0,1.2fr)_minmax(0,1.2fr)_auto] sm:items-center"
              >
                <Input
                  value={row.role}
                  disabled={disabled}
                  placeholder={t("grokConfig.subagentRolePlaceholder", {
                    defaultValue: "角色，如 explore",
                  })}
                  onChange={(e) => updateRow(index, { role: e.target.value })}
                  className="h-8 text-sm"
                />
                <Select
                  value={
                    row.providerId === activeProviderId
                      ? LOCAL_VALUE
                      : row.providerId || LOCAL_VALUE
                  }
                  disabled={disabled}
                  onValueChange={(value) => {
                    const nextProviderId =
                      value === LOCAL_VALUE
                        ? activeProviderId || LOCAL_VALUE
                        : value;
                    const models = modelsForProvider(nextProviderId);
                    updateRow(index, {
                      providerId: nextProviderId,
                      modelId: models.includes(row.modelId)
                        ? row.modelId
                        : (models[0] ?? ""),
                    });
                  }}
                >
                  <SelectTrigger className="h-8 text-sm">
                    <SelectValue placeholder={providerLabel(row.providerId)} />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value={LOCAL_VALUE}>
                      {t("grokConfig.subagentRouteLocal", {
                        defaultValue: "当前供应商",
                      })}
                    </SelectItem>
                    {selectableProviders.map((p) => (
                      <SelectItem key={p.id} value={p.id}>
                        {p.name}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <Select
                  value={row.modelId || undefined}
                  disabled={disabled || modelOptions.length === 0}
                  onValueChange={(value) =>
                    updateRow(index, { modelId: value })
                  }
                >
                  <SelectTrigger className="h-8 text-sm">
                    <SelectValue
                      placeholder={t("grokConfig.subagentModelPlaceholder", {
                        defaultValue: "选择模型",
                      })}
                    />
                  </SelectTrigger>
                  <SelectContent>
                    {modelOptions.map((id) => (
                      <SelectItem key={id} value={id}>
                        {id}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  className="h-8 w-8 text-muted-foreground hover:text-destructive"
                  disabled={disabled}
                  onClick={() => removeRow(index)}
                  aria-label={t("common.delete")}
                >
                  <Trash2 className="h-4 w-4" />
                </Button>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
