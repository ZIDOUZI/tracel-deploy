use std::{path::PathBuf, process::Command};

use anyhow::{Context as _, Result};

use crate::{
    context::DeployContext,
    rotate::{RotateOptions, RotateOutcome, RotateStrategy, rotate},
};

pub type DeployHook = Box<dyn FnMut(&DeployContext) -> Result<()>>;

/// A composable deployment plan.
///
/// Hooks run around the steps:
///
/// ```text
/// pre hooks -> steps -> post hooks
/// ```
///
/// If a pre hook or step fails, post hooks are not run. Use a
/// [`DeployStep::Run`] with `allow_failure = true` for best-effort cleanup.
pub struct DeployPlan {
    pub pre: Vec<DeployHook>,
    pub steps: Vec<DeployStep>,
    pub post: Vec<DeployHook>,
}

impl DeployPlan {
    pub fn new() -> Self {
        Self {
            pre: Vec::new(),
            steps: Vec::new(),
            post: Vec::new(),
        }
    }

    pub fn run(self, context: &DeployContext) -> Result<DeployReport> {
        let mut report = DeployReport {
            installed: context.install_path.clone(),
            dry_run: context.dry_run,
            steps: Vec::new(),
        };
        if context.dry_run {
            report.steps.push(DeployStepReport {
                name: "dry-run".to_owned(),
                detail: Some(format!("would run {} deployment step(s)", self.steps.len())),
            });
            return Ok(report);
        }

        for mut hook in self.pre {
            hook(context)?;
        }
        for step in self.steps {
            report.steps.push(step.run(context)?);
        }
        for mut hook in self.post {
            hook(context)?;
        }
        Ok(report)
    }
}

impl Default for DeployPlan {
    fn default() -> Self {
        Self::new()
    }
}

/// Builder returned by [`deploy`].
///
/// By default it runs:
///
/// 1. [`CargoBuildStep`] for `context.package`.
/// 2. [`RotateStep`] from `context.built_binary_path()` to
///    `context.install_path`.
///
/// Use [`DeployBuilder::without_defaults`] to fully replace the default plan.
pub struct DeployBuilder<'a> {
    context: &'a DeployContext,
    plan: DeployPlan,
    defaults: bool,
}

impl<'a> DeployBuilder<'a> {
    fn new(context: &'a DeployContext) -> Self {
        Self {
            context,
            plan: DeployPlan::new(),
            defaults: true,
        }
    }

    /// Add a hook that runs before the deployment steps. The usual use is
    /// stopping a service that owns the installed binary.
    pub fn pre<F>(mut self, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.plan.pre.push(Box::new(hook));
        self
    }

    /// Add a hook that runs after all deployment steps succeeded. The usual
    /// use is restarting or verifying a service.
    pub fn post<F>(mut self, hook: F) -> Self
    where
        F: FnMut(&DeployContext) -> Result<()> + 'static,
    {
        self.plan.post.push(Box::new(hook));
        self
    }

    /// Append a custom step after the defaults (or as the only step when
    /// [`without_defaults`](Self::without_defaults) is used).
    pub fn step(mut self, step: DeployStep) -> Self {
        self.plan.steps.push(step);
        self
    }

    /// Replace the default cargo-build + rotate plan with exactly the steps
    /// added by the caller.
    pub fn without_defaults(mut self) -> Self {
        self.defaults = false;
        self
    }

    pub fn run(mut self) -> Result<DeployReport> {
        if self.defaults {
            let mut steps = vec![
                DeployStep::CargoBuild(CargoBuildStep::from_context(self.context)),
                DeployStep::Rotate(RotateStep::from_context(self.context)),
            ];
            steps.append(&mut self.plan.steps);
            self.plan.steps = steps;
        }
        self.plan.run(self.context)
    }
}

/// Start a deploy plan. The returned builder includes the default build and
/// rotate steps; call [`DeployBuilder::run`] to execute it.
///
/// ```ignore
/// let context = tracel_deploy::DeployContext::from_args(&args, workspace, "my-app")?;
/// tracel_deploy::deploy(&context)
///     .pre(|_| stop_service())
///     .post(|_| start_and_verify_service())
///     .run()?;
/// ```
pub fn deploy(context: &DeployContext) -> DeployBuilder<'_> {
    DeployBuilder::new(context)
}

pub enum DeployStep {
    /// Run `cargo build -p <package>` in the workspace.
    CargoBuild(CargoBuildStep),
    /// Replace the installed binary, possibly while it is running.
    Rotate(RotateStep),
    /// Run a command. Useful for service stop/start steps.
    Run(RunStep),
    /// Arbitrary project-specific logic.
    Custom(DeployHook),
}

impl DeployStep {
    fn run(self, context: &DeployContext) -> Result<DeployStepReport> {
        match self {
            Self::CargoBuild(step) => step.run(context),
            Self::Rotate(step) => step.run(context),
            Self::Run(step) => step.run(context),
            Self::Custom(mut step) => {
                step(context)?;
                Ok(DeployStepReport {
                    name: "custom".to_owned(),
                    detail: None,
                })
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct CargoBuildStep {
    pub package: String,
    pub release: bool,
    pub extra_args: Vec<String>,
}

impl CargoBuildStep {
    pub fn from_context(context: &DeployContext) -> Self {
        Self {
            package: context.package.clone(),
            release: context.release,
            extra_args: context.build_args.clone(),
        }
    }

    fn run(self, context: &DeployContext) -> Result<DeployStepReport> {
        let mut args = vec![
            "build".to_owned(),
            "-p".to_owned(),
            self.package.clone(),
            "--color".to_owned(),
            "always".to_owned(),
        ];
        if self.release {
            args.push("--release".to_owned());
        }
        args.extend(self.extra_args);
        if !context.quiet {
            println!("cargo {}", args.join(" "));
        }
        let status = Command::new("cargo")
            .current_dir(&context.workspace_root)
            .args(&args)
            .status()
            .with_context(|| format!("failed to run cargo build for {}", self.package))?;
        if !status.success() {
            anyhow::bail!("cargo build -p {} failed with {status}", self.package);
        }
        Ok(DeployStepReport {
            name: "cargo build".to_owned(),
            detail: Some(self.package),
        })
    }
}

#[derive(Debug, Clone)]
pub struct RotateStep {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub backup_dir: Option<PathBuf>,
    pub strategy: RotateStrategy,
}

impl RotateStep {
    pub fn from_context(context: &DeployContext) -> Self {
        Self {
            source: context.built_binary_path(),
            destination: context.install_path.clone(),
            backup_dir: context.backup_dir.clone(),
            strategy: RotateStrategy::Auto,
        }
    }

    fn run(self, context: &DeployContext) -> Result<DeployStepReport> {
        let outcome = rotate(&RotateOptions {
            source: self.source,
            destination: self.destination,
            backup_dir: self.backup_dir,
            strategy: self.strategy,
            dry_run: context.dry_run,
        })?;
        let detail = match &outcome {
            RotateOutcome::WouldReplace { destination } => {
                format!("would replace {}", destination.display())
            }
            RotateOutcome::Replaced { destination } => {
                format!("replaced {}", destination.display())
            }
            RotateOutcome::ReplacedAfterRename {
                destination,
                backup,
            } => {
                format!(
                    "replaced {}; previous binary preserved at {}",
                    destination.display(),
                    backup.display()
                )
            }
        };
        Ok(DeployStepReport {
            name: "rotate".to_owned(),
            detail: Some(detail),
        })
    }
}

#[derive(Debug, Clone)]
pub struct RunStep {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub current_dir: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub allow_failure: bool,
}

impl RunStep {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            env: Vec::new(),
            allow_failure: false,
        }
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn allow_failure(mut self, allow: bool) -> Self {
        self.allow_failure = allow;
        self
    }

    fn run(self, context: &DeployContext) -> Result<DeployStepReport> {
        if !context.quiet {
            println!("{} {}", self.program.display(), self.args.join(" "));
        }
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if let Some(dir) = &self.current_dir {
            command.current_dir(dir);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        let status = command
            .status()
            .with_context(|| format!("failed to run {}", self.program.display()))?;
        if !status.success() && !self.allow_failure {
            anyhow::bail!("{} exited with {status}", self.program.display());
        }
        Ok(DeployStepReport {
            name: format!("run {}", self.program.display()),
            detail: None,
        })
    }
}

#[derive(Debug, Default)]
pub struct DeployReport {
    pub installed: PathBuf,
    pub dry_run: bool,
    pub steps: Vec<DeployStepReport>,
}

#[derive(Debug)]
pub struct DeployStepReport {
    pub name: String,
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::{DeployStep, deploy};
    use crate::context::DeployContext;

    #[test]
    fn pre_steps_and_post_hooks_run_in_order() {
        let mut context = DeployContext::new(std::env::temp_dir(), "tracel-deploy-test");
        context.quiet = true;
        let events = Rc::new(RefCell::new(Vec::<&'static str>::new()));
        let pre_events = Rc::clone(&events);
        let step_events = Rc::clone(&events);
        let post_events = Rc::clone(&events);

        deploy(&context)
            .without_defaults()
            .pre(move |_| {
                pre_events.borrow_mut().push("pre");
                Ok(())
            })
            .step(DeployStep::Custom(Box::new(move |_| {
                step_events.borrow_mut().push("step");
                Ok(())
            })))
            .post(move |_| {
                post_events.borrow_mut().push("post");
                Ok(())
            })
            .run()
            .expect("deploy plan");

        assert_eq!(*events.borrow(), ["pre", "step", "post"]);
    }

    #[test]
    fn dry_run_skips_hooks_and_steps() {
        let mut context = DeployContext::new(std::env::temp_dir(), "tracel-deploy-test");
        context.dry_run = true;
        let called = Rc::new(RefCell::new(false));
        let hook_called = Rc::clone(&called);

        deploy(&context)
            .without_defaults()
            .pre(move |_| {
                *hook_called.borrow_mut() = true;
                Ok(())
            })
            .run()
            .expect("dry-run plan");

        assert!(!*called.borrow());
    }
}
