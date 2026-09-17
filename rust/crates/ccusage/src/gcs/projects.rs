//! Listing the GCP projects a credential can create a bucket in.
//!
//! Cloud Resource Manager rather than the Storage API, because the question
//! setup asks — "which project should the bucket be billed to" — has no answer
//! in the Storage API. Only active projects are returned: a project pending
//! deletion still lists, and creating a bucket in one fails at the far end of a
//! setup flow the user already answered questions for.

use ccusage_objectstore::{ObjectStoreError, Result};
use serde_json::Value;

use super::{JsonApi, RetryPolicy, encode, status_error};

const DEFAULT_ENDPOINT: &str = "https://cloudresourcemanager.googleapis.com";
/// A picker is unusable well before this; the cap exists so a paginating server
/// cannot hold setup open indefinitely.
const MAX_PAGES: usize = 10;
const PAGE_SIZE: u32 = 200;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Project {
    pub(crate) id: String,
    /// The display name, which is what a user recognizes; IDs are often suffixed
    /// with digits Google added to make them unique.
    pub(crate) name: String,
}

pub(crate) struct ProjectCatalog {
    api: JsonApi,
}

impl ProjectCatalog {
    pub(crate) fn new(authorizer: Box<dyn super::Authorizer>) -> Self {
        Self::with_endpoint(DEFAULT_ENDPOINT, authorizer, RetryPolicy::default())
    }

    pub(crate) fn with_endpoint(
        endpoint: &str,
        authorizer: Box<dyn super::Authorizer>,
        retry: RetryPolicy,
    ) -> Self {
        Self {
            api: JsonApi::new(endpoint, authorizer, retry),
        }
    }

    pub(crate) fn list(&self) -> Result<Vec<Project>> {
        let mut projects = Vec::new();
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = format!(
                "{}/v1/projects?pageSize={PAGE_SIZE}&filter={}",
                self.api.endpoint(),
                encode("lifecycleState:ACTIVE"),
            );
            if let Some(token) = &page_token {
                url.push_str(&format!("&pageToken={}", encode(token)));
            }
            let body = self.api.with_retry(|| {
                let response = self.api.send_without_body(self.api.agent.get(&url))?;
                if let Some(error) = status_error(&response, "projects") {
                    return Err(error);
                }
                Ok(response.body)
            })?;
            let page: Value =
                serde_json::from_slice(&body).map_err(|error| ObjectStoreError::Other {
                    detail: format!("unreadable project list: {error}"),
                })?;
            projects.extend(parse_projects(&page));
            page_token = page
                .get("nextPageToken")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
                .map(str::to_string);
            if page_token.is_none() {
                break;
            }
        }
        projects.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(projects)
    }
}

fn parse_projects(page: &Value) -> Vec<Project> {
    page.get("projects")
        .and_then(Value::as_array)
        .map(|projects| {
            projects
                .iter()
                .filter_map(|project| {
                    let id = project.get("projectId").and_then(Value::as_str)?;
                    let name = project
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_string();
                    Some(Project {
                        id: id.to_string(),
                        name,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ccusage_test_support::http_server::{ScriptedServer, json_response as json};

    use super::{super::BearerToken, *};

    fn catalog_for(server: &ScriptedServer) -> ProjectCatalog {
        ProjectCatalog::with_endpoint(
            server.endpoint(),
            Box::new(BearerToken::new("test-token")),
            RetryPolicy {
                max_attempts: 1,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            },
        )
    }

    #[test]
    fn lists_only_active_projects_and_follows_page_tokens() {
        let mut fake = ScriptedServer::serving(vec![
            json(
                200,
                r#"{"projects":[{"projectId":"zeta-1","name":"Zeta"}],"nextPageToken":"page-2"}"#,
            ),
            json(
                200,
                r#"{"projects":[{"projectId":"alpha-1","name":"Alpha"}]}"#,
            ),
        ]);

        let projects = catalog_for(&fake).list().expect("list");

        assert_eq!(
            projects,
            vec![
                Project {
                    id: "alpha-1".to_string(),
                    name: "Alpha".to_string(),
                },
                Project {
                    id: "zeta-1".to_string(),
                    name: "Zeta".to_string(),
                },
            ]
        );
        let requests = fake.requests();
        assert!(
            requests[0].contains("filter=lifecycleState%3AACTIVE"),
            "{}",
            requests[0]
        );
        assert!(requests[1].contains("pageToken=page-2"), "{}", requests[1]);
    }

    #[test]
    fn a_credential_without_the_api_enabled_is_an_error_not_an_empty_list() {
        let fake = ScriptedServer::serving(vec![json(
            403,
            r#"{"error":{"message":"Cloud Resource Manager API has not been used"}}"#,
        )]);

        let error = catalog_for(&fake).list().expect_err("403");

        assert!(matches!(
            error,
            ObjectStoreError::Forbidden { .. } | ObjectStoreError::Unauthenticated { .. }
        ));
    }

    #[test]
    fn no_projects_reads_as_an_empty_list() {
        let fake = ScriptedServer::serving(vec![json(200, r#"{}"#)]);

        assert_eq!(catalog_for(&fake).list().expect("list"), Vec::new());
    }
}
