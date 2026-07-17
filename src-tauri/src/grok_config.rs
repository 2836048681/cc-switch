//! Grok Build 配置文件管理。
//!
//! Grok Build 将供应商连接信息保存在 `config.toml` 的 `[endpoints]`、
//! `[models]`、`[subagents.models]` 与 `[model.*]` 中。CC Switch 只重写这些
//! 供应商字段，UI、MCP、遥测等其它配置始终原样保留。
//!
//! 跨供应商子代理路由：`ProviderMeta.grokSubagentRoutes` 指向其它 Profile 的
//! `[model.*]`。写入 live 时物化为带 `ccswitch_xprov_` 前缀的 managed 模型定义，
//! 并把 `[subagents.models]` 指到这些 ID。
//!
//! **所有权不写入 `[model.<id>]` 字段。** 官方 xAI settings 文档只列出有限的
//! model 字段；未知键（如 `cc_switch_owned`）虽可被当前 `grok inspect` 接受，
//! 但不保证未来兼容。CC Switch 把所有权放在独立注册表
//! `[cc_switch.managed_xprov.<managed_id>]`（toml_edit 保留注释）。
//! 清理只依据注册表（及遗留内联标记迁移），**绝不**仅凭前缀推断所有权。
//!
//! Managed ID 使用 `sha2` 对 `provider\\0model` 做稳定摘要，避免 `a/b` 与
//! `a_b` 这类 sanitize 碰撞。

use crate::config::{get_home_dir, write_text_file};
use crate::error::AppError;
use crate::provider::{GrokSubagentRoute, Provider};
use indexmap::IndexMap;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::{value, DocumentMut, Item, Table};

pub const CC_SWITCH_GROK_MODEL_ID: &str = "ccswitch";
pub const GROK_PROXY_TOKEN_PLACEHOLDER: &str = "PROXY_MANAGED";

/// Managed 跨供应商模型 ID 前缀（稳定、可识别，但不单独作为所有权依据）。
pub const MANAGED_CROSS_PROVIDER_MODEL_PREFIX: &str = "ccswitch_xprov_";

/// CC Switch 私有注册表根表。位于 Grok 原生 schema 之外，由 toml_edit 维护。
pub const MANAGED_REGISTRY_ROOT: &str = "cc_switch";
/// 注册表子表：`[cc_switch.managed_xprov.<managed_id>]`。
pub const MANAGED_REGISTRY_TABLE: &str = "managed_xprov";
pub const MANAGED_REG_SOURCE_PROVIDER: &str = "source_provider";
pub const MANAGED_REG_SOURCE_MODEL: &str = "source_model";

/// 遗留：曾写入 `[model.*]` 的所有权字段。仅用于读取/迁移，不再新写。
pub const LEGACY_MANAGED_OWNER_KEY: &str = "cc_switch_owned";
pub const LEGACY_MANAGED_SOURCE_PROVIDER_KEY: &str = "cc_switch_source_provider";
pub const LEGACY_MANAGED_SOURCE_MODEL_KEY: &str = "cc_switch_source_model";

/// 获取 Grok Build 配置目录。
///
/// 优先级与 Grok Build 自身约定保持一致：`GROK_CONFIG`、`GROK_HOME`、
/// CC Switch 设置覆盖、最后回退到 `~/.grok`。
pub fn get_grok_config_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("GROK_CONFIG").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        return path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
    }
    if let Some(path) = std::env::var_os("GROK_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(custom) = crate::settings::get_grok_override_dir() {
        return custom;
    }
    get_home_dir().join(".grok")
}

pub fn get_grok_config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("GROK_CONFIG").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    get_grok_config_dir().join("config.toml")
}

pub fn get_grok_backup_dir() -> PathBuf {
    get_home_dir().join(".grok_switch").join("backups")
}

fn backup_current_config(path: &Path, next_text: &str) -> Result<(), AppError> {
    let Ok(current) = fs::read(path) else {
        return Ok(());
    };
    if current == next_text.as_bytes() {
        return Ok(());
    }

    let backup_dir = get_grok_backup_dir();
    fs::create_dir_all(&backup_dir).map_err(|e| AppError::io(&backup_dir, e))?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f");
    let backup_path = backup_dir.join(format!("config-{stamp}.toml"));
    fs::write(&backup_path, current).map_err(|e| AppError::io(&backup_path, e))?;

    let mut backups = fs::read_dir(&backup_dir)
        .map_err(|e| AppError::io(&backup_dir, e))?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with("config-")
                && entry.path().extension().and_then(|ext| ext.to_str()) == Some("toml")
        })
        .collect::<Vec<_>>();
    backups.sort_by_key(|entry| entry.file_name());
    let remove_count = backups.len().saturating_sub(10);
    for entry in backups.into_iter().take(remove_count) {
        fs::remove_file(entry.path()).map_err(|e| AppError::io(entry.path(), e))?;
    }
    Ok(())
}

pub fn read_grok_config_text() -> Result<String, AppError> {
    let path = get_grok_config_path();
    if !path.exists() {
        return Err(AppError::localized(
            "grok.live.missing",
            "Grok Build 配置文件不存在",
            "Grok Build configuration file is missing",
        ));
    }
    std::fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))
}

pub fn write_grok_config_text(text: &str) -> Result<(), AppError> {
    let path = get_grok_config_path();
    validate_config_toml(text)?;
    backup_current_config(&path, text)?;
    write_text_file(&path, text)
}

pub fn validate_config_toml(text: &str) -> Result<(), AppError> {
    text.parse::<DocumentMut>().map(|_| ()).map_err(|e| {
        AppError::localized(
            "provider.grok.config.invalid_toml",
            format!("Grok config.toml 格式错误: {e}"),
            format!("Invalid Grok config.toml: {e}"),
        )
    })
}

fn auth_api_key(settings: &Value) -> Option<String> {
    let auth = settings.get("auth")?.as_object()?;
    ["OPENAI_API_KEY", "XAI_API_KEY", "GROK_API_KEY"]
        .into_iter()
        .find_map(|key| auth.get(key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(ToString::to_string)
}

fn api_backend_from_format(format: Option<&str>) -> Option<&'static str> {
    let normalized = format?.trim().to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "chat" | "chat_completions" | "chat-completions" | "openai_chat" | "openai-chat"
    ) {
        Some("chat_completions")
    } else if matches!(
        normalized.as_str(),
        "responses" | "openai_responses" | "openai-responses"
    ) {
        Some("responses")
    } else {
        None
    }
}

pub fn api_format_from_backend(backend: Option<&str>) -> &'static str {
    match backend
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("chat") | Some("chat_completions") | Some("chat-completions") => "openai_chat",
        _ => "openai_responses",
    }
}

fn selected_model_table(doc: &DocumentMut) -> Option<Table> {
    let active = doc
        .get("models")
        .and_then(|item| item.get("default"))
        .and_then(Item::as_value)
        .and_then(|value| value.as_str());
    if let Some(table) = active.and_then(|name| {
        doc.get("model")
            .and_then(|item| item.get(name))
            .and_then(Item::as_table)
    }) {
        return Some(table.clone());
    }
    if let Some(table) = doc
        .get("model")
        .and_then(|item| item.get(CC_SWITCH_GROK_MODEL_ID))
        .and_then(Item::as_table)
    {
        return Some(table.clone());
    }
    doc.get("model")
        .and_then(Item::as_table)
        .and_then(|models| models.iter().find_map(|(_, item)| item.as_table()))
        .cloned()
}

fn provider_config_doc(provider: &Provider) -> Result<DocumentMut, AppError> {
    let config_text = provider
        .settings_config
        .get("config")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AppError::localized(
                "provider.grok.config.missing",
                "Grok 供应商缺少 config 配置",
                "Grok provider is missing config",
            )
        })?;
    config_text.parse::<DocumentMut>().map_err(|e| {
        AppError::localized(
            "provider.grok.config.invalid_toml",
            format!("Grok config.toml 格式错误: {e}"),
            format!("Invalid Grok config.toml: {e}"),
        )
    })
}

fn provider_model_table(provider: &Provider) -> Result<Table, AppError> {
    let doc = provider_config_doc(provider)?;
    selected_model_table(&doc).ok_or_else(|| {
        AppError::localized(
            "provider.grok.model.missing",
            "Grok 供应商配置缺少 [model.ccswitch] 模型定义",
            "Grok provider config is missing the [model.ccswitch] model definition",
        )
    })
}

fn set_owned_key(live: &mut DocumentMut, provider: &DocumentMut, section: &str, key: &str) {
    if let Some(item) = provider.get(section).and_then(|item| item.get(key)) {
        live[section][key] = item.clone();
    } else if let Some(table) = live.get_mut(section).and_then(Item::as_table_like_mut) {
        table.remove(key);
    }
}

fn merge_profile_doc(live_doc: &mut DocumentMut, profile_doc: &DocumentMut) {
    for (section, keys) in [
        ("endpoints", &["models_base_url"][..]),
        ("models", &["default", "web_search"][..]),
    ] {
        for key in keys {
            set_owned_key(live_doc, profile_doc, section, key);
        }
    }
    if let Some(routes) = profile_doc
        .get("subagents")
        .and_then(|item| item.get("models"))
    {
        live_doc["subagents"]["models"] = routes.clone();
    }
    if let Some(models) = profile_doc.get("model").and_then(Item::as_table) {
        for (name, model) in models {
            // 永不覆盖 live 中已有的 managed 跨供应商模型（由后续 materialize 步骤维护）
            if is_managed_cross_provider_model(live_doc, name) {
                continue;
            }
            live_doc["model"][name] = model.clone();
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// 生成稳定、碰撞风险极低的 managed 模型 ID。
///
/// 对 `source_provider_id \\0 source_model_id` 做 SHA-256，取前 16 字节（128 bit）
/// 十六进制。截断摘要的碰撞概率可忽略，但并非数学上不可能；与旧式
/// `sanitize(a/b) == sanitize(a_b)` 不同，不同源身份几乎总会得到不同 ID。
///
/// 格式：`ccswitch_xprov_{hex}`。若与 live 中非 managed 用户模型冲突，
/// 调用方追加 `_m1` / `_m2`…。`existing_models` 保留签名兼容，本函数返回首选 ID。
pub fn managed_cross_provider_model_id(
    source_provider_id: &str,
    source_model_id: &str,
    _existing_models: &HashSet<String>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_provider_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(source_model_id.as_bytes());
    let digest = hasher.finalize();
    format!(
        "{MANAGED_CROSS_PROVIDER_MODEL_PREFIX}{}",
        hex_encode(&digest[..16])
    )
}

fn legacy_model_table_has_owner_marker(table: &Table) -> bool {
    table
        .get(LEGACY_MANAGED_OWNER_KEY)
        .and_then(Item::as_value)
        .and_then(|v| v.as_bool())
        == Some(true)
}

fn strip_legacy_owner_fields(table: &mut Table) {
    table.remove(LEGACY_MANAGED_OWNER_KEY);
    table.remove(LEGACY_MANAGED_SOURCE_PROVIDER_KEY);
    table.remove(LEGACY_MANAGED_SOURCE_MODEL_KEY);
}

fn registry_table<'a>(doc: &'a DocumentMut) -> Option<&'a Table> {
    doc.get(MANAGED_REGISTRY_ROOT)
        .and_then(|item| item.get(MANAGED_REGISTRY_TABLE))
        .and_then(Item::as_table)
}

fn registry_entry_source(entry: &Table) -> Option<(String, String)> {
    let provider = entry
        .get(MANAGED_REG_SOURCE_PROVIDER)
        .and_then(Item::as_value)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let model = entry
        .get(MANAGED_REG_SOURCE_MODEL)
        .and_then(Item::as_value)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    Some((provider, model))
}

fn is_registered_managed_model(doc: &DocumentMut, name: &str) -> bool {
    registry_table(doc)
        .and_then(|table| table.get(name))
        .and_then(Item::as_table)
        .is_some_and(|entry| registry_entry_source(entry).is_some())
}

/// 是否为 CC Switch 托管的跨供应商模型。
/// 优先查独立注册表；兼容遗留 `[model.*]` 内联标记。从不单靠前缀判定。
fn is_managed_cross_provider_model(doc: &DocumentMut, name: &str) -> bool {
    if is_registered_managed_model(doc, name) {
        return true;
    }
    // 遗留兼容：旧写入把 cc_switch_owned 放在 model 表内
    doc.get("model")
        .and_then(|item| item.get(name))
        .and_then(Item::as_table)
        .is_some_and(legacy_model_table_has_owner_marker)
}

fn list_owned_managed_model_names(doc: &DocumentMut) -> HashSet<String> {
    let mut names = HashSet::new();
    if let Some(reg) = registry_table(doc) {
        for (name, item) in reg.iter() {
            if item
                .as_table()
                .is_some_and(|t| registry_entry_source(t).is_some())
            {
                names.insert(name.to_string());
            }
        }
    }
    if let Some(models) = doc.get("model").and_then(Item::as_table) {
        for (name, item) in models.iter() {
            if item
                .as_table()
                .is_some_and(legacy_model_table_has_owner_marker)
            {
                names.insert(name.to_string());
            }
        }
    }
    names
}

fn find_registered_managed_id(
    doc: &DocumentMut,
    source_provider_id: &str,
    source_model_id: &str,
) -> Option<String> {
    if let Some(reg) = registry_table(doc) {
        for (name, item) in reg.iter() {
            if let Some((provider, model)) = item.as_table().and_then(registry_entry_source) {
                if provider == source_provider_id && model == source_model_id {
                    let model_exists = doc
                        .get("model")
                        .and_then(|m| m.get(name))
                        .and_then(Item::as_table)
                        .is_some();
                    if model_exists {
                        return Some(name.to_string());
                    }
                }
            }
        }
    }
    // 遗留内联标记
    if let Some(models) = doc.get("model").and_then(Item::as_table) {
        for (name, item) in models.iter() {
            let Some(table) = item.as_table() else {
                continue;
            };
            if !legacy_model_table_has_owner_marker(table) {
                continue;
            }
            let provider = table
                .get(LEGACY_MANAGED_SOURCE_PROVIDER_KEY)
                .and_then(Item::as_value)
                .and_then(|v| v.as_str());
            let model = table
                .get(LEGACY_MANAGED_SOURCE_MODEL_KEY)
                .and_then(Item::as_value)
                .and_then(|v| v.as_str());
            if provider == Some(source_provider_id) && model == Some(source_model_id) {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn write_managed_registry_entry(
    doc: &mut DocumentMut,
    managed_id: &str,
    source_provider_id: &str,
    source_model_id: &str,
) {
    let mut entry = Table::new();
    entry[MANAGED_REG_SOURCE_PROVIDER] = value(source_provider_id);
    entry[MANAGED_REG_SOURCE_MODEL] = value(source_model_id);
    doc[MANAGED_REGISTRY_ROOT][MANAGED_REGISTRY_TABLE][managed_id] = Item::Table(entry);
}

fn remove_managed_registry_entries(doc: &mut DocumentMut, names: &[String]) {
    // names 为空时仍继续走下方收起逻辑（例如 strip 时清理空注册表残留）。
    if !names.is_empty() {
        if let Some(reg) = doc
            .get_mut(MANAGED_REGISTRY_ROOT)
            .and_then(|item| item.get_mut(MANAGED_REGISTRY_TABLE))
            .and_then(Item::as_table_like_mut)
        {
            for name in names {
                reg.remove(name);
            }
        }
    }
    // 注册表空时收起空表，避免留下无用段落
    let reg_empty = doc
        .get(MANAGED_REGISTRY_ROOT)
        .and_then(|item| item.get(MANAGED_REGISTRY_TABLE))
        .and_then(Item::as_table)
        .is_none_or(|t| t.is_empty());
    if reg_empty {
        if let Some(root) = doc
            .get_mut(MANAGED_REGISTRY_ROOT)
            .and_then(Item::as_table_like_mut)
        {
            root.remove(MANAGED_REGISTRY_TABLE);
        }
    }
    let root_empty = doc
        .get(MANAGED_REGISTRY_ROOT)
        .and_then(Item::as_table)
        .is_none_or(|t| t.is_empty());
    if root_empty {
        doc.as_table_mut().remove(MANAGED_REGISTRY_ROOT);
    }
}

/// 将遗留 model 内联标记迁入注册表，并剥离非文档字段。
fn migrate_legacy_inline_markers(doc: &mut DocumentMut) {
    let migrations: Vec<(String, String, String)> = doc
        .get("model")
        .and_then(Item::as_table)
        .map(|models| {
            models
                .iter()
                .filter_map(|(name, item)| {
                    let table = item.as_table()?;
                    if !legacy_model_table_has_owner_marker(table) {
                        return None;
                    }
                    let provider = table
                        .get(LEGACY_MANAGED_SOURCE_PROVIDER_KEY)
                        .and_then(Item::as_value)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let model = table
                        .get(LEGACY_MANAGED_SOURCE_MODEL_KEY)
                        .and_then(Item::as_value)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    Some((name.to_string(), provider, model))
                })
                .collect()
        })
        .unwrap_or_default();
    for (name, provider, model) in migrations {
        if !provider.is_empty() && !model.is_empty() && !is_registered_managed_model(doc, &name) {
            write_managed_registry_entry(doc, &name, &provider, &model);
        }
        if let Some(table) = doc
            .get_mut("model")
            .and_then(|item| item.get_mut(name.as_str()))
            .and_then(Item::as_table_mut)
        {
            strip_legacy_owner_fields(table);
        }
    }
}

fn list_model_names(doc: &DocumentMut) -> HashSet<String> {
    doc.get("model")
        .and_then(Item::as_table)
        .map(|models| models.iter().map(|(n, _)| n.to_string()).collect())
        .unwrap_or_default()
}

fn allocate_managed_model_id(
    live_doc: &DocumentMut,
    source_provider_id: &str,
    source_model_id: &str,
) -> String {
    // 已注册且模型表仍在：复用（含用户冲突后缀后的 ID）
    if let Some(existing) =
        find_registered_managed_id(live_doc, source_provider_id, source_model_id)
    {
        return existing;
    }
    let preferred =
        managed_cross_provider_model_id(source_provider_id, source_model_id, &HashSet::new());
    let existing = list_model_names(live_doc);
    if !existing.contains(&preferred) || is_managed_cross_provider_model(live_doc, &preferred) {
        return preferred;
    }
    // 与用户自建非 managed 模型冲突：追加后缀
    let mut n = 1u32;
    loop {
        let candidate = format!("{preferred}_m{n}");
        if !existing.contains(&candidate) || is_managed_cross_provider_model(live_doc, &candidate) {
            return candidate;
        }
        n += 1;
    }
}

fn extract_model_table_from_provider(source: &Provider, model_id: &str) -> Result<Table, AppError> {
    let doc = provider_config_doc(source)?;
    doc.get("model")
        .and_then(|item| item.get(model_id))
        .and_then(Item::as_table)
        .cloned()
        .ok_or_else(|| {
            AppError::localized(
                "provider.grok.subagent_route.model_missing",
                format!(
                    "Grok 跨供应商路由引用的模型不存在：供应商「{}」中没有 [model.{}]",
                    source.name, model_id
                ),
                format!(
                    "Cross-provider Grok subagent route refers to missing model: provider '{}' has no [model.{}]",
                    source.name, model_id
                ),
            )
        })
}

fn inject_auth_into_model_table(table: &mut Table, source: &Provider) {
    if table.get("api_key").is_some() || table.get("env_key").is_some() {
        return;
    }
    if let Some(api_key) = auth_api_key(&source.settings_config) {
        table["api_key"] = value(api_key);
    }
    // 确保不残留遗留所有权字段
    strip_legacy_owner_fields(table);
}

/// 将跨供应商路由物化进 live 文档，并写回 `[subagents.models]`。
///
/// - 同源路由：直接使用本 Profile 的 model ID（必须存在于 live/profile 中）
/// - 异源路由：复制源模型表到 managed ID，并在独立注册表登记所有权
/// - `all_providers` 为空或缺少源供应商时：若 live 已物化对应 managed 模型则复用，
///   否则返回明确错误（绝不静默回退到当前供应商同名模型）
fn apply_subagent_routes(
    live_doc: &mut DocumentMut,
    active: &Provider,
    all_providers: &IndexMap<String, Provider>,
) -> Result<(), AppError> {
    migrate_legacy_inline_markers(live_doc);

    let routes = active
        .meta
        .as_ref()
        .map(|m| &m.grok_subagent_routes)
        .cloned()
        .unwrap_or_default();
    if routes.is_empty() {
        // 无显式 meta 路由：先摘掉仍指向 managed 跨供应商模型的陈旧 live 角色，
        // 再删除已无引用的 managed 模型与注册表项。
        // 否则 merge_profile_doc 在 Profile 无 [subagents.models] 时会保留旧 live
        // 外源路由，cleanup 会把它当成引用，导致角色与 managed 模型永远清不掉。
        // 用户自有路由/模型与同源 Profile TOML 路由保留。
        cleanup_unreferenced_managed_models(live_doc);
        return Ok(());
    }

    let mut resolved: HashMap<String, String> = HashMap::new();
    let mut referenced_managed: HashSet<String> = HashSet::new();

    for (role, route) in &routes {
        let role = role.trim();
        if role.is_empty() {
            continue;
        }
        let model_id = route.model_id.trim();
        if model_id.is_empty() {
            return Err(AppError::localized(
                "provider.grok.subagent_route.model_id_empty",
                format!("Grok 子代理角色「{role}」的模型 ID 为空"),
                format!("Grok subagent role '{role}' has an empty model ID"),
            ));
        }

        let source_provider_id = route
            .provider_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .unwrap_or(active.id.as_str());

        if source_provider_id == active.id {
            // 同源：路由到当前 profile 模型。要求该模型定义存在。
            let exists = live_doc
                .get("model")
                .and_then(|item| item.get(model_id))
                .and_then(Item::as_table)
                .is_some();
            if !exists {
                return Err(AppError::localized(
                    "provider.grok.subagent_route.local_model_missing",
                    format!(
                        "Grok 子代理角色「{role}」引用的本供应商模型 [model.{model_id}] 不存在"
                    ),
                    format!(
                        "Grok subagent role '{role}' refers to missing local model [model.{model_id}]"
                    ),
                ));
            }
            resolved.insert(role.to_string(), model_id.to_string());
            continue;
        }

        match all_providers.get(source_provider_id) {
            None => {
                // 空 provider map / 源未加载：安全复用 live 中已物化的 managed 模型。
                // 正常切换路径会传入完整 map；此分支服务 legacy wrapper 与无 DB 快照。
                if let Some(existing_id) =
                    find_registered_managed_id(live_doc, source_provider_id, model_id)
                {
                    referenced_managed.insert(existing_id.clone());
                    resolved.insert(role.to_string(), existing_id);
                    continue;
                }
                return Err(AppError::localized(
                    "provider.grok.subagent_route.provider_missing",
                    format!(
                        "Grok 子代理角色「{role}」引用的供应商「{source_provider_id}」不存在或已删除"
                    ),
                    format!(
                        "Grok subagent role '{role}' refers to missing provider '{source_provider_id}'"
                    ),
                ));
            }
            Some(source) if source.category.as_deref() == Some("official") => {
                return Err(AppError::localized(
                    "provider.grok.subagent_route.official_source_unsupported",
                    format!(
                        "Grok 子代理角色「{role}」不能路由到官方 OAuth 供应商（无独立模型定义）"
                    ),
                    format!(
                        "Grok subagent role '{role}' cannot route to the official OAuth provider (no standalone model definitions)"
                    ),
                ));
            }
            Some(source) => {
                let mut model_table = extract_model_table_from_provider(source, model_id)?;
                inject_auth_into_model_table(&mut model_table, source);
                // 所有权只写注册表，绝不写进 [model.*] 非文档字段。

                let managed_id = allocate_managed_model_id(live_doc, source_provider_id, model_id);
                live_doc["model"][managed_id.as_str()] = Item::Table(model_table);
                write_managed_registry_entry(live_doc, &managed_id, source_provider_id, model_id);
                referenced_managed.insert(managed_id.clone());
                resolved.insert(role.to_string(), managed_id);
            }
        }
    }

    // 合并：meta 路由覆盖同名 role；保留 profile 中未被 meta 覆盖的 role。
    // 先 clone 再判断，避免对 live_doc 的双重借用。
    let existing_pairs: Vec<(String, Item)> = live_doc
        .get("subagents")
        .and_then(|item| item.get("models"))
        .and_then(Item::as_table)
        .map(|existing| {
            existing
                .iter()
                .map(|(role, item)| (role.to_string(), item.clone()))
                .collect()
        })
        .unwrap_or_default();
    let mut final_routes = Table::new();
    for (role, item) in existing_pairs {
        if resolved.contains_key(&role) {
            continue;
        }
        if let Some(model_name) = item.as_value().and_then(|v| v.as_str()) {
            // 丢弃指向未引用 managed 模型的陈旧路由，防止静默落到错误目标
            if is_managed_cross_provider_model(live_doc, model_name)
                && !referenced_managed.contains(model_name)
            {
                continue;
            }
        }
        final_routes[role.as_str()] = item;
    }
    for (role, model) in &resolved {
        final_routes[role.as_str()] = value(model.as_str());
    }
    live_doc["subagents"]["models"] = Item::Table(final_routes);

    // 清理：只删注册表登记（或遗留标记）且不再被引用的 managed 模型
    cleanup_managed_models(live_doc, &referenced_managed);
    Ok(())
}

/// 删除 live `[subagents.models]` 中目标为 CC Switch 托管跨供应商模型的角色。
/// 所有权仅依据注册表 / 遗留标记判定，绝不单靠前缀。
fn strip_live_routes_to_managed_models(live_doc: &mut DocumentMut) {
    let owned = list_owned_managed_model_names(live_doc);
    if owned.is_empty() {
        return;
    }
    if let Some(routes) = live_doc
        .get_mut("subagents")
        .and_then(|item| item.get_mut("models"))
        .and_then(Item::as_table_like_mut)
    {
        let stale: Vec<String> = routes
            .iter()
            .filter_map(|(role, item)| {
                item.as_value()
                    .and_then(|v| v.as_str())
                    .filter(|name| owned.contains(*name))
                    .map(|_| role.to_string())
            })
            .collect();
        for role in stale {
            routes.remove(&role);
        }
    }
}

fn cleanup_unreferenced_managed_models(live_doc: &mut DocumentMut) {
    migrate_legacy_inline_markers(live_doc);
    // meta 为空时，陈旧 live 外源路由不得再充当引用；先摘掉再清模型。
    strip_live_routes_to_managed_models(live_doc);
    cleanup_managed_models(live_doc, &HashSet::new());
}

fn cleanup_managed_models(live_doc: &mut DocumentMut, keep: &HashSet<String>) {
    let owned = list_owned_managed_model_names(live_doc);
    let stale: Vec<String> = owned
        .into_iter()
        .filter(|name| !keep.contains(name))
        .collect();
    if stale.is_empty() {
        return;
    }
    if let Some(models) = live_doc.get_mut("model").and_then(Item::as_table_like_mut) {
        for name in &stale {
            models.remove(name);
        }
    }
    remove_managed_registry_entries(live_doc, &stale);
    // 若 default/web_search 误指已删 managed 模型，一并清除
    for key in ["default", "web_search"] {
        let selected_is_stale = live_doc
            .get("models")
            .and_then(|item| item.get(key))
            .and_then(Item::as_value)
            .and_then(|v| v.as_str())
            .is_some_and(|name| stale.iter().any(|s| s == name));
        if selected_is_stale {
            if let Some(table) = live_doc.get_mut("models").and_then(Item::as_table_like_mut) {
                table.remove(key);
            }
        }
    }
}

/// 从 live 导入的供应商配置中剥离 managed 跨供应商模型与指向它们的路由。
/// meta 才是跨供应商路由的 SSOT；profile TOML 不应被污染。
fn strip_managed_models_from_doc(doc: &mut DocumentMut) {
    migrate_legacy_inline_markers(doc);
    let managed_names: Vec<String> = list_owned_managed_model_names(doc).into_iter().collect();
    if managed_names.is_empty() {
        // 仍尝试去掉空注册表
        remove_managed_registry_entries(doc, &[]);
        return;
    }
    if let Some(models) = doc.get_mut("model").and_then(Item::as_table_like_mut) {
        for name in &managed_names {
            models.remove(name);
        }
    }
    for key in ["default", "web_search"] {
        let selected_is_managed = doc
            .get("models")
            .and_then(|item| item.get(key))
            .and_then(Item::as_value)
            .and_then(|v| v.as_str())
            .is_some_and(|name| managed_names.iter().any(|m| m == name));
        if selected_is_managed {
            if let Some(table) = doc.get_mut("models").and_then(Item::as_table_like_mut) {
                table.remove(key);
            }
        }
    }
    if let Some(routes) = doc
        .get_mut("subagents")
        .and_then(|item| item.get_mut("models"))
        .and_then(Item::as_table_like_mut)
    {
        let stale: Vec<String> = routes
            .iter()
            .filter_map(|(role, item)| {
                item.as_value()
                    .and_then(|v| v.as_str())
                    .is_some_and(|model| managed_names.iter().any(|m| m == model))
                    .then(|| role.to_string())
            })
            .collect();
        for role in stale {
            routes.remove(&role);
        }
    }
    remove_managed_registry_entries(doc, &managed_names);
}

/// 将 Grok Profile 管理的段落加入现有全局配置，同时保留其它全局设置。
pub fn merge_grok_profile_config_text(
    existing_text: &str,
    profile_text: &str,
) -> Result<String, AppError> {
    let mut live_doc = if existing_text.trim().is_empty() {
        DocumentMut::new()
    } else {
        existing_text.parse::<DocumentMut>().map_err(|e| {
            AppError::localized(
                "grok.live.invalid_toml",
                format!("现有 Grok config.toml 格式错误: {e}"),
                format!("Existing Grok config.toml is invalid: {e}"),
            )
        })?
    };
    let profile_doc = profile_text.parse::<DocumentMut>().map_err(|e| {
        AppError::localized(
            "provider.grok.config.invalid_toml",
            format!("Grok Profile TOML 格式错误: {e}"),
            format!("Invalid Grok Profile TOML: {e}"),
        )
    })?;
    if profile_text.trim().is_empty() {
        return use_official_auth_config_text(existing_text);
    }
    selected_model_table(&profile_doc).ok_or_else(|| {
        AppError::localized(
            "provider.grok.model.missing",
            "Grok Profile 缺少 [model.*] 模型定义",
            "Grok Profile is missing a [model.*] model definition",
        )
    })?;

    merge_profile_doc(&mut live_doc, &profile_doc);
    Ok(live_doc.to_string())
}

pub fn merge_grok_profile_into_live(profile_text: &str) -> Result<(), AppError> {
    let existing = fs::read_to_string(get_grok_config_path()).unwrap_or_default();
    let next = merge_grok_profile_config_text(&existing, profile_text)?;
    write_grok_config_text(&next)
}

/// 清除第三方端点与活动模型选择，让 Grok 回退到 `grok login` 管理的官方账号。
/// 保留 `[model.*]` 定义，避免切换账号时破坏用户手工维护的模型目录。
pub fn use_official_auth_config_text(text: &str) -> Result<String, AppError> {
    let mut doc = if text.trim().is_empty() {
        DocumentMut::new()
    } else {
        text.parse::<DocumentMut>().map_err(|e| {
            AppError::localized(
                "grok.live.invalid_toml",
                format!("现有 Grok config.toml 格式错误: {e}"),
                format!("Existing Grok config.toml is invalid: {e}"),
            )
        })?
    };
    for (section, keys) in [
        ("endpoints", &["models_base_url"][..]),
        ("models", &["default", "web_search"][..]),
    ] {
        if let Some(table) = doc.get_mut(section).and_then(Item::as_table_like_mut) {
            for key in keys {
                table.remove(key);
            }
        }
    }
    Ok(doc.to_string())
}

pub fn apply_privacy_protection_config_text(text: &str) -> Result<String, AppError> {
    let mut doc = if text.trim().is_empty() {
        DocumentMut::new()
    } else {
        text.parse::<DocumentMut>().map_err(|e| {
            AppError::localized(
                "grok.live.invalid_toml",
                format!("Grok config.toml 格式错误: {e}"),
                format!("Invalid Grok config.toml: {e}"),
            )
        })?
    };
    doc["features"]["telemetry"] = value(false);
    doc["telemetry"]["trace_upload"] = value(false);
    doc["telemetry"]["mixpanel_enabled"] = value(false);
    doc["harness"]["disable_codebase_upload"] = value(true);
    Ok(doc.to_string())
}

pub fn apply_privacy_protection_live() -> Result<String, AppError> {
    let existing = fs::read_to_string(get_grok_config_path()).unwrap_or_default();
    let next = apply_privacy_protection_config_text(&existing)?;
    write_grok_config_text(&next)?;
    Ok(next)
}

/// 将供应商 Profile patch 到一份 Grok 配置文本中。
///
/// 修改 `[endpoints].models_base_url`、`[models].default/web_search`、
/// `[subagents.models]` 与全部 `[model.*]`；其它段落原样保留。
/// 调用者可覆盖 `base_url`、`api_key` 与 `api_backend`，用于本地代理接管。
///
/// `all_providers` 用于解析 `meta.grokSubagentRoutes` 中的跨供应商模型并物化。
pub fn patch_config_text_for_provider(
    existing_text: &str,
    provider: &Provider,
    base_url_override: Option<&str>,
    api_key_override: Option<&str>,
    api_backend_override: Option<&str>,
) -> Result<String, AppError> {
    let empty = IndexMap::new();
    patch_config_text_for_provider_with_routes(
        existing_text,
        provider,
        &empty,
        base_url_override,
        api_key_override,
        api_backend_override,
    )
}

pub fn patch_config_text_for_provider_with_routes(
    existing_text: &str,
    provider: &Provider,
    all_providers: &IndexMap<String, Provider>,
    base_url_override: Option<&str>,
    api_key_override: Option<&str>,
    api_backend_override: Option<&str>,
) -> Result<String, AppError> {
    let mut live_doc = if existing_text.trim().is_empty() {
        DocumentMut::new()
    } else {
        existing_text.parse::<DocumentMut>().map_err(|e| {
            AppError::localized(
                "grok.live.invalid_toml",
                format!("现有 Grok config.toml 格式错误: {e}"),
                format!("Existing Grok config.toml is invalid: {e}"),
            )
        })?
    };
    if provider.category.as_deref() == Some("official") {
        if base_url_override.is_some() || api_key_override.is_some() {
            return Err(AppError::localized(
                "provider.grok.official_proxy_unsupported",
                "Grok 官方 OAuth 账号不支持代理接管，请先切换到 API 供应商",
                "Grok official OAuth does not support proxy takeover; switch to an API provider first",
            ));
        }
        // 官方 OAuth：清掉第三方端点/默认模型，但保留用户模型与有效跨供应商路由。
        let cleaned = use_official_auth_config_text(existing_text)?;
        let mut cleaned_doc = cleaned.parse::<DocumentMut>().map_err(|e| {
            AppError::localized(
                "grok.live.invalid_toml",
                format!("现有 Grok config.toml 格式错误: {e}"),
                format!("Existing Grok config.toml is invalid: {e}"),
            )
        })?;
        apply_subagent_routes(&mut cleaned_doc, provider, all_providers)?;
        return Ok(cleaned_doc.to_string());
    }

    let mut provider_doc = provider_config_doc(provider)?;
    provider_model_table(provider)?;

    let api_key = api_key_override
        .map(ToString::to_string)
        .or_else(|| auth_api_key(&provider.settings_config));
    let fallback_backend = api_backend_override.or_else(|| {
        api_backend_from_format(
            provider
                .meta
                .as_ref()
                .and_then(|meta| meta.api_format.as_deref()),
        )
    });
    if let Some(models) = provider_doc.get_mut("model").and_then(Item::as_table_mut) {
        for (_, item) in models.iter_mut() {
            let Some(model) = item.as_table_mut() else {
                continue;
            };
            if let Some(base_url) = base_url_override {
                model["base_url"] = value(base_url);
            }
            if let Some(api_key) = api_key.as_deref() {
                if api_key_override.is_some()
                    || (model.get("api_key").is_none() && model.get("env_key").is_none())
                {
                    model["api_key"] = value(api_key);
                }
            }
            if let Some(backend) = fallback_backend {
                if api_backend_override.is_some() || model.get("api_backend").is_none() {
                    model["api_backend"] = value(backend);
                }
            }
        }
    }

    if let Some(base_url) = base_url_override {
        provider_doc["endpoints"]["models_base_url"] = value(base_url);
    }
    merge_profile_doc(&mut live_doc, &provider_doc);
    // 跨供应商物化：只注入所需 foreign 模型；同源路由用 profile 模型 ID。
    // 代理接管时仅改写 active profile 模型，foreign 模型保留自身 base_url/api_key。
    apply_subagent_routes(&mut live_doc, provider, all_providers)?;
    Ok(live_doc.to_string())
}

/// 无 provider map 的兼容入口（测试 / 无 DB 快照路径）。
///
/// 若 `meta.grokSubagentRoutes` 含跨供应商路由：仅当 live 中已有对应
/// managed 模型（注册表 + `[model.*]`）时才会安全复用；否则返回明确错误。
/// 正常切换/接管请使用 `write_grok_provider_live_with_routes`。
pub fn write_grok_provider_live(provider: &Provider) -> Result<(), AppError> {
    write_grok_provider_live_with_routes(provider, &IndexMap::new())
}

pub fn write_grok_provider_live_with_routes(
    provider: &Provider,
    all_providers: &IndexMap<String, Provider>,
) -> Result<(), AppError> {
    let existing = std::fs::read_to_string(get_grok_config_path()).unwrap_or_default();
    let patched = patch_config_text_for_provider_with_routes(
        &existing,
        provider,
        all_providers,
        None,
        None,
        None,
    )?;
    write_grok_config_text(&patched)
}

/// 无 provider map 的接管兼容入口；跨供应商路由语义同 `write_grok_provider_live`。
pub fn write_grok_takeover_live(provider: &Provider, proxy_base_url: &str) -> Result<(), AppError> {
    write_grok_takeover_live_with_routes(provider, proxy_base_url, &IndexMap::new())
}

pub fn write_grok_takeover_live_with_routes(
    provider: &Provider,
    proxy_base_url: &str,
    all_providers: &IndexMap<String, Provider>,
) -> Result<(), AppError> {
    let existing = std::fs::read_to_string(get_grok_config_path()).unwrap_or_default();
    let patched = patch_config_text_for_provider_with_routes(
        &existing,
        provider,
        all_providers,
        Some(proxy_base_url),
        Some(GROK_PROXY_TOKEN_PLACEHOLDER),
        None,
    )?;
    write_grok_config_text(&patched)
}

/// 将 Grok live 配置转换为供应商存储格式 `{ auth, config }`。
pub fn settings_from_config_text(text: &str) -> Result<Value, AppError> {
    let mut doc = text.parse::<DocumentMut>().map_err(|e| {
        AppError::localized(
            "grok.live.invalid_toml",
            format!("Grok config.toml 格式错误: {e}"),
            format!("Invalid Grok config.toml: {e}"),
        )
    })?;
    selected_model_table(&doc).ok_or_else(|| {
        AppError::localized(
            "grok.live.model_missing",
            "Grok config.toml 中未找到当前模型配置",
            "The active model is missing from Grok config.toml",
        )
    })?;

    let default_model = doc
        .get("models")
        .and_then(|item| item.get("default"))
        .and_then(Item::as_value)
        .and_then(|value| value.as_str())
        .map(ToString::to_string);
    let mut api_key = String::new();
    if let Some(models) = doc.get_mut("model").and_then(Item::as_table_mut) {
        if let Some(name) = default_model.as_deref() {
            if let Some(model) = models.get(name).and_then(Item::as_table) {
                api_key = model
                    .get("api_key")
                    .and_then(Item::as_value)
                    .and_then(|value| value.as_str())
                    .map(ToString::to_string)
                    .or_else(|| {
                        model
                            .get("env_key")
                            .and_then(Item::as_value)
                            .and_then(|value| value.as_str())
                            .and_then(|name| std::env::var(name).ok())
                    })
                    .unwrap_or_default();
            }
        }
        if api_key.is_empty() {
            api_key = models
                .iter()
                .filter_map(|(_, item)| item.as_table())
                .find_map(|model| {
                    model
                        .get("api_key")
                        .and_then(Item::as_value)
                        .and_then(|value| value.as_str())
                        .map(ToString::to_string)
                })
                .unwrap_or_default();
        }
        for (_, item) in models.iter_mut() {
            let Some(model) = item.as_table_mut() else {
                continue;
            };
            let same_as_profile_key = model
                .get("api_key")
                .and_then(Item::as_value)
                .and_then(|value| value.as_str())
                == Some(api_key.as_str());
            if same_as_profile_key {
                model.remove("api_key");
            }
        }
    }

    // 回填/导入时剥离 managed 跨供应商模型，避免污染供应商 Profile。
    strip_managed_models_from_doc(&mut doc);

    let mut provider_doc = DocumentMut::new();
    for (section, keys) in [
        ("endpoints", &["models_base_url"][..]),
        ("models", &["default", "web_search"][..]),
        ("subagents", &["models"][..]),
    ] {
        for key in keys {
            if let Some(item) = doc.get(section).and_then(|item| item.get(key)) {
                provider_doc[section][key] = item.clone();
            }
        }
    }
    if let Some(models) = doc.get("model") {
        provider_doc["model"] = models.clone();
    }

    Ok(json!({
        "auth": { "OPENAI_API_KEY": api_key },
        "config": provider_doc.to_string(),
    }))
}

pub fn read_grok_live_settings() -> Result<Value, AppError> {
    settings_from_config_text(&read_grok_config_text()?)
}

pub fn infer_api_format_from_settings(settings: &Value) -> &'static str {
    let backend = settings
        .get("config")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<DocumentMut>().ok())
        .and_then(|doc| selected_model_table(&doc))
        .and_then(|table| {
            table
                .get("api_backend")
                .and_then(Item::as_value)
                .and_then(|value| value.as_str())
                .map(ToString::to_string)
        });
    api_format_from_backend(backend.as_deref())
}

pub fn config_text_has_proxy_placeholder(text: &str) -> bool {
    text.parse::<DocumentMut>()
        .ok()
        .and_then(|doc| {
            doc.get("model").and_then(Item::as_table).map(|models| {
                models.iter().any(|(_, item)| {
                    item.as_table()
                        .and_then(|table| table.get("api_key"))
                        .and_then(Item::as_value)
                        .and_then(|value| value.as_str())
                        == Some(GROK_PROXY_TOKEN_PLACEHOLDER)
                })
            })
        })
        .unwrap_or(false)
}

pub fn active_base_url(text: &str) -> Option<String> {
    let doc = text.parse::<DocumentMut>().ok()?;
    selected_model_table(&doc)
        .and_then(|table| {
            table
                .get("base_url")
                .and_then(Item::as_value)
                .and_then(|value| value.as_str())
                .map(ToString::to_string)
        })
        .or_else(|| {
            doc.get("endpoints")
                .and_then(|item| item.get("models_base_url"))
                .and_then(Item::as_value)
                .and_then(|value| value.as_str())
                .map(ToString::to_string)
        })
}

pub fn cleanup_takeover_config_text(text: &str) -> Result<String, AppError> {
    let mut doc = text.parse::<DocumentMut>().map_err(|e| {
        AppError::localized(
            "grok.live.invalid_toml",
            format!("Grok config.toml 格式错误: {e}"),
            format!("Invalid Grok config.toml: {e}"),
        )
    })?;
    let managed_names = doc
        .get("model")
        .and_then(Item::as_table)
        .map(|models| {
            models
                .iter()
                .filter_map(|(name, item)| {
                    let managed = item
                        .as_table()
                        .and_then(|table| table.get("api_key"))
                        .and_then(Item::as_value)
                        .and_then(|value| value.as_str())
                        == Some(GROK_PROXY_TOKEN_PLACEHOLDER);
                    managed.then(|| name.to_string())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !managed_names.is_empty() {
        if let Some(models) = doc.get_mut("model").and_then(Item::as_table_like_mut) {
            for name in &managed_names {
                models.remove(name);
            }
        }
        for (section, key) in [("models", "default"), ("models", "web_search")] {
            let selected_is_managed = doc
                .get(section)
                .and_then(|item| item.get(key))
                .and_then(Item::as_value)
                .and_then(|value| value.as_str())
                .is_some_and(|name| managed_names.iter().any(|managed| managed == name));
            if selected_is_managed {
                if let Some(table) = doc.get_mut(section).and_then(Item::as_table_like_mut) {
                    table.remove(key);
                }
            }
        }
        if let Some(routes) = doc
            .get_mut("subagents")
            .and_then(|item| item.get_mut("models"))
            .and_then(Item::as_table_like_mut)
        {
            let stale_routes = routes
                .iter()
                .filter_map(|(name, item)| {
                    item.as_value()
                        .and_then(|value| value.as_str())
                        .is_some_and(|model| managed_names.iter().any(|managed| managed == model))
                        .then(|| name.to_string())
                })
                .collect::<Vec<_>>();
            for name in stale_routes {
                routes.remove(&name);
            }
        }
        if let Some(endpoints) = doc.get_mut("endpoints").and_then(Item::as_table_like_mut) {
            endpoints.remove("models_base_url");
        }
    }
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{GrokSubagentRoute, Provider, ProviderMeta};
    use std::collections::{HashMap, HashSet};

    fn provider() -> Provider {
        let mut provider = Provider::with_id(
            "xai".to_string(),
            "xAI".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "xai-key" },
                "config": r#"[endpoints]
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
name = "Grok 4.5"
api_backend = "responses"
context_window = 500000
supports_backend_search = true

[model.search]
model = "grok-4.5"
base_url = "https://api.x.ai/v1"
api_backend = "responses"
supports_backend_search = true
"#,
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            api_format: Some("openai_responses".to_string()),
            ..Default::default()
        });
        provider
    }

    fn official_provider() -> Provider {
        let mut provider = Provider::with_id(
            "official".to_string(),
            "Grok Official".to_string(),
            json!({ "auth": {}, "config": "" }),
            None,
        );
        provider.category = Some("official".to_string());
        provider
    }

    #[test]
    fn patch_replaces_provider_sections_and_preserves_unrelated_config() {
        let existing = r#"# keep this user comment
[hints]
project_picker_disabled = true

[models]
default = "cli"
default_reasoning_effort = "high"

[model.cli]
model = "old"
base_url = "https://old.example/v1"
api_key = "old-key"
api_backend = "responses"

[mcp_servers.keep]
url = "https://example.test/mcp"
"#;
        let patched = patch_config_text_for_provider(existing, &provider(), None, None, None)
            .expect("patch Grok config");
        let doc = patched
            .parse::<DocumentMut>()
            .expect("parse patched config");

        assert_eq!(doc["models"]["default"].as_str(), Some("fast"));
        assert_eq!(doc["models"]["web_search"].as_str(), Some("search"));
        assert_eq!(doc["subagents"]["models"]["explore"].as_str(), Some("fast"));
        assert_eq!(doc["subagents"]["models"]["plan"].as_str(), Some("search"));
        assert_eq!(
            doc["models"]["default_reasoning_effort"].as_str(),
            Some("high")
        );
        assert!(doc["model"]["cli"].is_table());
        assert!(patched.contains("# keep this user comment"));
        assert_eq!(doc["model"]["fast"]["api_key"].as_str(), Some("xai-key"));
        assert_eq!(
            doc["endpoints"]["models_base_url"].as_str(),
            Some("https://api.x.ai/v1")
        );
        assert_eq!(
            doc["mcp_servers"]["keep"]["url"].as_str(),
            Some("https://example.test/mcp")
        );
    }

    #[test]
    fn merge_profile_into_global_config_preserves_unmanaged_sections() {
        let existing = r#"[features]
telemetry = false

[models]
default = "old"
default_reasoning_effort = "high"

[model.old]
model = "old-model"
"#;
        let provider = provider();
        let profile = provider.settings_config["config"]
            .as_str()
            .expect("profile config");
        let merged = merge_grok_profile_config_text(existing, profile).expect("merge profile");
        let doc = merged.parse::<DocumentMut>().expect("parse merged config");

        assert_eq!(doc["features"]["telemetry"].as_bool(), Some(false));
        assert_eq!(
            doc["models"]["default_reasoning_effort"].as_str(),
            Some("high")
        );
        assert_eq!(doc["models"]["default"].as_str(), Some("fast"));
        assert!(doc["model"]["old"].is_table());
        assert!(doc["model"]["fast"].is_table());
    }

    #[test]
    fn empty_official_profile_restores_builtin_selection_without_deleting_models() {
        let existing = r#"[models]
default = "custom"

[model.custom]
model = "grok-4.5"
api_key = "secret"
"#;
        let merged = merge_grok_profile_config_text(existing, "").expect("merge official");
        let doc = merged
            .parse::<DocumentMut>()
            .expect("parse official config");

        assert!(doc["models"].get("default").is_none());
        assert!(doc["model"]["custom"].is_table());
    }

    #[test]
    fn live_import_preserves_multi_model_profile_and_moves_shared_key_to_auth() {
        let settings = settings_from_config_text(
            r#"[endpoints]
models_base_url = "https://api.example/v1"

[models]
default = "cli"
web_search = "search"

[subagents.models]
explore = "cli"

[model.cli]
model = "grok-4.5"
base_url = "https://api.example/v1"
api_key = "secret"
api_backend = "chat_completions"

[model.search]
model = "grok-4.5-search"
base_url = "https://api.example/v1"
api_key = "secret"
api_backend = "responses"
supports_backend_search = true
"#,
        )
        .expect("import Grok config");

        assert_eq!(settings["auth"]["OPENAI_API_KEY"], "secret");
        let config = settings["config"].as_str().expect("config string");
        let config_doc = config
            .parse::<DocumentMut>()
            .expect("parse provider config");
        assert_eq!(config_doc["models"]["default"].as_str(), Some("cli"));
        assert_eq!(config_doc["models"]["web_search"].as_str(), Some("search"));
        assert_eq!(
            config_doc["subagents"]["models"]["explore"].as_str(),
            Some("cli")
        );
        assert!(config_doc["model"]["cli"].is_table());
        assert!(config_doc["model"]["search"].is_table());
        assert!(!config.contains("secret"));
        assert_eq!(infer_api_format_from_settings(&settings), "openai_chat");
    }

    #[test]
    fn takeover_routes_every_profile_model_without_changing_backend() {
        let patched = patch_config_text_for_provider(
            "[features]\ntelemetry = false\n",
            &provider(),
            Some("http://127.0.0.1:15721/grok/v1"),
            Some(GROK_PROXY_TOKEN_PLACEHOLDER),
            None,
        )
        .expect("patch takeover config");
        let doc = patched.parse::<DocumentMut>().expect("parse takeover");
        for name in ["fast", "search"] {
            assert_eq!(
                doc["model"][name]["base_url"].as_str(),
                Some("http://127.0.0.1:15721/grok/v1")
            );
            assert_eq!(
                doc["model"][name]["api_key"].as_str(),
                Some(GROK_PROXY_TOKEN_PLACEHOLDER)
            );
            assert_eq!(
                doc["model"][name]["api_backend"].as_str(),
                Some("responses")
            );
        }
        assert_eq!(doc["features"]["telemetry"].as_bool(), Some(false));
    }

    #[test]
    fn official_oauth_rejects_proxy_takeover() {
        let error = patch_config_text_for_provider(
            "[features]\ntelemetry = false\n",
            &official_provider(),
            Some("http://127.0.0.1:15721/grok/v1"),
            Some(GROK_PROXY_TOKEN_PLACEHOLDER),
            None,
        )
        .expect_err("official OAuth must not bypass takeover");

        assert!(error.to_string().contains("official OAuth"));
    }

    #[test]
    fn official_auth_removes_only_provider_owned_fields() {
        let cleaned = use_official_auth_config_text(
            r#"[features]
telemetry = false

[models]
default = "custom"
web_search = "custom"
default_reasoning_effort = "high"

[subagents.models]
explore = "grok-build"

[model.custom]
model = "grok-4.5"
api_key = "secret"
"#,
        )
        .expect("clean provider overrides");
        let doc = cleaned
            .parse::<DocumentMut>()
            .expect("parse official config");
        assert!(doc["model"]["custom"].is_table());
        assert!(doc["models"].get("default").is_none());
        assert_eq!(
            doc["models"]["default_reasoning_effort"].as_str(),
            Some("high")
        );
        assert_eq!(
            doc["subagents"]["models"]["explore"].as_str(),
            Some("grok-build")
        );
        assert_eq!(doc["features"]["telemetry"].as_bool(), Some(false));
    }

    #[test]
    fn privacy_protection_preserves_provider_profile() {
        let protected = apply_privacy_protection_config_text(
            &provider().settings_config["config"].as_str().unwrap(),
        )
        .expect("apply privacy protection");
        let doc = protected
            .parse::<DocumentMut>()
            .expect("parse protected config");
        assert_eq!(doc["features"]["telemetry"].as_bool(), Some(false));
        assert_eq!(doc["telemetry"]["trace_upload"].as_bool(), Some(false));
        assert_eq!(doc["telemetry"]["mixpanel_enabled"].as_bool(), Some(false));
        assert_eq!(
            doc["harness"]["disable_codebase_upload"].as_bool(),
            Some(true)
        );
        assert!(doc["model"]["fast"].is_table());
    }

    fn foreign_provider() -> Provider {
        let mut p = Provider::with_id(
            "openai-compat".to_string(),
            "OpenAI Compat".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "foreign-secret-key" },
                "config": r#"[endpoints]
models_base_url = "https://foreign.example/v1"

[models]
default = "cli"

[model.cli]
model = "gpt-proxy"
base_url = "https://foreign.example/v1"
api_backend = "chat_completions"
name = "Foreign CLI"
"#,
            }),
            None,
        );
        p.category = Some("third_party".to_string());
        p
    }

    fn providers_map(active: &Provider, foreign: &Provider) -> IndexMap<String, Provider> {
        let mut map = IndexMap::new();
        map.insert(active.id.clone(), active.clone());
        map.insert(foreign.id.clone(), foreign.clone());
        map
    }

    #[test]
    fn same_provider_subagent_route_uses_local_model_id() {
        let mut active = provider();
        active.meta = Some(crate::provider::ProviderMeta {
            grok_subagent_routes: HashMap::from([(
                "explore".to_string(),
                GrokSubagentRoute {
                    provider_id: Some(active.id.clone()),
                    model_id: "search".to_string(),
                },
            )]),
            ..Default::default()
        });
        let foreign = foreign_provider();
        let all = providers_map(&active, &foreign);
        let patched = patch_config_text_for_provider_with_routes(
            "[features]\ntelemetry = false\n",
            &active,
            &all,
            None,
            None,
            None,
        )
        .expect("patch");
        let doc = patched.parse::<DocumentMut>().expect("parse");
        assert_eq!(
            doc["subagents"]["models"]["explore"].as_str(),
            Some("search")
        );
        // 同源不应产生 managed 模型
        let model_names: Vec<_> = doc["model"]
            .as_table()
            .unwrap()
            .iter()
            .map(|(n, _)| n.to_string())
            .collect();
        assert!(model_names
            .iter()
            .all(|n| !n.starts_with(MANAGED_CROSS_PROVIDER_MODEL_PREFIX)));
    }

    #[test]
    fn foreign_provider_route_materializes_managed_model() {
        let mut active = provider();
        active.meta = Some(crate::provider::ProviderMeta {
            grok_subagent_routes: HashMap::from([(
                "plan".to_string(),
                GrokSubagentRoute {
                    provider_id: Some("openai-compat".to_string()),
                    model_id: "cli".to_string(),
                },
            )]),
            api_format: Some("openai_responses".to_string()),
            ..Default::default()
        });
        let foreign = foreign_provider();
        let all = providers_map(&active, &foreign);
        let patched = patch_config_text_for_provider_with_routes(
            r#"[hints]
keep = true

[model.user_custom]
model = "mine"
"#,
            &active,
            &all,
            None,
            None,
            None,
        )
        .expect("patch foreign");
        let doc = patched.parse::<DocumentMut>().expect("parse");
        let managed_id = managed_cross_provider_model_id("openai-compat", "cli", &HashSet::new());
        assert_eq!(
            doc["subagents"]["models"]["plan"].as_str(),
            Some(managed_id.as_str())
        );
        // 所有权在独立注册表，不在 [model.*] 内
        assert!(doc["model"][managed_id.as_str()]
            .as_table()
            .unwrap()
            .get(LEGACY_MANAGED_OWNER_KEY)
            .is_none());
        assert_eq!(
            doc[MANAGED_REGISTRY_ROOT][MANAGED_REGISTRY_TABLE][managed_id.as_str()]
                [MANAGED_REG_SOURCE_PROVIDER]
                .as_str(),
            Some("openai-compat")
        );
        assert_eq!(
            doc[MANAGED_REGISTRY_ROOT][MANAGED_REGISTRY_TABLE][managed_id.as_str()]
                [MANAGED_REG_SOURCE_MODEL]
                .as_str(),
            Some("cli")
        );
        assert_eq!(
            doc["model"][managed_id.as_str()]["base_url"].as_str(),
            Some("https://foreign.example/v1")
        );
        assert_eq!(
            doc["model"][managed_id.as_str()]["api_key"].as_str(),
            Some("foreign-secret-key")
        );
        // 用户无关模型保留
        assert!(doc["model"]["user_custom"].is_table());
        // 密钥不得出现在日志式断言外的标签字段
        assert!(doc["model"][managed_id.as_str()]
            .as_table()
            .unwrap()
            .get("name")
            .and_then(Item::as_value)
            .and_then(|v| v.as_str())
            .is_some_and(|n| !n.contains("foreign-secret")));
    }

    #[test]
    fn managed_ids_distinct_for_sources_that_collide_under_sanitize() {
        // 旧 sanitize 会把 a/b 与 a_b 归一成同一段，导致静默覆盖
        let id_slash = managed_cross_provider_model_id("a/b", "m", &HashSet::new());
        let id_under = managed_cross_provider_model_id("a_b", "m", &HashSet::new());
        assert_ne!(
            id_slash, id_under,
            "digest IDs must differ for a/b vs a_b providers"
        );
        assert!(id_slash.starts_with(MANAGED_CROSS_PROVIDER_MODEL_PREFIX));
        assert!(id_under.starts_with(MANAGED_CROSS_PROVIDER_MODEL_PREFIX));

        let mut active = provider();
        active.meta = Some(crate::provider::ProviderMeta {
            grok_subagent_routes: HashMap::from([
                (
                    "explore".to_string(),
                    GrokSubagentRoute {
                        provider_id: Some("a/b".to_string()),
                        model_id: "m".to_string(),
                    },
                ),
                (
                    "plan".to_string(),
                    GrokSubagentRoute {
                        provider_id: Some("a_b".to_string()),
                        model_id: "m".to_string(),
                    },
                ),
            ]),
            ..Default::default()
        });

        let mut p_slash = Provider::with_id(
            "a/b".to_string(),
            "Slash".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "slash-key" },
                "config": r#"[model.m]
model = "from-slash"
base_url = "https://slash.example/v1"
api_backend = "responses"
"#,
            }),
            None,
        );
        p_slash.category = Some("third_party".to_string());
        let mut p_under = Provider::with_id(
            "a_b".to_string(),
            "Under".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "under-key" },
                "config": r#"[model.m]
model = "from-under"
base_url = "https://under.example/v1"
api_backend = "chat_completions"
"#,
            }),
            None,
        );
        p_under.category = Some("third_party".to_string());

        let mut all = IndexMap::new();
        all.insert(active.id.clone(), active.clone());
        all.insert(p_slash.id.clone(), p_slash);
        all.insert(p_under.id.clone(), p_under);

        let patched =
            patch_config_text_for_provider_with_routes("", &active, &all, None, None, None)
                .expect("materialize both");
        let doc = patched.parse::<DocumentMut>().expect("parse");
        assert_eq!(
            doc["subagents"]["models"]["explore"].as_str(),
            Some(id_slash.as_str())
        );
        assert_eq!(
            doc["subagents"]["models"]["plan"].as_str(),
            Some(id_under.as_str())
        );
        assert_ne!(id_slash, id_under);
        assert_eq!(
            doc["model"][id_slash.as_str()]["model"].as_str(),
            Some("from-slash")
        );
        assert_eq!(
            doc["model"][id_under.as_str()]["model"].as_str(),
            Some("from-under")
        );
        assert_eq!(
            doc["model"][id_slash.as_str()]["base_url"].as_str(),
            Some("https://slash.example/v1")
        );
        assert_eq!(
            doc["model"][id_under.as_str()]["base_url"].as_str(),
            Some("https://under.example/v1")
        );
        assert!(is_managed_cross_provider_model(&doc, &id_slash));
        assert!(is_managed_cross_provider_model(&doc, &id_under));
    }

    #[test]
    fn managed_model_id_collides_with_user_model_gets_suffix() {
        let mut active = provider();
        active.meta = Some(crate::provider::ProviderMeta {
            grok_subagent_routes: HashMap::from([(
                "explore".to_string(),
                GrokSubagentRoute {
                    provider_id: Some("openai-compat".to_string()),
                    model_id: "cli".to_string(),
                },
            )]),
            ..Default::default()
        });
        let foreign = foreign_provider();
        let all = providers_map(&active, &foreign);
        let preferred = managed_cross_provider_model_id("openai-compat", "cli", &HashSet::new());
        let existing = format!(
            r#"[model.{preferred}]
model = "user-owned"
base_url = "https://user.example/v1"
"#
        );
        let patched =
            patch_config_text_for_provider_with_routes(&existing, &active, &all, None, None, None)
                .expect("patch collision");
        let doc = patched.parse::<DocumentMut>().expect("parse");
        let routed = doc["subagents"]["models"]["explore"]
            .as_str()
            .expect("route");
        assert_ne!(routed, preferred.as_str());
        assert!(routed.starts_with(&format!("{preferred}_m")));
        // 用户条目未被覆盖
        assert_eq!(
            doc["model"][preferred.as_str()]["model"].as_str(),
            Some("user-owned")
        );
        // 所有权在注册表，前缀用户模型不被当作 managed
        assert!(!is_managed_cross_provider_model(&doc, &preferred));
        assert!(is_managed_cross_provider_model(&doc, routed));
        assert_eq!(
            doc[MANAGED_REGISTRY_ROOT][MANAGED_REGISTRY_TABLE][routed][MANAGED_REG_SOURCE_PROVIDER]
                .as_str(),
            Some("openai-compat")
        );
    }

    #[test]
    fn empty_provider_map_reuses_existing_materialized_foreign_route() {
        let mut active = provider();
        active.meta = Some(crate::provider::ProviderMeta {
            grok_subagent_routes: HashMap::from([(
                "plan".to_string(),
                GrokSubagentRoute {
                    provider_id: Some("openai-compat".to_string()),
                    model_id: "cli".to_string(),
                },
            )]),
            ..Default::default()
        });
        let foreign = foreign_provider();
        let all = providers_map(&active, &foreign);
        let first = patch_config_text_for_provider_with_routes("", &active, &all, None, None, None)
            .expect("first materialize");
        // legacy wrapper：空 map 不得因缺源供应商而失败，应复用已物化条目
        let second =
            patch_config_text_for_provider(&first, &active, None, None, None).expect("empty map");
        let managed_id = managed_cross_provider_model_id("openai-compat", "cli", &HashSet::new());
        let doc = second.parse::<DocumentMut>().expect("parse");
        assert_eq!(
            doc["subagents"]["models"]["plan"].as_str(),
            Some(managed_id.as_str())
        );
        assert!(doc["model"][managed_id.as_str()].is_table());
        assert_eq!(
            doc["model"][managed_id.as_str()]["api_key"].as_str(),
            Some("foreign-secret-key")
        );
    }

    #[test]
    fn invalid_foreign_route_fails_clearly_without_local_fallback() {
        let mut active = provider();
        active.meta = Some(crate::provider::ProviderMeta {
            grok_subagent_routes: HashMap::from([(
                "explore".to_string(),
                GrokSubagentRoute {
                    provider_id: Some("missing-provider".to_string()),
                    model_id: "fast".to_string(), // 与本地同名，但不得回退
                },
            )]),
            ..Default::default()
        });
        let err = patch_config_text_for_provider_with_routes(
            "",
            &active,
            &IndexMap::new(),
            None,
            None,
            None,
        )
        .expect_err("must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("missing-provider") || msg.contains("不存在"),
            "error should mention missing provider, got: {msg}"
        );
    }

    #[test]
    fn deleted_foreign_model_fails_without_local_same_name_fallback() {
        let mut active = provider();
        active.meta = Some(crate::provider::ProviderMeta {
            grok_subagent_routes: HashMap::from([(
                "explore".to_string(),
                GrokSubagentRoute {
                    provider_id: Some("openai-compat".to_string()),
                    model_id: "fast".to_string(), // foreign 没有 fast，active 有
                },
            )]),
            ..Default::default()
        });
        let foreign = foreign_provider();
        let all = providers_map(&active, &foreign);
        let err = patch_config_text_for_provider_with_routes("", &active, &all, None, None, None)
            .expect_err("must not fall back to local fast");
        assert!(err.to_string().contains("model") || err.to_string().contains("模型"));
    }

    #[test]
    fn provider_switch_preserves_valid_cross_provider_routes() {
        let mut active = provider();
        active.meta = Some(crate::provider::ProviderMeta {
            grok_subagent_routes: HashMap::from([(
                "plan".to_string(),
                GrokSubagentRoute {
                    provider_id: Some("openai-compat".to_string()),
                    model_id: "cli".to_string(),
                },
            )]),
            ..Default::default()
        });
        let foreign = foreign_provider();
        let all = providers_map(&active, &foreign);
        let first = patch_config_text_for_provider_with_routes(
            "[features]\ntelemetry = false\n",
            &active,
            &all,
            None,
            None,
            None,
        )
        .expect("first write");
        // 切换到 foreign 再切回 active，路由仍应物化
        let second =
            patch_config_text_for_provider_with_routes(&first, &active, &all, None, None, None)
                .expect("second write");
        let managed_id = managed_cross_provider_model_id("openai-compat", "cli", &HashSet::new());
        let doc = second.parse::<DocumentMut>().expect("parse");
        assert_eq!(
            doc["subagents"]["models"]["plan"].as_str(),
            Some(managed_id.as_str())
        );
        assert!(doc["model"][managed_id.as_str()].is_table());
        assert_eq!(doc["features"]["telemetry"].as_bool(), Some(false));
    }

    #[test]
    fn official_oauth_preserves_foreign_routes_and_user_settings() {
        let mut official = official_provider();
        official.meta = Some(crate::provider::ProviderMeta {
            grok_subagent_routes: HashMap::from([(
                "explore".to_string(),
                GrokSubagentRoute {
                    provider_id: Some("openai-compat".to_string()),
                    model_id: "cli".to_string(),
                },
            )]),
            ..Default::default()
        });
        let foreign = foreign_provider();
        let mut all = IndexMap::new();
        all.insert(official.id.clone(), official.clone());
        all.insert(foreign.id.clone(), foreign);

        let existing = r#"[features]
telemetry = false

[models]
default = "custom"
default_reasoning_effort = "high"

[subagents.models]
explore = "custom"

[model.custom]
model = "grok-4.5"
api_key = "user-secret"
"#;
        let patched =
            patch_config_text_for_provider_with_routes(existing, &official, &all, None, None, None)
                .expect("official with foreign route");
        let doc = patched.parse::<DocumentMut>().expect("parse");
        assert!(doc["models"].get("default").is_none());
        assert_eq!(
            doc["models"]["default_reasoning_effort"].as_str(),
            Some("high")
        );
        assert!(doc["model"]["custom"].is_table());
        assert_eq!(doc["features"]["telemetry"].as_bool(), Some(false));
        let managed_id = managed_cross_provider_model_id("openai-compat", "cli", &HashSet::new());
        assert_eq!(
            doc["subagents"]["models"]["explore"].as_str(),
            Some(managed_id.as_str())
        );
        assert!(doc["model"][managed_id.as_str()].is_table());
    }

    #[test]
    fn cleanup_only_removes_owned_managed_models() {
        let mut active = provider();
        // 先写入 foreign 路由
        active.meta = Some(crate::provider::ProviderMeta {
            grok_subagent_routes: HashMap::from([(
                "plan".to_string(),
                GrokSubagentRoute {
                    provider_id: Some("openai-compat".to_string()),
                    model_id: "cli".to_string(),
                },
            )]),
            ..Default::default()
        });
        let foreign = foreign_provider();
        let all = providers_map(&active, &foreign);
        let with_managed = patch_config_text_for_provider_with_routes(
            r#"[model.ccswitch_xprov_user]
model = "looks-like-managed"
# 无所有权标记，不得清理
"#,
            &active,
            &all,
            None,
            None,
            None,
        )
        .expect("with managed");

        // 清除路由后再写：应删除 owned managed，保留无标记的同前缀用户模型
        active.meta = Some(crate::provider::ProviderMeta {
            grok_subagent_routes: HashMap::new(),
            ..Default::default()
        });
        let cleaned = patch_config_text_for_provider_with_routes(
            &with_managed,
            &active,
            &all,
            None,
            None,
            None,
        )
        .expect("cleanup");
        let doc = cleaned.parse::<DocumentMut>().expect("parse");
        let managed_id = managed_cross_provider_model_id("openai-compat", "cli", &HashSet::new());
        assert!(doc
            .get("model")
            .and_then(|m| m.get(managed_id.as_str()))
            .is_none());
        assert!(doc["model"]["ccswitch_xprov_user"].is_table());
    }

    /// 回归：Profile TOML 无 `[subagents.models]` 时，清空 meta 外源路由必须
    /// 真正摘掉 live 陈旧角色、managed 模型与注册表项；用户路由/模型保留。
    #[test]
    fn clearing_meta_routes_removes_stale_foreign_role_when_profile_has_no_subagents() {
        // 刻意不含 [subagents.models]，模拟 merge_profile_doc 不会覆盖 live 路由
        let mut active = Provider::with_id(
            "xai".to_string(),
            "xAI".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "xai-key" },
                "config": r#"[endpoints]
models_base_url = "https://api.x.ai/v1"

[models]
default = "fast"

[model.fast]
model = "grok-4.5"
base_url = "https://api.x.ai/v1"
api_backend = "responses"
"#,
            }),
            None,
        );
        active.meta = Some(ProviderMeta {
            api_format: Some("openai_responses".to_string()),
            grok_subagent_routes: HashMap::from([(
                "plan".to_string(),
                GrokSubagentRoute {
                    provider_id: Some("openai-compat".to_string()),
                    model_id: "cli".to_string(),
                },
            )]),
            ..Default::default()
        });
        let foreign = foreign_provider();
        let all = providers_map(&active, &foreign);

        // live 预先有一条用户自有路由/模型，清理后必须仍在
        let existing = r#"[subagents.models]
user_role = "user_custom"

[model.user_custom]
model = "mine"
base_url = "https://user.example/v1"
"#;
        let with_foreign =
            patch_config_text_for_provider_with_routes(existing, &active, &all, None, None, None)
                .expect("write foreign route");
        let managed_id = managed_cross_provider_model_id("openai-compat", "cli", &HashSet::new());
        {
            let doc = with_foreign
                .parse::<DocumentMut>()
                .expect("parse with foreign");
            assert_eq!(
                doc["subagents"]["models"]["plan"].as_str(),
                Some(managed_id.as_str()),
                "precondition: foreign role materialized"
            );
            assert!(doc["model"][managed_id.as_str()].is_table());
            assert!(
                doc[MANAGED_REGISTRY_ROOT][MANAGED_REGISTRY_TABLE][managed_id.as_str()].is_table()
            );
            assert_eq!(
                doc["subagents"]["models"]["user_role"].as_str(),
                Some("user_custom")
            );
        }

        // 清空 meta；Profile 仍无 subagents 段 → merge 不会覆盖 live 路由
        active.meta = Some(ProviderMeta {
            api_format: Some("openai_responses".to_string()),
            grok_subagent_routes: HashMap::new(),
            ..Default::default()
        });
        let cleaned = patch_config_text_for_provider_with_routes(
            &with_foreign,
            &active,
            &all,
            None,
            None,
            None,
        )
        .expect("clear meta routes");
        let doc = cleaned.parse::<DocumentMut>().expect("parse cleaned");

        // 陈旧外源角色、managed 模型、注册表项全部消失
        assert!(
            doc.get("subagents")
                .and_then(|s| s.get("models"))
                .and_then(|m| m.get("plan"))
                .is_none(),
            "stale foreign role must be removed when meta routes are empty"
        );
        assert!(
            doc.get("model")
                .and_then(|m| m.get(managed_id.as_str()))
                .is_none(),
            "managed foreign model must be deleted"
        );
        assert!(
            doc.get(MANAGED_REGISTRY_ROOT)
                .and_then(|r| r.get(MANAGED_REGISTRY_TABLE))
                .and_then(|t| t.get(managed_id.as_str()))
                .is_none(),
            "registry entry must be deleted"
        );
        // 无关用户路由/模型保留
        assert_eq!(
            doc["subagents"]["models"]["user_role"].as_str(),
            Some("user_custom")
        );
        assert!(doc["model"]["user_custom"].is_table());
        assert_eq!(doc["model"]["user_custom"]["model"].as_str(), Some("mine"));
    }

    #[test]
    fn strip_collapses_empty_managed_registry_when_no_owned_models() {
        // managed_names 为空时仍应收起空注册表（原先 names=&[] 直接 return 会漏掉）
        let text = format!(
            r#"[model.cli]
model = "grok-4.5"
base_url = "https://api.example/v1"
api_key = "secret"

[{MANAGED_REGISTRY_ROOT}.{MANAGED_REGISTRY_TABLE}]
"#
        );
        let mut doc = text.parse::<DocumentMut>().expect("parse");
        assert!(
            doc.get(MANAGED_REGISTRY_ROOT).is_some(),
            "precondition: empty registry root present"
        );
        assert!(
            list_owned_managed_model_names(&doc).is_empty(),
            "precondition: no owned managed models"
        );
        strip_managed_models_from_doc(&mut doc);
        assert!(
            doc.get(MANAGED_REGISTRY_ROOT).is_none(),
            "empty registry must collapse when managed_names is empty"
        );
        assert!(doc["model"]["cli"].is_table());
        assert_eq!(doc["model"]["cli"]["api_key"].as_str(), Some("secret"));
    }

    #[test]
    fn live_import_strips_managed_models_and_keeps_secrets_out_of_config() {
        let managed_id = managed_cross_provider_model_id("openai-compat", "cli", &HashSet::new());
        let settings = settings_from_config_text(&format!(
            r#"[endpoints]
models_base_url = "https://api.example/v1"

[models]
default = "cli"

[subagents.models]
explore = "{managed_id}"
plan = "cli"

[model.cli]
model = "grok-4.5"
base_url = "https://api.example/v1"
api_key = "secret"
api_backend = "responses"

[model.{managed_id}]
model = "gpt-proxy"
base_url = "https://foreign.example/v1"
api_key = "foreign-secret-key"
api_backend = "chat_completions"

[{MANAGED_REGISTRY_ROOT}.{MANAGED_REGISTRY_TABLE}.{managed_id}]
{MANAGED_REG_SOURCE_PROVIDER} = "openai-compat"
{MANAGED_REG_SOURCE_MODEL} = "cli"
"#
        ))
        .expect("import");
        let config = settings["config"].as_str().expect("config");
        assert!(!config.contains(&managed_id));
        assert!(!config.contains("foreign-secret-key"));
        assert!(!config.contains("secret"));
        assert!(!config.contains(MANAGED_REGISTRY_ROOT));
        assert!(config.contains("[model.cli]"));
        let config_doc = config.parse::<DocumentMut>().expect("parse");
        // managed 路由被剥离；同源 plan 保留
        assert!(config_doc
            .get("subagents")
            .and_then(|s| s.get("models"))
            .and_then(|m| m.get("explore"))
            .is_none());
        assert_eq!(
            config_doc["subagents"]["models"]["plan"].as_str(),
            Some("cli")
        );
    }

    #[test]
    fn ownership_not_inferred_from_prefix_alone() {
        let preferred = managed_cross_provider_model_id("openai-compat", "cli", &HashSet::new());
        let text = format!(
            r#"[model.{preferred}]
model = "user-lookalike"
base_url = "https://user.example/v1"
"#
        );
        let doc = text.parse::<DocumentMut>().expect("parse");
        assert!(!is_managed_cross_provider_model(&doc, &preferred));
        assert!(!list_owned_managed_model_names(&doc).contains(&preferred));
    }

    /// 兼容性说明测试：注册表字段位于 Grok 原生 `[model.*]` 之外。
    /// xAI settings 文档未列出 `cc_switch_*` model 字段；我们不写入它们。
    #[test]
    fn managed_models_use_external_registry_not_model_table_markers() {
        let mut active = provider();
        active.meta = Some(crate::provider::ProviderMeta {
            grok_subagent_routes: HashMap::from([(
                "plan".to_string(),
                GrokSubagentRoute {
                    provider_id: Some("openai-compat".to_string()),
                    model_id: "cli".to_string(),
                },
            )]),
            ..Default::default()
        });
        let foreign = foreign_provider();
        let all = providers_map(&active, &foreign);
        let patched =
            patch_config_text_for_provider_with_routes("", &active, &all, None, None, None)
                .expect("patch");
        assert!(
            !patched.contains(LEGACY_MANAGED_OWNER_KEY),
            "must not write legacy owner keys into model tables"
        );
        assert!(
            !patched.contains(LEGACY_MANAGED_SOURCE_PROVIDER_KEY),
            "must not write legacy source keys into model tables"
        );
        assert!(patched.contains(MANAGED_REGISTRY_ROOT));
        assert!(patched.contains(MANAGED_REGISTRY_TABLE));
    }

    #[test]
    fn takeover_does_not_rewrite_foreign_managed_models() {
        let mut active = provider();
        active.meta = Some(crate::provider::ProviderMeta {
            grok_subagent_routes: HashMap::from([(
                "plan".to_string(),
                GrokSubagentRoute {
                    provider_id: Some("openai-compat".to_string()),
                    model_id: "cli".to_string(),
                },
            )]),
            ..Default::default()
        });
        let foreign = foreign_provider();
        let all = providers_map(&active, &foreign);
        let patched = patch_config_text_for_provider_with_routes(
            "",
            &active,
            &all,
            Some("http://127.0.0.1:15721/grok/v1"),
            Some(GROK_PROXY_TOKEN_PLACEHOLDER),
            None,
        )
        .expect("takeover");
        let doc = patched.parse::<DocumentMut>().expect("parse");
        assert_eq!(
            doc["model"]["fast"]["base_url"].as_str(),
            Some("http://127.0.0.1:15721/grok/v1")
        );
        assert_eq!(
            doc["model"]["fast"]["api_key"].as_str(),
            Some(GROK_PROXY_TOKEN_PLACEHOLDER)
        );
        let managed_id = managed_cross_provider_model_id("openai-compat", "cli", &HashSet::new());
        assert_eq!(
            doc["model"][managed_id.as_str()]["base_url"].as_str(),
            Some("https://foreign.example/v1")
        );
        assert_eq!(
            doc["model"][managed_id.as_str()]["api_key"].as_str(),
            Some("foreign-secret-key")
        );
    }
}
