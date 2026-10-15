//! Server catalogs: the official MCP Registry and Archestra's catalog.
//!
//! Both are normalized into install options: a transport template whose values may hold
//! `{{key}}` placeholders, plus the inputs that fill them. `render` turns an option and the
//! user's values into a `TransportView` that the app adds with `server.add`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::{KeyValue, TransportView};

const OFFICIAL_URL: &str = "https://registry.modelcontextprotocol.io/v0/servers";
const ARCHESTRA_URL: &str = "https://archestra.ai/mcp-catalog/api/search";
const PAGE_SIZE: usize = 30;
const CACHE_TTL: Duration = Duration::from_secs(600);
const CACHE_LIMIT: usize = 200;
/// Joins a named flag and its value into one template arg, so an empty optional value
/// drops both.
const ARG_JOIN: char = '\u{1f}';

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    Official,
    Archestra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub id: String,
    pub source: Source,
    pub name: String,
    pub title: String,
    pub description: String,
    pub version: Option<String>,
    pub repository: Option<String>,
    pub website: Option<String>,
    pub stars: Option<u64>,
    pub category: Option<String>,
    pub options: Vec<InstallOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallOption {
    pub id: String,
    pub label: String,
    pub template: TransportView,
    pub inputs: Vec<InputField>,
    /// The catalog says this remote server signs in through the browser.
    #[serde(default)]
    pub oauth: bool,
    pub note: Option<String>,
    /// Why Tulma cannot launch this option; such options are listed but not installable.
    pub unsupported: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InputKind {
    #[default]
    String,
    Number,
    Boolean,
    Directory,
    File,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputField {
    pub key: String,
    pub label: String,
    pub description: Option<String>,
    pub required: bool,
    pub secret: bool,
    pub default: Option<String>,
    #[serde(default)]
    pub kind: InputKind,
    #[serde(default)]
    pub choices: Vec<String>,
    /// One value per line, each becoming its own argument.
    #[serde(default)]
    pub multiple: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogPage {
    pub entries: Vec<CatalogEntry>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rendered {
    pub transport: TransportView,
    /// Required inputs that are still empty; their placeholders stay in the output.
    pub missing: Vec<String>,
}

// Fetching

type PageKey = (Source, String, Option<String>);

pub struct Registry {
    http: reqwest::Client,
    cache: Mutex<HashMap<PageKey, (Instant, CatalogPage)>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("Tulma/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            .build()
            .expect("reqwest client with static settings");
        Self { http, cache: Mutex::new(HashMap::new()) }
    }

    pub async fn search(&self, source: Source, query: &str, cursor: Option<&str>) -> Result<CatalogPage> {
        let query = query.trim().to_string();
        let key = (source, query.clone(), cursor.map(str::to_string));
        if let Some((at, page)) = self.cache.lock().unwrap().get(&key)
            && at.elapsed() < CACHE_TTL
        {
            return Ok(page.clone());
        }
        let page = match source {
            Source::Official => self.official(&query, cursor).await?,
            Source::Archestra => self.archestra(&query, cursor).await?,
        };
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= CACHE_LIMIT {
            cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        }
        cache.insert(key, (Instant::now(), page.clone()));
        Ok(page)
    }

    async fn official(&self, query: &str, cursor: Option<&str>) -> Result<CatalogPage> {
        let mut params = vec![("limit", PAGE_SIZE.to_string()), ("version", "latest".into())];
        if !query.is_empty() {
            params.push(("search", query.into()));
        }
        if let Some(cursor) = cursor {
            params.push(("cursor", cursor.into()));
        }
        let url = reqwest::Url::parse_with_params(OFFICIAL_URL, &params)?;
        Ok(parse_official(self.get(url).await.context("official MCP registry")?))
    }

    async fn archestra(&self, query: &str, cursor: Option<&str>) -> Result<CatalogPage> {
        let offset: usize = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
        let mut params = vec![("limit", PAGE_SIZE.to_string()), ("offset", offset.to_string())];
        if !query.is_empty() {
            params.push(("q", query.into()));
        }
        let url = reqwest::Url::parse_with_params(ARCHESTRA_URL, &params)?;
        Ok(parse_archestra(self.get(url).await.context("Archestra catalog")?, offset))
    }

    async fn get<T: DeserializeOwned>(&self, url: reqwest::Url) -> Result<T> {
        let response = self.http.get(url).send().await?;
        let status = response.status();
        let body = response.bytes().await?;
        if !status.is_success() {
            bail!("HTTP {status}: {}", String::from_utf8_lossy(&body[..body.len().min(300)]));
        }
        serde_json::from_slice(&body).context("unexpected response")
    }
}

// Rendering

fn placeholder(key: &str) -> String {
    format!("{{{{{key}}}}}")
}

/// Splits `text` into literal and placeholder pieces.
fn pieces(text: &str) -> Vec<Result<&str, &str>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start + 2..].find("}}") else { break };
        if start > 0 {
            out.push(Ok(&rest[..start]));
        }
        out.push(Err(&rest[start + 2..start + 2 + len]));
        rest = &rest[start + 4 + len..];
    }
    if !rest.is_empty() {
        out.push(Ok(rest));
    }
    out
}

fn secret_env_name(key: &str) -> String {
    let cleaned: String =
        key.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    format!("TULMA_{cleaned}")
}

struct Renderer<'a> {
    inputs: HashMap<&'a str, &'a InputField>,
    values: &'a BTreeMap<String, String>,
    missing: Vec<String>,
    /// Secret values pulled out of args into env vars, so they never sit in the config.
    hidden: Vec<KeyValue>,
}

enum Filled {
    Text(String),
    /// An optional input left empty: the arg, env var or header is dropped.
    Skip,
}

impl<'a> Renderer<'a> {
    fn value(&self, key: &str) -> Option<String> {
        let typed = self.values.get(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        typed.or_else(|| self.inputs.get(key).and_then(|i| i.default.clone()).filter(|v| !v.is_empty()))
    }

    fn fill(&mut self, text: &str, hide_secrets: bool) -> Filled {
        let mut out = String::new();
        for piece in pieces(text) {
            match piece {
                Ok(literal) => out.push_str(literal),
                Err(key) => {
                    let input = self.inputs.get(key).copied();
                    match self.value(key) {
                        Some(value) if hide_secrets && input.is_some_and(|i| i.secret) => {
                            let name = secret_env_name(key);
                            if !self.hidden.iter().any(|kv| kv.key == name) {
                                self.hidden.push(KeyValue { key: name.clone(), value, secret: true });
                            }
                            out.push_str(&format!("${{{name}}}"));
                        }
                        Some(value) => out.push_str(&value),
                        None if input.is_some_and(|i| !i.required) => return Filled::Skip,
                        None => {
                            if !self.missing.iter().any(|m| m == key) {
                                self.missing.push(key.to_string());
                            }
                            out.push_str(&placeholder(key));
                        }
                    }
                }
            }
        }
        Filled::Text(out)
    }

    fn args(&mut self, template: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for arg in template {
            if let [Err(key)] = pieces(arg).as_slice()
                && self.inputs.get(key).is_some_and(|i| i.multiple)
                && let Some(value) = self.value(key)
            {
                out.extend(value.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string));
                continue;
            }
            let mut group = Vec::new();
            for part in arg.split(ARG_JOIN) {
                match self.fill(part, true) {
                    Filled::Text(text) => group.push(text),
                    Filled::Skip => {
                        group.clear();
                        break;
                    }
                }
            }
            out.extend(group);
        }
        out
    }

    fn pairs(&mut self, template: &[KeyValue]) -> Vec<KeyValue> {
        template
            .iter()
            .filter_map(|kv| {
                let secret = kv.secret || pieces(&kv.value).iter().any(|p| matches!(p, Err(k) if self.inputs.get(k).is_some_and(|i| i.secret)));
                match self.fill(&kv.value, false) {
                    Filled::Text(value) if !value.is_empty() => Some(KeyValue { key: kv.key.clone(), value, secret }),
                    _ => None,
                }
            })
            .collect()
    }
}

pub fn render(option: &InstallOption, values: &BTreeMap<String, String>) -> Rendered {
    let mut r = Renderer {
        inputs: option.inputs.iter().map(|i| (i.key.as_str(), i)).collect(),
        values,
        missing: Vec::new(),
        hidden: Vec::new(),
    };
    let transport = match &option.template {
        TransportView::Stdio { command, args, env, cwd } => {
            let command = match r.fill(command, false) {
                Filled::Text(c) => c,
                Filled::Skip => String::new(),
            };
            let args = r.args(args);
            let mut env = r.pairs(env);
            for kv in std::mem::take(&mut r.hidden) {
                if !env.iter().any(|e| e.key == kv.key) {
                    env.push(kv);
                }
            }
            TransportView::Stdio { command, args, env, cwd: cwd.clone() }
        }
        TransportView::Http { url, headers } => {
            let url = match r.fill(url, false) {
                Filled::Text(u) => u,
                Filled::Skip => String::new(),
            };
            TransportView::Http { url, headers: r.pairs(headers) }
        }
    };
    let missing = r.missing;
    Rendered { transport, missing }
}

// Shared helpers

fn text(value: &Option<Value>) -> Option<String> {
    match value.as_ref()? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Catalog JSON often has `null` where a list or map is expected.
fn nullable<'de, D: serde::Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Option::<T>::deserialize(d).map(Option::unwrap_or_default)
}

fn kind_of(format: Option<&str>) -> InputKind {
    match format.unwrap_or_default() {
        "number" | "integer" => InputKind::Number,
        "boolean" => InputKind::Boolean,
        "directory" => InputKind::Directory,
        "file" | "filepath" => InputKind::File,
        _ => InputKind::String,
    }
}

fn non_empty(s: Option<&str>) -> Option<String> {
    s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn push_input(inputs: &mut Vec<InputField>, field: InputField) {
    if !inputs.iter().any(|i| i.key == field.key) {
        inputs.push(field);
    }
}

fn unique_key(base: &str, taken: &mut HashSet<String>) -> String {
    let base = if base.is_empty() { "value" } else { base };
    let key = (1..)
        .map(|n| if n == 1 { base.to_string() } else { format!("{base}_{n}") })
        .find(|k| !taken.contains(k))
        .expect("unbounded range always yields a free key");
    taken.insert(key.clone());
    key
}

// Official MCP Registry (server.json)

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct OfficialList {
    #[serde(deserialize_with = "nullable")]
    servers: Vec<OfficialItem>,
    metadata: OfficialMeta,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct OfficialMeta {
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct OfficialItem {
    server: OfficialServer,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct OfficialServer {
    name: String,
    title: Option<String>,
    description: String,
    version: Option<String>,
    repository: Option<Repository>,
    website_url: Option<String>,
    #[serde(deserialize_with = "nullable")]
    packages: Vec<Package>,
    #[serde(deserialize_with = "nullable")]
    remotes: Vec<Remote>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Repository {
    url: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Package {
    registry_type: String,
    identifier: String,
    version: Option<String>,
    transport: PackageTransport,
    #[serde(deserialize_with = "nullable")]
    runtime_arguments: Vec<Argument>,
    #[serde(deserialize_with = "nullable")]
    package_arguments: Vec<Argument>,
    #[serde(deserialize_with = "nullable")]
    environment_variables: Vec<KeyValueInput>,
}

#[derive(Deserialize)]
#[serde(default)]
struct PackageTransport {
    r#type: String,
}

impl Default for PackageTransport {
    fn default() -> Self {
        Self { r#type: "stdio".into() }
    }
}

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
struct InputSpec {
    description: Option<String>,
    is_required: bool,
    is_secret: bool,
    default: Option<Value>,
    format: Option<String>,
    #[serde(deserialize_with = "nullable")]
    choices: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Argument {
    r#type: String,
    name: Option<String>,
    value: Option<String>,
    value_hint: Option<String>,
    is_repeated: bool,
    #[serde(deserialize_with = "nullable")]
    variables: BTreeMap<String, InputSpec>,
    #[serde(flatten)]
    spec: InputSpec,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct KeyValueInput {
    name: String,
    value: Option<String>,
    #[serde(deserialize_with = "nullable")]
    variables: BTreeMap<String, InputSpec>,
    #[serde(flatten)]
    spec: InputSpec,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Remote {
    r#type: String,
    url: String,
    #[serde(deserialize_with = "nullable")]
    headers: Vec<KeyValueInput>,
    #[serde(deserialize_with = "nullable")]
    variables: BTreeMap<String, InputSpec>,
}

fn spec_field(key: String, label: String, spec: &InputSpec, multiple: bool) -> InputField {
    InputField {
        key,
        label,
        description: non_empty(spec.description.as_deref()),
        required: spec.is_required,
        secret: spec.is_secret,
        default: text(&spec.default),
        kind: kind_of(spec.format.as_deref()),
        choices: spec.choices.clone(),
        multiple,
    }
}

/// Rewrites `{var}` references into placeholders and registers each variable as an input.
fn with_variables(
    value: &str,
    prefix: &str,
    variables: &BTreeMap<String, InputSpec>,
    secret: bool,
    inputs: &mut Vec<InputField>,
) -> String {
    let mut out = value.to_string();
    for (var, spec) in variables {
        let token = format!("{{{var}}}");
        if !out.contains(&token) {
            continue;
        }
        let key = if prefix.is_empty() { var.clone() } else { format!("{prefix}.{var}") };
        out = out.replace(&token, &placeholder(&key));
        let mut field = spec_field(key, var.clone(), spec, false);
        field.secret |= secret;
        push_input(inputs, field);
    }
    out
}

fn kv_template(kv: &KeyValueInput, inputs: &mut Vec<InputField>) -> KeyValue {
    let value = match &kv.value {
        Some(v) if !kv.variables.is_empty() => with_variables(v, &kv.name, &kv.variables, kv.spec.is_secret, inputs),
        Some(v) => v.clone(),
        None => {
            push_input(inputs, spec_field(kv.name.clone(), kv.name.clone(), &kv.spec, false));
            placeholder(&kv.name)
        }
    };
    KeyValue { key: kv.name.clone(), value, secret: kv.spec.is_secret }
}

fn arg_templates(args: &[Argument], taken: &mut HashSet<String>, inputs: &mut Vec<InputField>) -> Vec<String> {
    let mut out = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        let named = arg.r#type == "named";
        let flag = arg.name.as_deref().unwrap_or_default();
        let base = non_empty(arg.value_hint.as_deref())
            .or_else(|| non_empty(Some(flag.trim_start_matches('-'))))
            .unwrap_or_else(|| format!("arg{}", i + 1));
        let value = match &arg.value {
            Some(v) if !arg.variables.is_empty() => {
                Some(with_variables(v, &unique_key(&base, taken), &arg.variables, arg.spec.is_secret, inputs))
            }
            Some(v) => Some(v.clone()),
            None if named && arg.value_hint.is_none() && arg.spec.default.is_none() && !arg.spec.is_required => None,
            None => {
                let key = unique_key(&base, taken);
                let label = non_empty(arg.value_hint.as_deref()).unwrap_or_else(|| flag.to_string());
                let label = if label.is_empty() { key.clone() } else { label };
                push_input(inputs, spec_field(key.clone(), label, &arg.spec, arg.is_repeated && !named));
                Some(placeholder(&key))
            }
        };
        match (named, value) {
            (true, Some(v)) => out.push(format!("{flag}{ARG_JOIN}{v}")),
            (true, None) => out.push(flag.to_string()),
            (false, Some(v)) => out.push(v),
            (false, None) => {}
        }
    }
    out
}

fn package_option(p: &Package) -> InstallOption {
    let mut inputs = Vec::new();
    let mut taken: HashSet<String> = p.environment_variables.iter().map(|e| e.name.clone()).collect();
    let env: Vec<KeyValue> = p.environment_variables.iter().map(|kv| kv_template(kv, &mut inputs)).collect();
    let runtime = arg_templates(&p.runtime_arguments, &mut taken, &mut inputs);
    let package = arg_templates(&p.package_arguments, &mut taken, &mut inputs);
    let version = p.version.as_deref().filter(|v| !v.is_empty() && *v != "latest");
    let ident = p.identifier.as_str();
    let mut unsupported = None;
    let (command, args): (&str, Vec<String>) = match p.registry_type.as_str() {
        "npm" => {
            let pkg = version.map_or(ident.to_string(), |v| format!("{ident}@{v}"));
            ("npx", [vec!["-y".into()], runtime, vec![pkg], package].concat())
        }
        "pypi" => {
            let pkg = version.map_or(ident.to_string(), |v| format!("{ident}=={v}"));
            ("uvx", [runtime, vec![pkg], package].concat())
        }
        "oci" => {
            let tagged = ident.rsplit('/').next().is_some_and(|last| last.contains(':') || last.contains('@'));
            let image = match version {
                Some(v) if !tagged => format!("{ident}:{v}"),
                _ => ident.to_string(),
            };
            let pass_env = env.iter().flat_map(|kv| ["-e".to_string(), kv.key.clone()]).collect();
            ("docker", [vec!["run".into(), "-i".into(), "--rm".into()], pass_env, runtime, vec![image], package].concat())
        }
        "nuget" => {
            let pkg = version.map_or(ident.to_string(), |v| format!("{ident}@{v}"));
            ("dnx", [vec![pkg, "--yes".into()], runtime, vec!["--".into()], package].concat())
        }
        other => {
            unsupported = Some(format!("{other} packages are not supported"));
            ("", Vec::new())
        }
    };
    if unsupported.is_none() && p.transport.r#type != "stdio" {
        unsupported = Some("This package runs its own HTTP server. Start it yourself and add its URL.".into());
    }
    let label = match version {
        Some(v) => format!("{} · {ident}@{v}", p.registry_type),
        None => format!("{} · {ident}", p.registry_type),
    };
    InstallOption {
        id: format!("{}:{ident}", p.registry_type),
        label,
        template: TransportView::Stdio { command: command.into(), args, env, cwd: None },
        inputs,
        oauth: false,
        note: None,
        unsupported,
    }
}

/// Tulma speaks Streamable HTTP; legacy SSE servers run through `mcp-remote`.
fn mcp_remote_option(id: String, url: String, headers: Vec<KeyValue>, inputs: Vec<InputField>) -> InstallOption {
    let mut args = vec!["-y".to_string(), "mcp-remote".into(), url];
    for kv in &headers {
        args.push(format!("--header{ARG_JOIN}{}:{}", kv.key, kv.value));
    }
    InstallOption {
        id,
        label: "Remote · SSE via mcp-remote".into(),
        template: TransportView::Stdio { command: "npx".into(), args, env: Vec::new(), cwd: None },
        inputs,
        oauth: false,
        note: Some("Legacy SSE server, bridged by mcp-remote. If it needs a login, mcp-remote opens the browser itself.".into()),
        unsupported: None,
    }
}

fn remote_option(r: &Remote, index: usize) -> InstallOption {
    let mut inputs = Vec::new();
    let url = with_variables(&r.url, "", &r.variables, false, &mut inputs);
    let headers: Vec<KeyValue> = r.headers.iter().map(|kv| kv_template(kv, &mut inputs)).collect();
    let id = format!("remote:{index}");
    match r.r#type.as_str() {
        "sse" => mcp_remote_option(id, url, headers, inputs),
        "streamable-http" | "http" => InstallOption {
            id,
            label: "Remote · Streamable HTTP".into(),
            template: TransportView::Http { url, headers },
            inputs,
            oauth: false,
            note: None,
            unsupported: None,
        },
        other => InstallOption {
            id,
            label: format!("Remote · {other}"),
            template: TransportView::Http { url, headers },
            inputs,
            oauth: false,
            note: None,
            unsupported: Some(format!("{other} remotes are not supported")),
        },
    }
}

fn official_entry(s: OfficialServer) -> CatalogEntry {
    let mut options: Vec<InstallOption> = s.remotes.iter().enumerate().map(|(i, r)| remote_option(r, i)).collect();
    options.extend(s.packages.iter().map(package_option));
    options.sort_by_key(|o| o.unsupported.is_some());
    let short = s.name.rsplit('/').next().unwrap_or(&s.name).to_string();
    CatalogEntry {
        id: format!("official:{}", s.name),
        source: Source::Official,
        title: non_empty(s.title.as_deref()).unwrap_or(short),
        name: s.name,
        description: s.description,
        version: s.version,
        repository: s.repository.and_then(|r| r.url).filter(|u| !u.is_empty()),
        website: s.website_url.filter(|u| !u.is_empty()),
        stars: None,
        category: None,
        options,
    }
}

fn parse_official(list: OfficialList) -> CatalogPage {
    let mut seen = HashSet::new();
    let entries = list
        .servers
        .into_iter()
        .filter(|item| seen.insert(item.server.name.clone()))
        .map(|item| official_entry(item.server))
        .collect();
    CatalogPage { entries, next_cursor: list.metadata.next_cursor.filter(|c| !c.is_empty()) }
}

// Archestra catalog

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct ArchestraList {
    #[serde(deserialize_with = "nullable")]
    servers: Vec<ArchestraServer>,
    has_more: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ArchestraServer {
    name: String,
    display_name: Option<String>,
    description: Option<String>,
    server: ArchestraLaunch,
    #[serde(deserialize_with = "nullable")]
    user_config: BTreeMap<String, ArchestraField>,
    archestra_config: Option<ArchestraConfig>,
    github_info: Option<GithubInfo>,
    category: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ArchestraLaunch {
    r#type: String,
    command: Option<String>,
    #[serde(deserialize_with = "nullable")]
    args: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    env: BTreeMap<String, String>,
    url: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ArchestraField {
    r#type: String,
    title: Option<String>,
    description: Option<String>,
    required: bool,
    default: Option<Value>,
    sensitive: bool,
    multiple: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ArchestraConfig {
    oauth: Option<ArchestraOAuth>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ArchestraOAuth {
    required: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GithubInfo {
    url: Option<String>,
    stars: Option<u64>,
}

const USER_CONFIG: &str = "${user_config.";

/// `${user_config.x}` → `{{x}}`, collecting the keys it used.
fn archestra_template(value: &str, used: &mut Vec<String>) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find(USER_CONFIG) {
        let Some(len) = rest[start + USER_CONFIG.len()..].find('}') else { break };
        let key = &rest[start + USER_CONFIG.len()..start + USER_CONFIG.len() + len];
        out.push_str(&rest[..start]);
        out.push_str(&placeholder(key));
        if !used.iter().any(|u| u == key) {
            used.push(key.to_string());
        }
        rest = &rest[start + USER_CONFIG.len() + len + 1..];
    }
    out.push_str(rest);
    out
}

/// Archestra defaults reference its own variables; keep the ones that mean a fixed folder.
fn archestra_default(value: &Option<Value>) -> Option<String> {
    if let Some(Value::Array(items)) = value {
        let kept: Vec<String> = items.iter().filter_map(|v| archestra_default(&Some(v.clone()))).collect();
        return (!kept.is_empty()).then(|| kept.join("\n"));
    }
    let raw = text(value)?;
    if !raw.contains("${") {
        return Some(raw);
    }
    let home = dirs::home_dir()?.display().to_string();
    let mut out = raw.replace("${HOME}", &home);
    for (var, dir) in [("${DESKTOP}", "Desktop"), ("${DOCUMENTS}", "Documents"), ("${DOWNLOADS}", "Downloads")] {
        out = out.replace(var, &format!("{home}/{dir}"));
    }
    (!out.contains("${")).then_some(out)
}

fn archestra_inputs(fields: &BTreeMap<String, ArchestraField>, used: &[String]) -> Vec<InputField> {
    used.iter()
        .map(|key| match fields.get(key) {
            Some(f) => InputField {
                key: key.clone(),
                label: non_empty(f.title.as_deref()).unwrap_or_else(|| key.clone()),
                description: non_empty(f.description.as_deref()),
                required: f.required,
                secret: f.sensitive,
                default: archestra_default(&f.default),
                kind: kind_of(Some(f.r#type.as_str())),
                choices: Vec::new(),
                multiple: f.multiple,
            },
            None => InputField {
                key: key.clone(),
                label: key.clone(),
                description: None,
                required: true,
                secret: false,
                default: None,
                kind: InputKind::String,
                choices: Vec::new(),
                multiple: false,
            },
        })
        .collect()
}

fn archestra_option(s: &ArchestraServer) -> InstallOption {
    let mut used = Vec::new();
    let oauth = s.archestra_config.as_ref().and_then(|c| c.oauth.as_ref()).is_some_and(|o| o.required);
    if s.server.r#type == "remote" {
        let url = archestra_template(s.server.url.as_deref().unwrap_or_default(), &mut used);
        let inputs = archestra_inputs(&s.user_config, &used);
        if url.trim_end_matches('/').ends_with("/sse") {
            return mcp_remote_option("remote".into(), url, Vec::new(), inputs);
        }
        return InstallOption {
            id: "remote".into(),
            label: "Remote · Streamable HTTP".into(),
            template: TransportView::Http { url, headers: Vec::new() },
            inputs,
            oauth,
            note: oauth.then(|| "Sign in through the browser after adding it.".to_string()),
            unsupported: None,
        };
    }
    let command = s.server.command.clone().unwrap_or_default();
    let args: Vec<String> = s.server.args.iter().map(|a| archestra_template(a, &mut used)).collect();
    let mut env = Vec::new();
    for (key, value) in &s.server.env {
        let before = used.len();
        let value = archestra_template(value, &mut used);
        let secret = used[before..].iter().any(|k| s.user_config.get(k).is_some_and(|f| f.sensitive));
        env.push(KeyValue { key: key.clone(), value, secret });
    }
    let inputs = archestra_inputs(&s.user_config, &used);
    let needs_checkout = command.contains("${") || command.starts_with('.') || command.starts_with('/')
        || s.server.args.iter().any(|a| a.contains("${__dirname}"));
    let unsupported = if command.is_empty() || command == "unknown" {
        Some("The catalog has no launch command for this server.".to_string())
    } else if needs_checkout {
        Some("Runs from a local checkout of its repository; follow its README to install it.".to_string())
    } else {
        None
    };
    let note = matches!(command.as_str(), "python" | "python3" | "node" | "go" | "java")
        .then(|| format!("Runs with `{command}`: install the server's package first."));
    InstallOption {
        id: "local".into(),
        label: format!("Local · {command}"),
        template: TransportView::Stdio { command, args, env, cwd: None },
        inputs,
        oauth: false,
        note,
        unsupported,
    }
}

fn archestra_entry(s: ArchestraServer) -> CatalogEntry {
    let option = archestra_option(&s);
    let github = s.github_info.unwrap_or_default();
    CatalogEntry {
        id: format!("archestra:{}", s.name),
        source: Source::Archestra,
        title: non_empty(s.display_name.as_deref()).unwrap_or_else(|| s.name.clone()),
        name: s.name,
        description: s.description.unwrap_or_default(),
        version: None,
        repository: github.url.filter(|u| !u.is_empty()),
        website: None,
        stars: github.stars,
        category: s.category,
        options: vec![option],
    }
}

fn parse_archestra(list: ArchestraList, offset: usize) -> CatalogPage {
    let count = list.servers.len();
    let entries = list.servers.into_iter().map(archestra_entry).collect();
    let next_cursor = (list.has_more && count > 0).then(|| (offset + count).to_string());
    CatalogPage { entries, next_cursor }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn official(server: Value) -> CatalogEntry {
        let list: OfficialList = serde_json::from_value(json!({"servers": [{"server": server}]})).unwrap();
        parse_official(list).entries.remove(0)
    }

    fn archestra(server: Value) -> CatalogEntry {
        let list: ArchestraList =
            serde_json::from_value(json!({"servers": [server], "totalCount": 1, "hasMore": false})).unwrap();
        parse_archestra(list, 0).entries.remove(0)
    }

    fn stdio(t: &TransportView) -> (&str, &[String], &[KeyValue]) {
        match t {
            TransportView::Stdio { command, args, env, .. } => (command, args, env),
            _ => panic!("expected stdio"),
        }
    }

    #[test]
    fn npm_package_becomes_pinned_npx_with_secret_env() {
        let entry = official(json!({
            "name": "io.github.acme/weather", "description": "Weather", "version": "1.2.0",
            "repository": {"url": "https://github.com/acme/weather", "source": "github"},
            "packages": [{
                "registryType": "npm", "identifier": "@acme/weather", "version": "1.2.0",
                "transport": {"type": "stdio"},
                "environmentVariables": [
                    {"name": "WEATHER_PAT", "isRequired": true, "isSecret": true, "description": "Token"},
                    {"name": "WEATHER_UNITS", "default": "metric"}
                ]
            }]
        }));
        assert_eq!(entry.id, "official:io.github.acme/weather");
        assert_eq!(entry.title, "weather");
        assert_eq!(entry.repository.as_deref(), Some("https://github.com/acme/weather"));
        let option = &entry.options[0];
        assert_eq!(option.label, "npm · @acme/weather@1.2.0");
        assert!(option.inputs[0].required && option.inputs[0].secret);

        let missing = render(option, &BTreeMap::new());
        assert_eq!(missing.missing, vec!["WEATHER_PAT".to_string()]);

        let rendered = render(option, &values(&[("WEATHER_PAT", "ghp_1")]));
        assert!(rendered.missing.is_empty());
        let (command, args, env) = stdio(&rendered.transport);
        assert_eq!(command, "npx");
        assert_eq!(args, ["-y", "@acme/weather@1.2.0"]);
        assert_eq!(env[0], KeyValue { key: "WEATHER_PAT".into(), value: "ghp_1".into(), secret: true });
        assert_eq!(env[1], KeyValue { key: "WEATHER_UNITS".into(), value: "metric".into(), secret: false });
    }

    #[test]
    fn pypi_and_oci_packages_render_their_runners() {
        let entry = official(json!({
            "name": "io.github.acme/db", "description": "",
            "packages": [
                {"registryType": "pypi", "identifier": "acme-db", "version": "0.3.1",
                 "packageArguments": [
                     {"type": "named", "name": "--dsn", "valueHint": "dsn", "isRequired": true},
                     {"type": "named", "name": "--schema", "valueHint": "schema"},
                     {"type": "named", "name": "--read-only"}
                 ]},
                {"registryType": "oci", "identifier": "ghcr.io/acme/db", "version": "0.3.1",
                 "environmentVariables": [{"name": "DB_URL", "isRequired": true}]}
            ]
        }));
        let pypi = render(&entry.options[0], &values(&[("dsn", "postgres://x")]));
        let (command, args, _) = stdio(&pypi.transport);
        assert_eq!(command, "uvx");
        assert_eq!(args, ["acme-db==0.3.1", "--dsn", "postgres://x", "--read-only"]);

        let oci = render(&entry.options[1], &values(&[("DB_URL", "postgres://y")]));
        let (command, args, env) = stdio(&oci.transport);
        assert_eq!(command, "docker");
        assert_eq!(args, ["run", "-i", "--rm", "-e", "DB_URL", "ghcr.io/acme/db:0.3.1"]);
        assert_eq!(env[0].value, "postgres://y");
    }

    #[test]
    fn secret_arguments_move_into_env() {
        let entry = official(json!({
            "name": "x/y", "description": "",
            "packages": [{"registryType": "npm", "identifier": "y",
                "packageArguments": [{"type": "named", "name": "--api-key", "valueHint": "api_key",
                                      "isRequired": true, "isSecret": true}]}]
        }));
        let rendered = render(&entry.options[0], &values(&[("api_key", "s3cr3t")]));
        let (_, args, env) = stdio(&rendered.transport);
        assert_eq!(args, ["-y", "y", "--api-key", "${TULMA_API_KEY}"]);
        assert_eq!(env, [KeyValue { key: "TULMA_API_KEY".into(), value: "s3cr3t".into(), secret: true }]);
    }

    #[test]
    fn remotes_map_to_http_and_sse_goes_through_mcp_remote() {
        let entry = official(json!({
            "name": "com.acme/mcp", "title": "Acme", "description": "",
            "remotes": [
                {"type": "streamable-http", "url": "https://{tenant}.acme.com/mcp",
                 "variables": {"tenant": {"isRequired": true}},
                 "headers": [{"name": "Authorization", "value": "Bearer {token}", "isSecret": true,
                              "variables": {"token": {"isRequired": true}}}]},
                {"type": "sse", "url": "https://acme.com/sse"}
            ],
            "packages": [{"registryType": "mcpb", "identifier": "https://x/y.mcpb"}]
        }));
        assert_eq!(entry.title, "Acme");
        assert_eq!(entry.options.len(), 3);
        assert!(entry.options[2].unsupported.is_some(), "unsupported options sort last");

        let http = render(&entry.options[0], &values(&[("tenant", "eu"), ("Authorization.token", "t1")]));
        let TransportView::Http { url, headers } = &http.transport else { panic!() };
        assert_eq!(url, "https://eu.acme.com/mcp");
        assert_eq!(headers[0], KeyValue { key: "Authorization".into(), value: "Bearer t1".into(), secret: true });

        let sse = render(&entry.options[1], &BTreeMap::new());
        let (command, args, _) = stdio(&sse.transport);
        assert_eq!(command, "npx");
        assert_eq!(args, ["-y", "mcp-remote", "https://acme.com/sse"]);
    }

    #[test]
    fn official_pages_dedupe_and_carry_the_cursor() {
        let list: OfficialList = serde_json::from_value(json!({
            "servers": [{"server": {"name": "a/b"}}, {"server": {"name": "a/b"}}, {"server": {"name": "c/d"}}],
            "metadata": {"nextCursor": "c/d:1.0.0", "count": 3}
        }))
        .unwrap();
        let page = parse_official(list);
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.next_cursor.as_deref(), Some("c/d:1.0.0"));
    }

    #[test]
    fn archestra_local_server_maps_user_config() {
        let entry = archestra(json!({
            "name": "modelcontextprotocol__servers__src__filesystem", "display_name": "filesystem",
            "description": "Files", "category": "File Management",
            "github_info": {"url": "https://github.com/modelcontextprotocol/servers", "stars": 70000},
            "server": {"type": "local", "command": "npx",
                       "args": ["-y", "@modelcontextprotocol/server-filesystem", "${user_config.allowed_directories}"],
                       "env": {"API_TOKEN": "${user_config.token}"}},
            "user_config": {
                "allowed_directories": {"type": "directory", "title": "Allowed Directories", "required": true,
                                        "multiple": true, "default": ["${HOME}"]},
                "token": {"type": "string", "title": "Token", "required": false, "sensitive": true},
                "unused": {"type": "string", "title": "Unused", "required": true}
            }
        }));
        assert_eq!(entry.title, "filesystem");
        assert_eq!(entry.stars, Some(70000));
        let option = &entry.options[0];
        assert!(option.unsupported.is_none());
        assert_eq!(option.inputs.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(), ["allowed_directories", "token"]);
        assert_eq!(option.inputs[0].kind, InputKind::Directory);
        assert!(option.inputs[1].secret);

        let rendered = render(option, &values(&[("allowed_directories", "/a\n/b\n")]));
        let (_, args, env) = stdio(&rendered.transport);
        assert_eq!(args, ["-y", "@modelcontextprotocol/server-filesystem", "/a", "/b"]);
        assert!(env.is_empty(), "optional secret left empty is dropped");

        let rendered = render(option, &values(&[("allowed_directories", "/a"), ("token", "t")]));
        let (_, _, env) = stdio(&rendered.transport);
        assert_eq!(env, [KeyValue { key: "API_TOKEN".into(), value: "t".into(), secret: true }]);
    }

    #[test]
    fn archestra_marks_checkouts_unsupported_and_remotes_with_oauth() {
        let checkout = archestra(json!({
            "name": "x", "server": {"type": "local", "command": "python", "args": ["${__dirname}/server.py"]}
        }));
        assert!(checkout.options[0].unsupported.is_some());

        let remote = archestra(json!({
            "name": "linear", "display_name": "Linear",
            "server": {"type": "remote", "url": "https://mcp.linear.app/mcp"},
            "archestra_config": {"oauth": {"required": true}}
        }));
        let option = &remote.options[0];
        assert!(option.oauth);
        assert_eq!(option.template, TransportView::Http { url: "https://mcp.linear.app/mcp".into(), headers: vec![] });

        let sse = archestra(json!({"name": "asana", "server": {"type": "remote", "url": "https://mcp.asana.com/sse"}}));
        assert_eq!(stdio(&sse.options[0].template).0, "npx");
    }

    #[test]
    fn archestra_pages_use_offsets() {
        let list: ArchestraList = serde_json::from_value(json!({
            "servers": [{"name": "a", "server": {"type": "local", "command": "npx"}}],
            "totalCount": 5, "hasMore": true
        }))
        .unwrap();
        assert_eq!(parse_archestra(list, 30).next_cursor.as_deref(), Some("31"));
    }

    #[test]
    fn archestra_defaults_keep_only_fixed_folders() {
        let home = dirs::home_dir().unwrap().display().to_string();
        assert_eq!(archestra_default(&Some(json!("${HOME}"))), Some(home.clone()));
        assert_eq!(archestra_default(&Some(json!("${DESKTOP}"))), Some(format!("{home}/Desktop")));
        assert_eq!(archestra_default(&Some(json!("${workspaceFolder}"))), None);
        assert_eq!(archestra_default(&Some(json!(8080))), Some("8080".into()));
        assert_eq!(archestra_default(&Some(json!(true))), Some("true".into()));
    }

    #[test]
    fn missing_required_values_keep_their_placeholder() {
        let option = InstallOption {
            id: "x".into(),
            label: "x".into(),
            template: TransportView::Http { url: "https://{{host}}/mcp".into(), headers: vec![] },
            inputs: vec![InputField {
                key: "host".into(),
                label: "Host".into(),
                description: None,
                required: true,
                secret: false,
                default: None,
                kind: InputKind::String,
                choices: vec![],
                multiple: false,
            }],
            oauth: false,
            note: None,
            unsupported: None,
        };
        let rendered = render(&option, &BTreeMap::new());
        assert_eq!(rendered.missing, vec!["host".to_string()]);
        assert_eq!(rendered.transport, TransportView::Http { url: "https://{{host}}/mcp".into(), headers: vec![] });
    }
}
