//! Turning `--auth` into a credential, with the one prompt setup is allowed to ask.
//!
//! The ladder itself lives in `credentials`; what belongs here is the decision of
//! what to do when it finds nothing. Interactively that is an offer to run
//! `gcloud auth application-default login`, because for most users the missing
//! step is exactly that command. Under `--non-interactive` it is an error that
//! names the command instead of running it: an unattended run that opens a browser
//! and blocks is worse than one that exits.

use std::{
    collections::HashMap,
    io::{IsTerminal as _, Write as _},
    path::PathBuf,
    process::{Command, Stdio},
};

use ccusage_cli::SyncAuthMode;
use ccusage_objectstore::{ObjectStoreError, Result};

use crate::credentials::{Credentials, Resolver, gcloud_program};

/// Asks the user a yes/no question. Injected so the tests never touch a terminal.
pub(crate) trait Prompt {
    fn confirm(&mut self, question: &str) -> bool;
}

/// Only prompts on a real terminal: a piped stdin would otherwise read EOF and
/// silently decline, or worse, consume data meant for something else.
pub(crate) struct TerminalPrompt;

impl Prompt for TerminalPrompt {
    fn confirm(&mut self, question: &str) -> bool {
        let mut stdout = std::io::stdout();
        if !std::io::stdin().is_terminal() || !stdout.is_terminal() {
            return false;
        }
        let _ = write!(stdout, "{question} [y/N] ");
        let _ = stdout.flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err() {
            return false;
        }
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}

pub(crate) fn resolve(
    mode: SyncAuthMode,
    non_interactive: bool,
    prompt: &mut dyn Prompt,
) -> Result<Credentials> {
    let env: HashMap<String, String> = std::env::vars().collect();
    resolve_with(
        mode,
        non_interactive,
        prompt,
        &mut || Resolver::new().with_mode(mode).resolve(),
        &mut |gcloud| adc_login(&gcloud),
        gcloud_program(&env),
    )
}

/// `login` receives the resolved gcloud path, so a caller that has none cannot
/// reach the prompt at all.
pub(crate) fn resolve_with(
    mode: SyncAuthMode,
    non_interactive: bool,
    prompt: &mut dyn Prompt,
    resolve_ladder: &mut dyn FnMut() -> Result<Credentials>,
    login: &mut dyn FnMut(PathBuf) -> std::result::Result<(), String>,
    gcloud: Option<PathBuf>,
) -> Result<Credentials> {
    let first = match resolve_ladder() {
        Ok(credentials) => return Ok(credentials),
        Err(error) => error,
    };
    // HMAC is a pair of environment variables; no login flow can produce it.
    let loginable = matches!(mode, SyncAuthMode::Auto | SyncAuthMode::Adc);
    let Some(gcloud) = gcloud.filter(|_| loginable && !non_interactive) else {
        return Err(first);
    };
    if !prompt
        .confirm("No Google credentials found. Run `gcloud auth application-default login` now?")
    {
        return Err(first);
    }
    // gcloud's own output goes straight to the terminal — the user needs the
    // verification URL — and is never captured, so it cannot reach a log.
    login(gcloud).map_err(|detail| ObjectStoreError::Unauthenticated {
        source: "gcloud auth application-default login".to_string(),
        detail,
    })?;
    resolve_ladder()
}

fn adc_login(gcloud: &PathBuf) -> std::result::Result<(), String> {
    let status = Command::new(gcloud)
        .args(["auth", "application-default", "login"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() {
        return Ok(());
    }
    Err(format!(
        "exited with {}",
        status
            .code()
            .map_or_else(|| "a signal".to_string(), |code| code.to_string())
    ))
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap, rc::Rc};

    use super::*;

    #[test]
    fn a_working_ladder_never_prompts() {
        let prompt = &mut RecordingPrompt::answering(true);
        let logins = Logins::default();

        let credentials = resolve_with(
            SyncAuthMode::Auto,
            false,
            prompt,
            &mut || working_ladder(),
            &mut logins.succeeding(),
            Some(PathBuf::from("/usr/bin/gcloud")),
        )
        .expect("the env token resolves");

        assert_eq!(
            credentials.source().to_string(),
            "CCUSAGE_SYNC_ACCESS_TOKEN"
        );
        assert!(prompt.questions.is_empty());
        assert_eq!(logins.count(), 0);
    }

    #[test]
    fn non_interactive_never_prompts_and_never_logs_in() {
        let prompt = &mut RecordingPrompt::answering(true);
        let logins = Logins::default();

        let error = resolve_with(
            SyncAuthMode::Auto,
            true,
            prompt,
            &mut || empty_ladder(),
            &mut logins.succeeding(),
            Some(PathBuf::from("/usr/bin/gcloud")),
        )
        .expect_err("nothing to resolve");

        assert!(prompt.questions.is_empty());
        assert_eq!(logins.count(), 0);
        assert!(
            error
                .to_string()
                .contains("gcloud auth application-default login"),
            "the error should name the command it declined to run: {error}"
        );
    }

    #[test]
    fn declining_the_prompt_keeps_the_original_failure() {
        let prompt = &mut RecordingPrompt::answering(false);
        let logins = Logins::default();

        let error = resolve_with(
            SyncAuthMode::Auto,
            false,
            prompt,
            &mut || empty_ladder(),
            &mut logins.succeeding(),
            Some(PathBuf::from("/usr/bin/gcloud")),
        )
        .expect_err("nothing to resolve");

        assert_eq!(prompt.questions.len(), 1);
        assert_eq!(logins.count(), 0);
        assert!(error.to_string().contains("no Google credentials found"));
    }

    #[test]
    fn accepting_the_prompt_logs_in_once_and_retries() {
        let prompt = &mut RecordingPrompt::answering(true);
        let logins = Logins::default();
        let mut attempts = 0;

        let credentials = resolve_with(
            SyncAuthMode::Auto,
            false,
            prompt,
            &mut || {
                attempts += 1;
                if attempts == 1 {
                    empty_ladder()
                } else {
                    working_ladder()
                }
            },
            &mut logins.succeeding(),
            Some(PathBuf::from("/usr/bin/gcloud")),
        )
        .expect("the retry resolves");

        assert_eq!(
            credentials.source().to_string(),
            "CCUSAGE_SYNC_ACCESS_TOKEN"
        );
        assert_eq!(logins.count(), 1);
        assert_eq!(attempts, 2);
    }

    #[test]
    fn a_failed_login_reports_the_command_not_its_output() {
        let prompt = &mut RecordingPrompt::answering(true);

        let error = resolve_with(
            SyncAuthMode::Auto,
            false,
            prompt,
            &mut || empty_ladder(),
            &mut |_| Err("exited with 1".to_string()),
            Some(PathBuf::from("/usr/bin/gcloud")),
        )
        .expect_err("the login failed");

        let rendered = error.to_string();
        assert!(rendered.contains("gcloud auth application-default login"));
        assert!(rendered.contains("exited with 1"));
    }

    #[test]
    fn hmac_mode_is_not_offered_a_login_it_cannot_use() {
        let prompt = &mut RecordingPrompt::answering(true);
        let logins = Logins::default();

        let error = resolve_with(
            SyncAuthMode::Hmac,
            false,
            prompt,
            &mut || empty_ladder(),
            &mut logins.succeeding(),
            Some(PathBuf::from("/usr/bin/gcloud")),
        )
        .expect_err("no HMAC key is set");

        assert!(prompt.questions.is_empty());
        assert_eq!(logins.count(), 0);
        assert!(error.to_string().contains("CCUSAGE_SYNC_HMAC_ACCESS_ID"));
    }

    #[test]
    fn a_machine_without_gcloud_is_not_asked_to_run_it() {
        let prompt = &mut RecordingPrompt::answering(true);

        resolve_with(
            SyncAuthMode::Auto,
            false,
            prompt,
            &mut || empty_ladder(),
            &mut |_| panic!("no gcloud to run"),
            None,
        )
        .expect_err("nothing to resolve");

        assert!(prompt.questions.is_empty());
    }

    fn ladder(pairs: &[(&str, &str)], mode: SyncAuthMode) -> Result<Credentials> {
        Resolver::new()
            .with_env(
                pairs
                    .iter()
                    .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                    .collect::<HashMap<_, _>>(),
            )
            .with_home(None)
            .with_gcloud(std::sync::Arc::new(|| Err("not installed".to_string())))
            .with_mode(mode)
            .resolve()
    }

    fn working_ladder() -> Result<Credentials> {
        ladder(
            &[("CCUSAGE_SYNC_ACCESS_TOKEN", "ya29.token")],
            SyncAuthMode::Auto,
        )
    }

    fn empty_ladder() -> Result<Credentials> {
        ladder(&[], SyncAuthMode::Auto)
    }

    #[derive(Default)]
    struct Logins(Rc<RefCell<usize>>);

    impl Logins {
        fn succeeding(&self) -> impl FnMut(PathBuf) -> std::result::Result<(), String> + use<'_> {
            let count = Rc::clone(&self.0);
            move |_| {
                *count.borrow_mut() += 1;
                Ok(())
            }
        }

        fn count(&self) -> usize {
            *self.0.borrow()
        }
    }

    struct RecordingPrompt {
        answer: bool,
        questions: Vec<String>,
    }

    impl RecordingPrompt {
        fn answering(answer: bool) -> Self {
            Self {
                answer,
                questions: Vec::new(),
            }
        }
    }

    impl Prompt for RecordingPrompt {
        fn confirm(&mut self, question: &str) -> bool {
            self.questions.push(question.to_string());
            self.answer
        }
    }
}
