//! The dormant tool catalog — servers that ship *with* the agent but stay
//! disconnected until the user loads one (`/mcp load <name>`). Defined in code
//! rather than a config file so the binary is self-contained: there's no file
//! to embed or ship alongside an installed `nudge`. User-owned servers
//! live in the project-local `.mcp.json` instead and connect at startup.
//!
//! Each entry yields the same `ServerSpec` the `.mcp.json` parser produces, so
//! everything downstream (connect, route, schema) is origin-agnostic.

use super::{HttpAuth, ServerSpec, Transport};

/// The built-in dormant servers, in stable name order.
pub(super) fn dormant_catalog() -> Vec<ServerSpec> {
    vec![
        ServerSpec {
            name: "everything".into(),
            description: Some(
                "MCP reference/test server: echo, add, long-running progress, \
                 sampling, and resource demos. Useful for exercising MCP itself."
                    .into(),
            ),
            transport: Transport::Stdio {
                command: "npx".into(),
                args: vec![
                    "-y".into(),
                    "@modelcontextprotocol/server-everything".into(),
                ],
                env: Vec::new(),
            },
        },
        ServerSpec {
            name: "gitlab".into(),
            description: Some(
                "GitLab API: issues, merge requests, pipelines, repositories. \
                 Load when the task involves a GitLab project (auth via OAuth on \
                 first load)."
                    .into(),
            ),
            transport: Transport::Http {
                url: "https://gitlab.com/api/v4/mcp".into(),
                // No client_id ⇒ dynamic client registration (no GCP-style setup).
                auth: HttpAuth::OAuth {
                    scopes: Vec::new(),
                    client_id: None,
                    client_secret: None,
                },
            },
        },
    ]
}
