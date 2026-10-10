use std::collections::{BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};

use rmcp::model::{Prompt, Resource, Tool};

use crate::model::ServerId;

pub const MAX_NAME: usize = 64;
const SEP: &str = "__";

pub struct CatalogInput<'a> {
    pub server_id: &'a str,
    pub slug: &'a str,
    pub tools: &'a [Tool],
    pub prompts: &'a [Prompt],
    pub resources: &'a [Resource],
    pub disabled_tools: &'a BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub server_id: ServerId,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fingerprints {
    pub tools: u64,
    pub prompts: u64,
    pub resources: u64,
}

#[derive(Debug, Default)]
pub struct Catalog {
    pub tools: Vec<Tool>,
    pub tool_route: HashMap<String, Route>,
    pub prompts: Vec<Prompt>,
    pub prompt_route: HashMap<String, Route>,
    pub resources: Vec<Resource>,
    pub resource_route: HashMap<String, ServerId>,
    pub fingerprints: Fingerprints,
    pub warnings: Vec<String>,
}

pub fn exposed_name(slug: &str, name: &str) -> String {
    let tool: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    let slug_budget = MAX_NAME.saturating_sub(SEP.len() + tool.len()).max(1);
    let slug = &slug[..slug.len().min(slug_budget)];
    let mut full = format!("{slug}{SEP}{tool}");
    full.truncate(MAX_NAME);
    full
}

fn unique(base: String, taken: &HashMap<String, Route>) -> String {
    if !taken.contains_key(&base) {
        return base;
    }
    (2..)
        .map(|n| {
            let suffix = format!("_{n}");
            let mut stem = base.clone();
            stem.truncate(MAX_NAME - suffix.len());
            stem + &suffix
        })
        .find(|candidate| !taken.contains_key(candidate))
        .expect("unbounded range always yields a free name")
}

fn fingerprint<T: serde::Serialize>(items: &[T]) -> u64 {
    let mut hasher = DefaultHasher::new();
    serde_json::to_string(items).unwrap_or_default().hash(&mut hasher);
    hasher.finish()
}

impl Catalog {
    pub fn build(inputs: &[CatalogInput<'_>]) -> Catalog {
        let mut catalog = Catalog::default();
        for input in inputs {
            for tool in input.tools.iter().filter(|t| !input.disabled_tools.contains(t.name.as_ref())) {
                let name = unique(exposed_name(input.slug, &tool.name), &catalog.tool_route);
                catalog.tool_route.insert(
                    name.clone(),
                    Route { server_id: input.server_id.to_string(), name: tool.name.to_string() },
                );
                let mut exposed = tool.clone();
                exposed.name = name.into();
                catalog.tools.push(exposed);
            }
            for prompt in input.prompts {
                let name = unique(exposed_name(input.slug, &prompt.name), &catalog.prompt_route);
                catalog.prompt_route.insert(
                    name.clone(),
                    Route { server_id: input.server_id.to_string(), name: prompt.name.clone() },
                );
                let mut exposed = prompt.clone();
                exposed.name = name;
                catalog.prompts.push(exposed);
            }
            for resource in input.resources {
                if let Some(owner) = catalog.resource_route.get(&resource.uri) {
                    catalog.warnings.push(format!(
                        "resource {} from {} shadowed by server {owner}",
                        resource.uri, input.server_id
                    ));
                    continue;
                }
                catalog.resource_route.insert(resource.uri.clone(), input.server_id.to_string());
                catalog.resources.push(resource.clone());
            }
        }
        catalog.fingerprints = Fingerprints {
            tools: fingerprint(&catalog.tools),
            prompts: fingerprint(&catalog.prompts),
            resources: fingerprint(&catalog.resources),
        };
        catalog
    }

    /// Reverse lookup used by the UI to show which exposed name a tool got.
    pub fn exposed_for(&self, server_id: &str, tool: &str) -> Option<&str> {
        self.tool_route
            .iter()
            .find(|(_, r)| r.server_id == server_id && r.name == tool)
            .map(|(exposed, _)| exposed.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn tool(name: &str) -> Tool {
        Tool::new(name.to_string(), format!("{name} desc"), Arc::new(serde_json::Map::new()))
    }

    #[test]
    fn exposed_name_joins_slug_and_tool() {
        assert_eq!(exposed_name("echo", "ping"), "echo__ping");
    }

    #[test]
    fn exposed_name_sanitizes_tool_characters() {
        assert_eq!(exposed_name("yt", "get.issue/comments"), "yt__get_issue_comments");
    }

    #[test]
    fn exposed_name_truncates_slug_to_fit_limit() {
        let tool_name = "t".repeat(50);
        let name = exposed_name("a_really_long_server_slug", &tool_name);
        assert_eq!(name.len(), MAX_NAME);
        assert!(name.ends_with(&format!("__{tool_name}")));
        assert!(name.starts_with("a_really_lo"));
    }

    #[test]
    fn exposed_name_truncates_tool_when_it_alone_is_too_long() {
        let name = exposed_name("s", &"x".repeat(100));
        assert_eq!(name.len(), MAX_NAME);
        assert!(name.starts_with("s__x"));
    }

    #[test]
    fn build_skips_disabled_tools_and_routes_the_rest() {
        let tools = vec![tool("ping"), tool("echo"), tool("secret")];
        let disabled = BTreeSet::from(["secret".to_string()]);
        let catalog = Catalog::build(&[CatalogInput {
            server_id: "s1",
            slug: "echo",
            tools: &tools,
            prompts: &[],
            resources: &[],
            disabled_tools: &disabled,
        }]);
        let names: Vec<_> = catalog.tools.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, vec!["echo__ping", "echo__echo"]);
        assert_eq!(
            catalog.tool_route["echo__ping"],
            Route { server_id: "s1".into(), name: "ping".into() }
        );
        assert_eq!(catalog.tools[0].description.as_deref(), Some("ping desc"));
        assert_eq!(catalog.exposed_for("s1", "echo"), Some("echo__echo"));
        assert_eq!(catalog.exposed_for("s1", "secret"), None);
    }

    #[test]
    fn build_resolves_exposed_name_collisions_deterministically() {
        let a = vec![tool("get.x"), tool("get_x")];
        let none = BTreeSet::new();
        let catalog = Catalog::build(&[CatalogInput {
            server_id: "s1",
            slug: "srv",
            tools: &a,
            prompts: &[],
            resources: &[],
            disabled_tools: &none,
        }]);
        let names: Vec<_> = catalog.tools.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, vec!["srv__get_x", "srv__get_x_2"]);
        assert_eq!(catalog.tool_route["srv__get_x_2"].name, "get_x");
    }

    #[test]
    fn build_namespaces_prompts_and_routes_resources_by_uri() {
        let none = BTreeSet::new();
        let p1 = vec![Prompt::new("review", Some("Review code"), None)];
        let r1 = vec![Resource::new("file:///a", "a")];
        let r2 = vec![Resource::new("file:///a", "a-dup"), Resource::new("file:///b", "b")];
        let catalog = Catalog::build(&[
            CatalogInput { server_id: "s1", slug: "one", tools: &[], prompts: &p1, resources: &r1, disabled_tools: &none },
            CatalogInput { server_id: "s2", slug: "two", tools: &[], prompts: &[], resources: &r2, disabled_tools: &none },
        ]);
        assert_eq!(catalog.prompts[0].name, "one__review");
        assert_eq!(catalog.prompt_route["one__review"].name, "review");
        assert_eq!(catalog.resources.len(), 2);
        assert_eq!(catalog.resource_route["file:///a"], "s1");
        assert_eq!(catalog.resource_route["file:///b"], "s2");
        assert_eq!(catalog.warnings.len(), 1);
    }

    #[test]
    fn fingerprints_change_only_with_content() {
        let none = BTreeSet::new();
        let t1 = vec![tool("ping")];
        let input = |tools| CatalogInput { server_id: "s1", slug: "e", tools, prompts: &[], resources: &[], disabled_tools: &none };
        let a = Catalog::build(&[input(&t1)]);
        let b = Catalog::build(&[input(&t1)]);
        let t2 = vec![tool("ping"), tool("pong")];
        let c = Catalog::build(&[input(&t2)]);
        assert_eq!(a.fingerprints, b.fingerprints);
        assert_ne!(a.fingerprints.tools, c.fingerprints.tools);
        assert_eq!(a.fingerprints.prompts, c.fingerprints.prompts);
    }
}
