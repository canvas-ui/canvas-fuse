//! Resolve user-facing context selectors once, then keep every refresh, write
//! and socket subscription bound to the server's canonical context ID.
use crate::api::{ApiClient, ContextInfo};
use anyhow::{Context as _, Result};
use std::collections::HashSet;

pub(crate) struct ContextSelection {
    pub filter: Option<HashSet<String>>,
    pub root: Option<String>,
    pub workspace_id: Option<String>,
}

fn resolve<'a>(contexts: &[&'a ContextInfo], selector: &str) -> Result<&'a ContextInfo> {
    let folded = selector.to_lowercase();
    // IDs are case-insensitive on canvas-server. Prefer an ID over a display
    // name, and never choose arbitrarily between different matching contexts.
    let mut matches: Vec<_> = contexts
        .iter()
        .copied()
        .filter(|context| context.id.to_lowercase() == folded)
        .collect();
    if matches.is_empty() {
        matches = contexts
            .iter()
            .copied()
            .filter(|context| {
                context
                    .raw
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|name| name.to_lowercase() == folded)
            })
            .collect();
    }
    match matches.as_slice() {
        [context] => Ok(context),
        [] => anyhow::bail!("context `{selector}` was not found in the selected workspace or accessible contexts; use `canvas-fuse contexts --json` to inspect context IDs and workspace IDs"),
        _ => anyhow::bail!("context `{selector}` is ambiguous; select its workspace with --workspace and use an unambiguous context ID"),
    }
}

impl ContextSelection {
    pub fn resolve(
        api: &ApiClient,
        selectors: Option<&[String]>,
        root: Option<&str>,
        workspace_id: Option<String>,
    ) -> Result<Self> {
        if selectors.is_none() && root.is_none() {
            return Ok(Self {
                filter: None,
                root: None,
                workspace_id,
            });
        }
        let available = api.list_contexts().context("resolving selected contexts")?;
        let scoped: Vec<_> = available
            .iter()
            .filter(|context| {
                workspace_id
                    .as_ref()
                    .is_none_or(|id| context.workspace_id.as_ref() == Some(id))
            })
            .collect();
        let selected = selectors
            .unwrap_or_default()
            .iter()
            .map(|selector| resolve(&scoped, selector))
            .collect::<Result<Vec<_>>>()?;
        let root = root
            .map(|selector| resolve(&scoped, selector))
            .transpose()?;
        let mut filter: HashSet<_> = selected.iter().map(|context| context.id.clone()).collect();
        if let Some(context) = root {
            anyhow::ensure!(
                filter.is_empty() || (filter.len() == 1 && filter.contains(&context.id)),
                "a context-rooted mount must select only its root context"
            );
            filter.insert(context.id.clone());
        }
        let workspaces: HashSet<_> = selected
            .iter()
            .copied()
            .chain(root)
            .filter_map(|context| context.workspace_id.as_ref())
            .collect();
        anyhow::ensure!(workspaces.len() <= 1, "selected contexts belong to different workspaces; use a separate mount for each workspace");
        let workspace_id = workspace_id.or_else(|| workspaces.into_iter().next().cloned());
        Ok(Self {
            filter: Some(filter),
            root: root.map(|context| context.id.clone()),
            workspace_id,
        })
    }
}
