//! Built-in dormant servers, defined in code so the binary needs no shipped
//! config file.

use super::{HttpAuth, ServerSpec, Transport};

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
                // No client_id ⇒ dynamic client registration.
                auth: HttpAuth::OAuth {
                    scopes: Vec::new(),
                    client_id: None,
                    client_secret: None,
                },
            },
        },
    ]
}
