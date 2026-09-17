//! Choosing the project the bucket is billed to.
//!
//! The choice is a pure function of what the user asked for, what the
//! environment says, and what the credential can see, so the whole decision
//! table is testable without a network: `--project` wins, then the environment
//! variables gcloud itself honours, then the credential's own project list —
//! one is taken silently, several are offered, and none is the interesting case,
//! because the fix is on Google's side and the user needs the exact steps.

use std::{
    collections::HashMap,
    io::{IsTerminal as _, Write as _},
};

use ccusage_objectstore::{ObjectStoreError, Result};

use crate::gcs::projects::Project;

/// Presents a choice between projects. Injected so tests never read a terminal.
pub(crate) trait Picker {
    fn choose(&mut self, projects: &[Project]) -> Option<usize>;
}

/// Numbers the projects and reads one number. Off a terminal it chooses nothing,
/// which turns into the error that names every candidate.
pub(crate) struct TerminalPicker;

impl Picker for TerminalPicker {
    fn choose(&mut self, projects: &[Project]) -> Option<usize> {
        let mut stdout = std::io::stdout();
        if !std::io::stdin().is_terminal() || !stdout.is_terminal() {
            return None;
        }
        let _ = writeln!(stdout, "Which Google Cloud project should hold the bucket?");
        for (index, project) in projects.iter().enumerate() {
            let _ = writeln!(stdout, "  {}) {} ({})", index + 1, project.id, project.name);
        }
        let _ = write!(stdout, "Project [1-{}]: ", projects.len());
        let _ = stdout.flush();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok()?;
        answer
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|choice| (1..=projects.len()).contains(choice))
            .map(|choice| choice - 1)
    }
}

/// How the project was arrived at, so setup can say whether it picked for you.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectOrigin {
    Flag,
    Environment,
    OnlyProject,
    Chosen,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectedProject {
    pub(crate) id: String,
    pub(crate) origin: ProjectOrigin,
}

/// The environment variables gcloud and the client libraries already agree on.
const PROJECT_ENV_VARS: [&str; 3] = [
    "CLOUDSDK_CORE_PROJECT",
    "GOOGLE_CLOUD_PROJECT",
    "GCLOUD_PROJECT",
];

pub(crate) fn select(
    flag: Option<&str>,
    env: &HashMap<String, String>,
    non_interactive: bool,
    list: &mut dyn FnMut() -> Result<Vec<Project>>,
    picker: &mut dyn Picker,
) -> Result<SelectedProject> {
    if let Some(id) = flag.map(str::trim).filter(|id| !id.is_empty()) {
        return Ok(SelectedProject {
            id: id.to_string(),
            origin: ProjectOrigin::Flag,
        });
    }
    if let Some(id) = PROJECT_ENV_VARS
        .iter()
        .find_map(|name| env.get(*name))
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
    {
        return Ok(SelectedProject {
            id: id.to_string(),
            origin: ProjectOrigin::Environment,
        });
    }
    let projects = list()?;
    match projects.as_slice() {
        [] => Err(no_project_error()),
        [only] => Ok(SelectedProject {
            id: only.id.clone(),
            origin: ProjectOrigin::OnlyProject,
        }),
        several if non_interactive => Err(ambiguous_project_error(several)),
        several => picker
            .choose(several)
            .and_then(|index| several.get(index))
            .map(|project| SelectedProject {
                id: project.id.clone(),
                origin: ProjectOrigin::Chosen,
            })
            .ok_or_else(|| ambiguous_project_error(several)),
    }
}

/// A credential with no project is the one failure setup cannot work around, so
/// the message is the procedure rather than a description of the problem.
fn no_project_error() -> ObjectStoreError {
    ObjectStoreError::Other {
        detail: "this account has no active Google Cloud project. Create one with \
                 `gcloud projects create <project-id> --name=ccusage`, enable billing on it at \
                 https://console.cloud.google.com/billing (a bucket cannot be created without it), \
                 then re-run `ccusage sync setup --project <project-id>`"
            .to_string(),
    }
}

fn ambiguous_project_error(projects: &[Project]) -> ObjectStoreError {
    let ids = projects
        .iter()
        .map(|project| project.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    ObjectStoreError::Other {
        detail: format!(
            "several Google Cloud projects are available; pass --project <id>. Found: {ids}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_beats_everything_including_the_environment() {
        let selected = select(
            Some("flag-project"),
            &env(&[("GOOGLE_CLOUD_PROJECT", "env-project")]),
            false,
            &mut || panic!("the flag answers the question; do not call the API"),
            &mut RefusingPicker,
        )
        .expect("select");

        assert_eq!(selected.id, "flag-project");
        assert_eq!(selected.origin, ProjectOrigin::Flag);
    }

    #[test]
    fn the_gcloud_environment_is_honoured_before_listing() {
        let selected = select(
            None,
            &env(&[("CLOUDSDK_CORE_PROJECT", "env-project")]),
            false,
            &mut || panic!("the environment answers the question; do not call the API"),
            &mut RefusingPicker,
        )
        .expect("select");

        assert_eq!(selected.id, "env-project");
        assert_eq!(selected.origin, ProjectOrigin::Environment);
    }

    #[test]
    fn a_single_project_is_taken_without_asking() {
        let selected = select(
            None,
            &env(&[]),
            false,
            &mut || Ok(vec![project("only-1")]),
            &mut RefusingPicker,
        )
        .expect("select");

        assert_eq!(selected.id, "only-1");
        assert_eq!(selected.origin, ProjectOrigin::OnlyProject);
    }

    #[test]
    fn several_projects_are_offered_to_the_user() {
        let mut picker = IndexPicker { index: Some(1) };

        let selected = select(
            None,
            &env(&[]),
            false,
            &mut || Ok(vec![project("alpha-1"), project("beta-2")]),
            &mut picker,
        )
        .expect("select");

        assert_eq!(selected.id, "beta-2");
        assert_eq!(selected.origin, ProjectOrigin::Chosen);
    }

    #[test]
    fn several_projects_without_a_terminal_name_the_candidates() {
        let error = select(
            None,
            &env(&[]),
            true,
            &mut || Ok(vec![project("alpha-1"), project("beta-2")]),
            &mut RefusingPicker,
        )
        .expect_err("ambiguous");

        let rendered = error.to_string();
        assert!(rendered.contains("--project"), "{rendered}");
        assert!(rendered.contains("alpha-1"), "{rendered}");
        assert!(rendered.contains("beta-2"), "{rendered}");
    }

    #[test]
    fn an_abandoned_picker_does_not_pick_for_the_user() {
        let mut picker = IndexPicker { index: None };

        let error = select(
            None,
            &env(&[]),
            false,
            &mut || Ok(vec![project("alpha-1"), project("beta-2")]),
            &mut picker,
        )
        .expect_err("nothing chosen");

        assert!(error.to_string().contains("--project"));
    }

    #[test]
    fn no_projects_yields_the_steps_that_fix_it() {
        let error = select(
            None,
            &env(&[]),
            false,
            &mut || Ok(Vec::new()),
            &mut RefusingPicker,
        )
        .expect_err("no projects");

        let rendered = error.to_string();
        assert!(rendered.contains("gcloud projects create"), "{rendered}");
        assert!(rendered.contains("billing"), "{rendered}");
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn project(id: &str) -> Project {
        Project {
            id: id.to_string(),
            name: id.to_string(),
        }
    }

    struct RefusingPicker;

    impl Picker for RefusingPicker {
        fn choose(&mut self, _projects: &[Project]) -> Option<usize> {
            panic!("the user should not have been asked");
        }
    }

    struct IndexPicker {
        index: Option<usize>,
    }

    impl Picker for IndexPicker {
        fn choose(&mut self, _projects: &[Project]) -> Option<usize> {
            self.index
        }
    }
}
