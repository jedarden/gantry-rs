// gantry — Argo Workflows backend (plan §"Phase 1a", Components §5 "RemoteBackend trait").
//
// Phase 1a: builds Workflow manifests with serde (no string splicing, S-4), submits via
// kubectl, streams pod logs, polls status.phase for verdict, falls back to output parameters
// when podGC ate the logs.
//
// The Argo backend implements RemoteBackend using kubectl as the execution layer:
// - submit: builds Workflow manifest with serde, runs kubectl create -f -;
//   every submission carries `contract-version` (CONTRACT_VERSION) — the
//   client half of the verdict.json handshake the reference template echoes
//   back, and a foreign echo classifies the run infra in wait() below
// - stream_logs: kubectl logs -f on the workflow's pod
// - wait: polls kubectl get workflow status.phase until terminal or deadline
// - status: one-shot status.phase → RunStatus snapshot (never blocks, never
//   errors on an unanswerable query)
// - describe: returns the workflow name/UI URL
// - cancel: kubectl delete workflow
//
// Every blocking kubectl invocation routes through one injectable seam,
// [`KubectlRunner`] (production: [`ProcessKubectl`], which spawns the real
// binary; unit tests: an in-memory fake), so submit()'s error paths are
// unit-testable without a cluster or a kubectl executable — the same funnel
// pattern the command backend uses for its argv. Only `follow_pod_logs` sits
// outside the seam: streaming needs live process stdout to copy from as the
// run progresses, not a captured end-of-run result.

use crate::backend::{BackendError, RemoteBackend, RunSpec, RunStatus, Verdict, VerdictJson};
use crate::verdict::CONTRACT_VERSION;
use std::io::{Read, Write};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

/// How long stream_logs waits for the workflow's pod to exist before giving
/// up. Log streaming is best-effort (wait() is the authoritative verdict
/// source), so a workflow that never schedules must not hang the run.
const POD_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(300);
/// Poll interval while waiting for the workflow's pod to exist.
const POD_DISCOVERY_POLL: Duration = Duration::from_secs(1);
/// Poll interval while waiting for the workflow to reach a terminal phase.
const STATUS_POLL: Duration = Duration::from_secs(2);

/// Argo Workflow manifest structures (serde-based, no string splicing).
///
/// Phase 1a implements minimal Workflow submit spec matching the gantry-verify
/// template contract: parameters (repo, revision, args-json, contract-version,
/// builder-image), generateName, entrypoint, and a workflowTemplateRef to the
/// cluster's WorkflowTemplate. Workflow-level arguments are merged with the
/// template's arguments (argo-workflows docs §"Workflow Templates"): names the
/// workflow supplies take effect; names it omits keep the template's default —
/// which is how an unconfigured builder-image falls back to the template
/// default.
mod workflow {
    use crate::verdict::CONTRACT_VERSION;
    use serde::{Deserialize, Serialize};

    /// Workflow submit manifest.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Workflow {
        pub api_version: String,
        pub kind: String,
        pub metadata: Metadata,
        pub spec: WorkflowSpec,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Metadata {
        pub generate_name: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct WorkflowSpec {
        pub entrypoint: String,
        /// Reference to the cluster's WorkflowTemplate (namespaced by default:
        /// clusterScope false, same namespace as the workflow).
        pub workflow_template_ref: WorkflowTemplateRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub arguments: Option<Arguments>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Arguments {
        pub parameters: Vec<Parameter>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Parameter {
        pub name: String,
        pub value: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct WorkflowTemplateRef {
        pub name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub cluster_scope: Option<bool>,
    }

    /// The Workflow object as `kubectl get workflow -o json` returns it.
    /// Only `status` is read; every other field is ignored. `status` is
    /// absent entirely until the controller first reconciles the workflow.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct WorkflowObject {
        pub status: Option<WorkflowStatus>,
    }

    /// The `status` stanza of a Workflow.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct WorkflowStatus {
        /// Terminal phase (Succeeded / Failed / Error); None until the
        /// controller sets it — an early object may carry a bare status.
        pub phase: Option<String>,
        pub message: Option<String>,
        pub outputs: Option<Outputs>,
        pub nodes: Option<std::collections::HashMap<String, NodeStatus>>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Outputs {
        pub parameters: Option<Vec<OutputParameter>>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct OutputParameter {
        pub name: String,
        pub value: Option<String>,
        pub value_from: Option<ValueFrom>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ValueFrom {
        pub parameter: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct NodeStatus {
        pub phase: Option<String>,
        pub message: Option<String>,
        pub outputs: Option<Outputs>,
    }

    impl WorkflowStatus {
        /// Find an output parameter by name and return its value.
        ///
        /// Output parameters are how the gantry-verify template exports
        /// results that outlive the pod (`verdict` = verdict.json, `output` =
        /// the captured run log the backend recovers when podGC ate the pod).
        pub fn output_parameter(&self, name: &str) -> Option<&str> {
            self.outputs
                .as_ref()?
                .parameters
                .as_ref()?
                .iter()
                .find(|p| p.name == name)
                .and_then(|p| p.value.as_deref())
        }
    }

    impl Workflow {
        /// Create a new Workflow manifest for gantry.
        ///
        /// Parameters follow the gantry-verify template contract (plan §argo):
        /// repo, revision, args-json always; builder-image only when configured
        /// (omitting it lets the WorkflowTemplate default apply). `contract-version`
        /// is always sent — the client half of the handshake (plan §"Versioning &
        /// compatibility"): the template echoes it back verbatim in verdict.json,
        /// and the client reading a different echo is contract drift
        /// (src/verdict.rs). Gates stay unsent here: they are opt-in from trusted
        /// user config and the template's faithful-argv default ("[]") already
        /// implements Q-2.
        pub fn new(
            generate_name: &str,
            template_name: &str,
            repo_url: &str,
            sha: &str,
            args_json: &str,
            builder_image: Option<&str>,
        ) -> Self {
            let mut parameters = vec![
                Parameter {
                    name: "repo".to_string(),
                    value: repo_url.to_string(),
                },
                Parameter {
                    name: "revision".to_string(),
                    value: sha.to_string(),
                },
                Parameter {
                    name: "args-json".to_string(),
                    value: args_json.to_string(),
                },
                Parameter {
                    name: "contract-version".to_string(),
                    value: CONTRACT_VERSION.to_string(),
                },
            ];
            if let Some(image) = builder_image {
                parameters.push(Parameter {
                    name: "builder-image".to_string(),
                    value: image.to_string(),
                });
            }

            Workflow {
                api_version: "argoproj.io/v1alpha1".to_string(),
                kind: "Workflow".to_string(),
                metadata: Metadata {
                    generate_name: generate_name.to_string(),
                },
                spec: WorkflowSpec {
                    entrypoint: "gantry-verify".to_string(),
                    workflow_template_ref: WorkflowTemplateRef {
                        name: template_name.to_string(),
                        // Namespaced: the WorkflowTemplate lives in the same
                        // namespace as the submitted workflow (k8s default;
                        // omitted rather than written out as false).
                        cluster_scope: None,
                    },
                    arguments: Some(Arguments { parameters }),
                },
            }
        }
    }
}

use workflow::{Workflow, WorkflowObject};

/// RunHandle for Argo workflows.
///
/// Phase 1a: contains workflow name and optional pod name for log streaming.
/// The pod name is discovered after workflow submission.
#[derive(Debug, Clone, PartialEq)]
pub struct ArgoHandle {
    /// Workflow name (output of submit).
    pub workflow_name: String,
    /// Pod name (discovered during streaming, may be empty initially).
    pub pod_name: Option<String>,
    /// Namespace where the workflow runs.
    pub namespace: String,
}

/// Argo configuration from config file.
///
/// Phase 1a: kubectl path, WorkflowTemplate reference, and submit plumbing
/// (kubeconfig, namespace, generate_name, builder_image, base_url).
#[derive(Debug, Clone, PartialEq)]
pub struct ArgoConfig {
    /// Path to the kubectl binary ("kubectl" = resolve via PATH).
    /// Configurable so the backend is testable against mock executables.
    pub kubectl_path: String,
    /// Path to kubeconfig file (empty = use default).
    pub kubeconfig: String,
    /// Kubernetes namespace.
    pub namespace: String,
    /// WorkflowTemplate name to reference.
    pub template: String,
    /// generateName prefix for submitted workflows.
    pub generate_name: String,
    /// Builder image passed as the template's `builder-image` parameter.
    /// None omits the parameter so the WorkflowTemplate default applies
    /// (an empty override would break the template; Q-4 governs repo-layer
    /// selection, so user config supplies the trusted value for now).
    pub builder_image: Option<String>,
    /// Base URL for Argo UI (optional, for describe() to return human-readable URLs).
    pub base_url: Option<String>,
}

impl Default for ArgoConfig {
    fn default() -> Self {
        ArgoConfig {
            kubectl_path: "kubectl".to_string(),
            kubeconfig: "".to_string(),
            namespace: "argo-workflows".to_string(),
            template: "gantry-verify".to_string(),
            generate_name: "gantry-".to_string(),
            builder_image: None,
            base_url: None,
        }
    }
}

/// The captured result of one kubectl invocation.
///
/// The backend only ever asks whether the call succeeded and reads the
/// captured streams (submit surfaces stderr, status polling parses stdout),
/// so an exit code plus the two captures is the whole contract — a signal
/// death (`code: None`) simply never counts as success.
#[derive(Debug, Clone, PartialEq)]
pub struct KubectlOutcome {
    /// Process exit code (`None` = killed by a signal), mirroring
    /// `std::process::ExitStatus::code()`.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl KubectlOutcome {
    /// Whether kubectl exited successfully (exit code 0).
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

impl From<Output> for KubectlOutcome {
    fn from(output: Output) -> Self {
        KubectlOutcome {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

/// Seam for kubectl process invocation.
///
/// Every blocking kubectl call the backend makes (submit's `create -f -`,
/// `get workflow`, `get pods`, `delete workflow`) funnels through this one
/// trait, the way the command backend funnels its argv through a single
/// `run_command` helper. Production injects [`ProcessKubectl`]; unit tests
/// inject an in-memory fake so submit()'s error paths run without a cluster
/// or a kubectl binary.
pub trait KubectlRunner: Send + Sync {
    /// Run kubectl with `args`, feeding `stdin` to the process when given
    /// (submit pipes the Workflow manifest into `create -f -`).
    ///
    /// A child that exits before reading all of stdin (broken pipe) is not an
    /// error here: the caller diagnoses it from the returned non-zero status
    /// and captured stderr. Only spawn and wait failures are Err.
    fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<KubectlOutcome, BackendError>;
}

/// Production [`KubectlRunner`]: spawns the configured kubectl binary.
pub struct ProcessKubectl {
    kubectl_path: String,
    kubeconfig: String,
    namespace: String,
}

impl ProcessKubectl {
    /// Build the runner for `config`'s kubectl path, kubeconfig, and namespace.
    pub fn from_config(config: &ArgoConfig) -> Self {
        ProcessKubectl {
            kubectl_path: config.kubectl_path.clone(),
            kubeconfig: config.kubeconfig.clone(),
            namespace: config.namespace.clone(),
        }
    }

    /// A Command with the connection flags (`--kubeconfig`, `-n`) applied;
    /// callers add only the subcommand arguments.
    fn base_command(&self) -> Command {
        let mut cmd = Command::new(&self.kubectl_path);

        // Add kubeconfig flag if set
        if !self.kubeconfig.is_empty() {
            cmd.arg("--kubeconfig").arg(&self.kubeconfig);
        }

        // Add namespace flag
        cmd.arg("-n").arg(&self.namespace);
        cmd
    }
}

impl KubectlRunner for ProcessKubectl {
    fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<KubectlOutcome, BackendError> {
        let output = match stdin {
            // No stdin: capture-and-wait in one step.
            None => self
                .base_command()
                .args(args)
                .output()
                .map_err(|e| BackendError::new(&format!("failed to run kubectl: {}", e)))?,
            // Manifest on stdin: spawn with all three pipes, write the
            // payload, then collect. A broken pipe means kubectl exited
            // before reading it (e.g. the manifest was rejected client-side)
            // — fall through so wait_with_output surfaces kubectl's stderr
            // as the real error instead of masking it.
            Some(manifest) => {
                let mut child = self
                    .base_command()
                    .args(args)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .map_err(|e| BackendError::new(&format!("failed to spawn kubectl: {}", e)))?;

                if let Some(mut pipe) = child.stdin.take() {
                    if let Err(e) = pipe.write_all(manifest) {
                        if e.kind() != std::io::ErrorKind::BrokenPipe {
                            return Err(BackendError::new(&format!(
                                "failed to write workflow: {}",
                                e
                            )));
                        }
                    }
                    // Dropping `pipe` signals EOF, letting kubectl finish.
                }

                child
                    .wait_with_output()
                    .map_err(|e| BackendError::new(&format!("failed to wait for kubectl: {}", e)))?
            }
        };
        Ok(output.into())
    }
}

/// Outcome of bounded pod discovery for log streaming.
#[derive(Debug, Clone, PartialEq)]
enum PodDiscovery {
    /// The workflow's pod exists — stream logs from it.
    Pod(String),
    /// The workflow reached a terminal phase with no pod ever observed:
    /// podGC (`OnPodCompletion`) deleted the pod at completion, so there is
    /// nothing left to stream from and the log must be recovered from the
    /// workflow's `output` parameter instead.
    PodGone,
}

/// The `status.phase` ladder of an Argo Workflow, classified for polling.
///
/// status.phase is the authoritative terminal signal (plan §"argo"): the
/// `verdict` output parameter refines the classification *within* a terminal
/// rung but never moves the workflow between rungs. Every value kubectl can
/// serve is either a known pending rung, a known terminal rung, or a loud
/// error — a phase string gantry does not recognize must not read as "keep
/// waiting", or a controller speaking an unexpected phase would poll forever
/// exactly when something has gone wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowPhase {
    /// Submitted but not yet executing (controller queue or pod scheduling).
    Pending,
    /// Currently executing.
    Running,
    /// Finished: the suite ran and passed.
    Succeeded,
    /// Finished: the suite ran and failed.
    Failed,
    /// Finished broken: the workflow itself failed (template not found,
    /// controller error) — no test result exists.
    Error,
}

impl WorkflowPhase {
    /// Classify one observed `status.phase` value.
    ///
    /// `Ok(None)` covers the two pending shapes that carry no phase value at
    /// all: the `status` stanza is absent entirely (the controller has not
    /// reconciled the workflow yet) or present but phase-less. An
    /// unrecognized string is a loud error naming the value, never a silent
    /// retry.
    fn parse(phase: Option<&str>) -> Result<Option<Self>, BackendError> {
        match phase {
            None | Some("") => Ok(None),
            Some("Pending") => Ok(Some(Self::Pending)),
            Some("Running") => Ok(Some(Self::Running)),
            Some("Succeeded") => Ok(Some(Self::Succeeded)),
            Some("Failed") => Ok(Some(Self::Failed)),
            Some("Error") => Ok(Some(Self::Error)),
            Some(unknown) => Err(BackendError::new(&format!(
                "unknown workflow status phase: {:?}",
                unknown
            ))),
        }
    }

    /// Whether the workflow has finished (Succeeded / Failed / Error);
    /// Pending and Running (and the phase-less pending shapes) have not.
    fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Error)
    }

    /// Phase-only verdict fallback for a terminal phase whose `verdict`
    /// output parameter is absent or unparseable.
    ///
    /// Routes through the shared exit-code-only classifier
    /// ([`VerdictJson::from_exit_code`]) rather than a private ladder: the
    /// terminal phase pins the exit code the run must have had — Succeeded
    /// means the suite exited 0, Failed means something exited non-zero (a
    /// test failure, absent verdict.json's gate attribution) — and the
    /// classifier applies the Error precedence and exit ladder exactly as a
    /// parsed document would. The rungs with no test outcome feed the ≥2
    /// infra bucket: no exit code exists, which IS the infra case.
    fn fallback_verdict(self) -> Verdict {
        let (phase, exit_code) = match self {
            Self::Succeeded => ("Succeeded", 0),
            Self::Failed => ("Failed", 1),
            // The workflow itself broke, so no test result exists to report.
            // (The pending rungs can never reach this terminal fallback; they
            // classify as infra if one ever does.)
            Self::Pending => ("Pending", 2),
            Self::Running => ("Running", 2),
            Self::Error => ("Error", 2),
        };
        VerdictJson::from_exit_code(phase, exit_code)
    }
}

/// The Argo Workflows backend implementation.
pub struct ArgoBackend {
    /// Argo-specific configuration.
    config: ArgoConfig,
    /// The kubectl execution seam: the production binary runner, or a test
    /// fake when the backend is built through `with_runner`.
    kubectl: Box<dyn KubectlRunner>,
    /// Poll interval while waiting for a terminal phase. Production value:
    /// [`STATUS_POLL`]; tests shorten it so polling scenarios stay fast.
    status_poll: Duration,
    /// Poll interval while discovering the workflow's log pod. Production
    /// value: [`POD_DISCOVERY_POLL`]; tests shorten it.
    discovery_poll: Duration,
}

impl ArgoBackend {
    /// Create a new ArgoBackend with configuration.
    pub fn new(config: ArgoConfig) -> Self {
        let kubectl = Box::new(ProcessKubectl::from_config(&config));
        ArgoBackend {
            config,
            kubectl,
            status_poll: STATUS_POLL,
            discovery_poll: POD_DISCOVERY_POLL,
        }
    }

    /// Create a new ArgoBackend with default config.
    pub fn default_config() -> Self {
        ArgoBackend::new(ArgoConfig::default())
    }

    /// Create a new ArgoBackend around an injected kubectl runner (tests
    /// hand in an in-memory fake; no cluster, no kubectl binary).
    ///
    /// Poll intervals default fast (1ms) so wait()/discovery loops exercising
    /// several rungs stay fast; a test that needs to observe cadence
    /// overrides the `status_poll`/`discovery_poll` fields directly.
    #[cfg(test)]
    fn with_runner(config: ArgoConfig, kubectl: Box<dyn KubectlRunner>) -> Self {
        ArgoBackend {
            config,
            kubectl,
            status_poll: Duration::from_millis(1),
            discovery_poll: Duration::from_millis(1),
        }
    }

    /// Run kubectl with arguments and return its captured outcome.
    fn kubectl(&self, args: &[&str]) -> Result<KubectlOutcome, BackendError> {
        self.kubectl.run(args, None)
    }

    /// Fetch and parse the workflow's `status` stanza.
    ///
    /// `Ok(None)` means kubectl could not serve the object (not created yet,
    /// transient API error) — pending for callers that poll, not an error. A
    /// malformed object is a loud error: gantry never guesses at a status it
    /// cannot parse.
    fn workflow_status(
        &self,
        workflow_name: &str,
    ) -> Result<Option<workflow::WorkflowStatus>, BackendError> {
        let output = self.kubectl(&["get", "workflow", workflow_name, "-o", "json"])?;
        if !output.success() {
            return Ok(None);
        }
        let json = String::from_utf8_lossy(&output.stdout);
        let obj: WorkflowObject = serde_json::from_str(&json)
            .map_err(|e| BackendError::new(&format!("failed to parse workflow status: {}", e)))?;
        Ok(obj.status)
    }

    /// Fetch the workflow and classify its `status.phase` on the ladder.
    ///
    /// `Ok(None)` = the workflow object is not retrievable yet (transient
    /// kubectl failure, or the controller has not created/reconciled it) —
    /// pending for callers that poll. A malformed object and an unrecognized
    /// phase string are loud errors from [`Self::workflow_status`] and
    /// [`WorkflowPhase::parse`] respectively.
    fn workflow_phase(&self, workflow_name: &str) -> Result<Option<WorkflowPhase>, BackendError> {
        match self.workflow_status(workflow_name)? {
            None => Ok(None),
            Some(status) => WorkflowPhase::parse(status.phase.as_deref()),
        }
    }

    /// Read one output parameter's value from the workflow's status.
    /// `Ok(None)` = workflow not retrievable yet or parameter absent.
    fn read_output_parameter(
        &self,
        workflow_name: &str,
        wanted: &str,
    ) -> Result<Option<String>, BackendError> {
        Ok(self
            .workflow_status(workflow_name)?
            .and_then(|status| status.output_parameter(wanted).map(str::to_string)))
    }

    /// Discover the pod to stream logs from, bounded by `timeout`.
    ///
    /// The loop is deadline-aware: it converts `timeout` into a deadline up
    /// front, re-checks it before every poll, and clamps each sleep to the
    /// time remaining — so it can never loop or sleep past the budget. Expiry
    /// is a loud [`BackendError`], not a hang.
    ///
    /// Returns [`PodDiscovery::PodGone`] as soon as the workflow itself goes
    /// terminal with no pod ever observed: podGC (`OnPodCompletion`) deletes
    /// the pod the moment the run finishes, so a fast run goes straight from
    /// "no pod" to a terminal workflow — waiting out the full timeout would
    /// idle five minutes on every quick run before the output-parameter
    /// fallback could fire. An unrecognized phase string errors immediately
    /// rather than counting as "not terminal yet".
    fn discover_pod_or_terminal(
        &self,
        workflow_name: &str,
        timeout: Duration,
    ) -> Result<PodDiscovery, BackendError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(pod) = self.discover_pod(workflow_name)? {
                return Ok(PodDiscovery::Pod(pod));
            }
            if self
                .workflow_phase(workflow_name)?
                .is_some_and(WorkflowPhase::is_terminal)
            {
                return Ok(PodDiscovery::PodGone);
            }
            // Never sleep past the discovery deadline: the clamp bounds this
            // iteration, and zero remaining ends the loop here.
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(BackendError::new(&format!(
                    "pod discovery deadline exceeded: no pod found for workflow {} \
                     within {:?}",
                    workflow_name, timeout
                )));
            }
            thread::sleep(self.discovery_poll.min(remaining));
        }
    }

    /// Stream `kubectl logs -f <pod>` into `out` as the logs arrive.
    ///
    /// `-f` blocks until the pod's log stream closes, so output must be
    /// copied through incrementally — a buffered `.output()` would withhold
    /// every line until the run finished, defeating the point of streaming.
    /// kubectl's stderr is drained concurrently (a full stderr pipe would
    /// otherwise deadlock the copy) and reported when the stream fails.
    ///
    /// This is the one kubectl invocation deliberately NOT on the
    /// [`KubectlRunner`] seam: streaming needs live process stdout to copy
    /// from as the run progresses, not a captured end-of-run result.
    fn follow_pod_logs(&self, pod_name: &str, out: &mut dyn Write) -> Result<(), BackendError> {
        let mut cmd = Command::new(&self.config.kubectl_path);

        // Add kubeconfig flag if set
        if !self.config.kubeconfig.is_empty() {
            cmd.arg("--kubeconfig").arg(&self.config.kubeconfig);
        }

        // Add namespace flag
        cmd.arg("-n").arg(&self.config.namespace);
        cmd.args(["logs", "-f", pod_name]);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| BackendError::new(&format!("failed to spawn kubectl logs: {}", e)))?;

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| BackendError::new("kubectl logs has no stdout pipe"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| BackendError::new("kubectl logs has no stderr pipe"))?;
        let stderr_drain = thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        });

        let copied = std::io::copy(&mut stdout, out);
        let status = child
            .wait()
            .map_err(|e| BackendError::new(&format!("failed to wait for kubectl logs: {}", e)))?;
        let stderr_buf = stderr_drain
            .join()
            .unwrap_or_else(|_| b"kubectl logs stderr unreadable".to_vec());

        match copied {
            Ok(_) if status.success() => Ok(()),
            Ok(_) => Err(BackendError::new(&format!(
                "kubectl logs -f {} failed: {}",
                pod_name,
                String::from_utf8_lossy(&stderr_buf).trim()
            ))),
            Err(e) => Err(BackendError::new(&format!(
                "failed to stream pod logs: {}",
                e
            ))),
        }
    }

    /// Recover the run log when podGC deleted the pod: the workflow's
    /// `output` output parameter carries the captured log (contrib template
    /// tees the step's output into it). `Ok(None)` = the parameter is absent.
    fn recover_output_log(&self, workflow_name: &str) -> Result<Option<String>, BackendError> {
        self.read_output_parameter(workflow_name, "output")
    }

    /// Discover the pod name for a workflow by listing pods.
    fn discover_pod(&self, workflow_name: &str) -> Result<Option<String>, BackendError> {
        let output = self.kubectl(&[
            "get",
            "pods",
            "-l",
            &format!("workflows.argoproj.io/workflow={}", workflow_name),
            "-o",
            "json",
        ])?;

        if !output.success() {
            return Ok(None); // No pods found yet
        }

        let json = String::from_utf8_lossy(&output.stdout);
        let pod_list: serde_json::Value = serde_json::from_str(&json)
            .map_err(|e| BackendError::new(&format!("failed to parse pod list: {}", e)))?;

        if let Some(items) = pod_list["items"].as_array() {
            if let Some(first_pod) = items.first() {
                if let Some(name) = first_pod["metadata"]["name"].as_str() {
                    return Ok(Some(name.to_string()));
                }
            }
        }

        Ok(None)
    }
}

impl RemoteBackend for ArgoBackend {
    /// Submit a workflow to Argo.
    ///
    /// Builds the Workflow manifest with serde and pipes it into
    /// `kubectl create -f -` through the [`KubectlRunner`] seam. The workflow
    /// name (stdout) becomes the handle.
    fn submit(&self, spec: &RunSpec) -> Result<crate::backend::RunHandle, BackendError> {
        // Format args as JSON array
        let args_json = serde_json::to_string(&spec.args)
            .map_err(|e| BackendError::new(&format!("failed to serialize args: {}", e)))?;

        // Build Workflow manifest with serde (no string splicing)
        let workflow = Workflow::new(
            &self.config.generate_name,
            &self.config.template,
            &spec.repo_url,
            &spec.sha,
            &args_json,
            self.config.builder_image.as_deref(),
        );

        // Serialize to JSON
        let workflow_json = serde_json::to_string_pretty(&workflow)
            .map_err(|e| BackendError::new(&format!("failed to serialize workflow: {}", e)))?;

        // Submit via kubectl create -f - (spawn, stdin piping, and the
        // broken-pipe tolerance all live behind the runner seam)
        let output = self.kubectl.run(
            ["create", "-f", "-"].as_slice(),
            Some(workflow_json.as_bytes()),
        )?;

        if !output.success() {
            return Err(BackendError::new(&format!(
                "workflow submission failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        // Extract the workflow name from stdout. Real kubectl prints
        // "workflow.argoproj.io/<generated-name> created" — keep only the
        // name token, never the status word. The name must start immediately
        // after the prefix: nothing (or only whitespace) after the slash
        // means kubectl emitted no name, and falling through would mint a
        // garbage handle out of the status word ("created"), so any
        // malformed stdout is a loud error.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        let workflow_name = trimmed
            .strip_prefix("workflow.argoproj.io/")
            .and_then(|rest| {
                if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                    return None;
                }
                rest.split_whitespace().next()
            })
            .ok_or_else(|| {
                BackendError::new(&format!(
                    "kubectl output missing workflow name: {:?}",
                    trimmed
                ))
            })?
            .to_string();

        Ok(crate::backend::RunHandle {
            handle: workflow_name.clone(),
        })
    }

    /// Stream logs from the workflow's pod.
    ///
    /// Discovers the pod name, then streams `kubectl logs -f` into `out` as
    /// the logs arrive. Best-effort: failures don't fail the overall run
    /// (wait is authoritative).
    ///
    /// podGC (`OnPodCompletion`) deletes the pod the moment the run finishes,
    /// so the pod may be gone before we ever see it (fast run) or vanish
    /// mid-stream. Both recover the run log from the workflow's `output`
    /// output parameter (the contrib template tees the step's output into
    /// it); only when that is missing too does streaming fail loudly.
    fn stream_logs(
        &self,
        h: &crate::backend::RunHandle,
        out: &mut dyn Write,
    ) -> Result<(), BackendError> {
        // Discover the pod, bounded — a workflow that never schedules must
        // fail streaming loudly instead of hanging the run.
        match self.discover_pod_or_terminal(&h.handle, POD_DISCOVERY_TIMEOUT)? {
            PodDiscovery::Pod(pod_name) => {
                if self.follow_pod_logs(&pod_name, out).is_ok() {
                    return Ok(());
                }
                eprintln!(
                    "[gantry] pod log stream ended early; recovering log from output parameter"
                );
            }
            PodDiscovery::PodGone => {
                eprintln!(
                    "[gantry] no pod for workflow {} (podGC deleted it at completion); \
                     recovering log from output parameter",
                    h.handle
                );
            }
        }

        // podGC recovery: the pod is unusable, so the captured log in the
        // `output` parameter is the last copy that exists.
        match self.recover_output_log(&h.handle)? {
            Some(recovered) => out
                .write_all(recovered.as_bytes())
                .map_err(|e| BackendError::new(&format!("failed to write logs: {}", e))),
            None => Err(BackendError::new(&format!(
                "no logs available for workflow {}: no pod to stream and no `output` \
                 parameter to recover",
                h.handle
            ))),
        }
    }

    /// Wait for the workflow to complete and return its verdict.
    ///
    /// Polls `kubectl get workflow -o json` until `status.phase` reaches a
    /// terminal rung or the deadline expires, whichever comes first. The loop
    /// is deadline-aware in both directions: it re-checks the deadline before
    /// every poll, and each pending sleep is clamped to the time remaining —
    /// wait() can never loop (or sleep) past its deadline. Expiry is a loud
    /// [`BackendError`] (structured `deadline_exceeded`, so the caller
    /// classifies it InfraFailure — never a fabricated verdict) carrying
    /// [`Self::describe`]: the watch is abandoned, the workflow itself keeps
    /// running on the cluster, and the run URL is how the operator finds it.
    ///
    /// The phase ladder is explicit ([`WorkflowPhase`]): no status / bare
    /// status / Pending / Running are pending; Succeeded / Failed / Error are
    /// terminal; any other phase string is a loud error rather than a silent
    /// retry. Malformed workflow JSON is likewise a loud error from
    /// [`Self::workflow_status`] — gantry never guesses at a status it cannot
    /// parse.
    ///
    /// Within a terminal phase the `verdict` output parameter (verdict.json)
    /// decides the verdict. All three shapes of "no usable verdict.json" —
    /// the parameter absent, malformed JSON, an unsupported schema_version —
    /// degrade through one shared exit-code-only path
    /// ([`VerdictJson::from_exit_code`]): the parameter absent degrades
    /// silently, while a parse failure first surfaces its typed
    /// [`BackendError`] on stderr. Either way the terminal phase classifies
    /// the run (Succeeded → Pass, Failed → TestFailure, Error →
    /// InfraFailure). status.phase remains the authoritative terminal signal.
    /// Attributions gate failures to "[gantry] gate:" in output, and a
    /// contract-version echo this client does not speak surfaces as the
    /// explicit "contract drift" message the plan's versioning section
    /// requires before its InfraFailure classification.
    fn wait(
        &self,
        h: &crate::backend::RunHandle,
        deadline: Instant,
    ) -> Result<Verdict, BackendError> {
        loop {
            // Deadline first: no poll, sleep, or verdict may happen past it.
            // The expiry abandons the watch — it does not cancel the
            // workflow — so the error carries describe(): the run URL (or
            // bare identifier) the operator needs once gantry stops
            // watching (features.md v1.x "here's-the-run-URL message").
            if Instant::now() >= deadline {
                return Err(BackendError::deadline_with_url(
                    &format!(
                        "workflow {} deadline exceeded while polling status.phase",
                        h.handle
                    ),
                    &self.describe(h),
                ));
            }

            // The `status` stanza is absent until the controller first
            // reconciles the workflow (and may be phase-less right after) —
            // both are pending, not errors. A malformed object or an
            // unrecognized phase errors loudly instead of retrying.
            let status = self.workflow_status(&h.handle)?;
            let phase = WorkflowPhase::parse(status.as_ref().and_then(|s| s.phase.as_deref()))?;

            let Some(phase) = phase.filter(|p| p.is_terminal()) else {
                // Pending shape: sleep until the next poll, but never past
                // the deadline — the clamp guarantees the next loop-top check
                // lands on time.
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(self.status_poll.min(remaining));
                continue;
            };

            // Terminal - try to read verdict.json from the workflow's
            // `verdict` output parameter.
            if let Some(value) = status.as_ref().and_then(|s| s.output_parameter("verdict")) {
                match VerdictJson::parse(value) {
                    Ok(vj) => {
                        // Contract drift first, loudly: the document echoed a
                        // contract this client does not speak, so nothing
                        // else it claims can be trusted — to_verdict()
                        // classifies it InfraFailure, and the plan's
                        // versioning section requires that classification to
                        // arrive with an explicit "contract drift" message,
                        // never as a bare verdict.
                        if let Some(echo) = vj.contract_drift() {
                            eprintln!(
                                "[gantry] contract drift: verdict.json echoes contract_version {:?}, this client speaks {:?} — treating as infra failure",
                                echo, CONTRACT_VERSION
                            );
                        }
                        let verdict = vj.to_verdict();
                        // Attributions for gate failures
                        if verdict == Verdict::GateFailure {
                            eprintln!("[gantry] gate: quality gate failed");
                        }
                        return Ok(verdict);
                    }
                    Err(e) => {
                        // The typed parse error (malformed JSON, unsupported
                        // schema_version) is reported, not raised: an unusable
                        // document is a degradation, not a wait failure.
                        eprintln!("[gantry] failed to parse verdict.json: {}", e);
                    }
                }
            }
            // No `verdict` parameter at all lands here without a whisper —
            // absence is the silent shape of the degradation.

            // Every shape of "no usable verdict.json" converges on the shared
            // exit-code-only classifier ([`VerdictJson::from_exit_code`], via
            // [`WorkflowPhase::fallback_verdict`]) — the same ladder a parsed
            // document runs, fed only what the terminal phase can vouch for.
            return Ok(phase.fallback_verdict());
        }
    }

    /// Describe the workflow for human consumption.
    ///
    /// Returns a human-readable URL to the workflow in the Argo UI.
    /// If base_url is configured, returns a full URL like:
    /// "https://argo-ui.example.com/workflows/{namespace}/{workflow-name}"
    /// Otherwise returns a simplified identifier.
    fn describe(&self, h: &crate::backend::RunHandle) -> String {
        if let Some(base_url) = &self.config.base_url {
            format!(
                "{}/workflows/{}/{}",
                base_url.trim_end_matches('/'),
                self.config.namespace,
                h.handle
            )
        } else {
            format!("workflow/{}", h.handle)
        }
    }

    /// Cancel the running workflow.
    ///
    /// Runs kubectl delete workflow.
    fn cancel(&self, h: &crate::backend::RunHandle) -> Result<(), BackendError> {
        let output = self.kubectl(&["delete", "workflow", &h.handle])?;

        if !output.success() {
            return Err(BackendError::new(&format!(
                "failed to cancel workflow: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(())
    }

    /// Query the workflow's current state without waiting for it.
    ///
    /// One `get workflow -o json` through the same plumbing wait() polls,
    /// with `status.phase` mapped onto the coarse [`RunStatus`] ladder:
    /// Pending and Running are themselves, all three terminal rungs
    /// (Succeeded / Failed / Error) are Completed.
    ///
    /// Every shape of "no answer" — the workflow object not retrievable yet,
    /// no `status` stanza, no phase value, an unrecognized phase string, a
    /// malformed object — is [`RunStatus::Unknown`], never an error: this is
    /// a point-in-time snapshot, so an unanswerable query is "no news" for a
    /// polling caller (plan §argo's degrade-gracefully stance). That is a
    /// deliberate softening of wait(), where the same unrecognized phase is
    /// a loud error — a poller that must eventually return a verdict cannot
    /// afford to wait forever, while a status poller loses nothing by
    /// trying again later.
    fn status(&self, h: &crate::backend::RunHandle) -> Result<RunStatus, BackendError> {
        Ok(match self.workflow_phase(&h.handle) {
            Ok(Some(WorkflowPhase::Pending)) => RunStatus::Pending,
            Ok(Some(WorkflowPhase::Running)) => RunStatus::Running,
            Ok(Some(phase)) if phase.is_terminal() => RunStatus::Completed,
            // No retrievable status (`Ok(None)`), an unrecognized phase
            // string, a malformed document, or a failed kubectl (`Err`):
            // Unknown — the query has no answer, not a failure.
            _ => RunStatus::Unknown,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::RunHandle;
    use crate::verdict::CONTRACT_VERSION;

    #[test]
    fn test_workflow_manifest_serialization() {
        let workflow = Workflow::new(
            "gantry-",
            "gantry-verify",
            "https://github.com/example/repo",
            "abc123",
            r#"["test","--","--nocapture"]"#,
            Some("rust:1.83"),
        );

        let json = serde_json::to_string(&workflow);
        assert!(json.is_ok());

        // Verify no string splicing - all fields are structured
        let parsed: Workflow = serde_json::from_str(&json.unwrap()).unwrap();
        assert_eq!(parsed.metadata.generate_name, "gantry-");
    }

    /// The serialized manifest must match the expected Kubernetes/YAML structure
    /// exactly: k8s camelCase key names, the gantry-verify template contract, and
    /// all five parameters (repo, revision, args-json, contract-version,
    /// builder-image). kubectl receives JSON on stdin, and JSON is a YAML subset,
    /// so this pins the YAML manifest shape too. Object key order is irrelevant
    /// to the comparison.
    #[test]
    fn test_workflow_manifest_matches_expected_yaml_structure() {
        let workflow = Workflow::new(
            "gantry-",
            "gantry-verify",
            "https://github.com/example/repo",
            "abc123",
            r#"["test","--","--nocapture"]"#,
            Some("rust:1.83"),
        );

        let actual: serde_json::Value =
            serde_json::to_value(&workflow).expect("manifest must serialize");

        let expected = serde_json::json!({
            "apiVersion": "argoproj.io/v1alpha1",
            "kind": "Workflow",
            "metadata": {
                "generateName": "gantry-"
            },
            "spec": {
                "entrypoint": "gantry-verify",
                "workflowTemplateRef": {
                    "name": "gantry-verify"
                },
                "arguments": {
                    "parameters": [
                        { "name": "repo", "value": "https://github.com/example/repo" },
                        { "name": "revision", "value": "abc123" },
                        { "name": "args-json", "value": r#"["test","--","--nocapture"]"# },
                        { "name": "contract-version", "value": CONTRACT_VERSION },
                        { "name": "builder-image", "value": "rust:1.83" },
                    ]
                }
            }
        });

        assert_eq!(actual, expected);
    }

    /// Unconfigured builder image must omit the parameter entirely, so the
    /// WorkflowTemplate default applies (an empty-value override would break it).
    /// `contract-version` is not optional — it rides every submission.
    #[test]
    fn test_workflow_manifest_omits_builder_image_when_unset() {
        let workflow = Workflow::new(
            "gantry-",
            "gantry-verify",
            "https://github.com/example/repo",
            "abc123",
            "[]",
            None,
        );

        let actual: serde_json::Value =
            serde_json::to_value(&workflow).expect("manifest must serialize");

        // The whole manifest must match the four-parameter shape exactly:
        // no `builder-image` parameter, no `clusterScope` in the template
        // ref, and no other structural drift.
        let expected = serde_json::json!({
            "apiVersion": "argoproj.io/v1alpha1",
            "kind": "Workflow",
            "metadata": {
                "generateName": "gantry-"
            },
            "spec": {
                "entrypoint": "gantry-verify",
                "workflowTemplateRef": {
                    "name": "gantry-verify"
                },
                "arguments": {
                    "parameters": [
                        { "name": "repo", "value": "https://github.com/example/repo" },
                        { "name": "revision", "value": "abc123" },
                        { "name": "args-json", "value": "[]" },
                        { "name": "contract-version", "value": CONTRACT_VERSION },
                    ]
                }
            }
        });

        assert_eq!(actual, expected);
    }

    /// Every field of the Default impl is asserted: a new field added to
    /// ArgoConfig must land here too, or the default drifts silently.
    #[test]
    fn test_argo_config_default() {
        let config = ArgoConfig::default();
        assert_eq!(config.kubectl_path, "kubectl");
        assert_eq!(config.kubeconfig, ""); // empty = cluster default
        assert_eq!(config.namespace, "argo-workflows");
        assert_eq!(config.template, "gantry-verify");
        assert_eq!(config.generate_name, "gantry-");
        assert_eq!(config.builder_image, None); // omit the parameter
        assert_eq!(config.base_url, None);
    }

    // verdict.json parser tests (schema versioning, failure_class presence /
    // absence / invalid shapes, oom and deadline_exceeded defaults, the verdict
    // ladder) live in src/verdict.rs — the single definition site of the
    // verdict.json schema — rather than being duplicated here against the
    // extraction (bf-2jnj, gantry-3eb02ee9). The tests below cover the
    // *backend* side: wait() feeding a fetched verdict.json document through
    // VerdictJson::parse to the Verdict it reports.

    #[test]
    fn test_argo_backend_describe_without_base_url() {
        let config = ArgoConfig {
            kubectl_path: "kubectl".to_string(),
            kubeconfig: "".to_string(),
            namespace: "argo-workflows".to_string(),
            template: "gantry-verify".to_string(),
            generate_name: "gantry-".to_string(),
            builder_image: None,
            base_url: None,
        };
        let backend = ArgoBackend::new(config);
        let handle = RunHandle::new("test-workflow-abc123");

        let description = backend.describe(&handle);
        assert_eq!(description, "workflow/test-workflow-abc123");
    }

    #[test]
    fn test_argo_backend_describe_with_base_url() {
        let config = ArgoConfig {
            kubectl_path: "kubectl".to_string(),
            kubeconfig: "".to_string(),
            namespace: "argo-workflows".to_string(),
            template: "gantry-verify".to_string(),
            generate_name: "gantry-".to_string(),
            builder_image: None,
            base_url: Some("https://argo.example.com".to_string()),
        };
        let backend = ArgoBackend::new(config);
        let handle = RunHandle::new("test-workflow-abc123");

        let description = backend.describe(&handle);
        assert_eq!(
            description,
            "https://argo.example.com/workflows/argo-workflows/test-workflow-abc123"
        );
    }

    #[test]
    fn test_argo_backend_describe_with_base_url_trailing_slash() {
        let config = ArgoConfig {
            kubectl_path: "kubectl".to_string(),
            kubeconfig: "".to_string(),
            namespace: "my-namespace".to_string(),
            template: "gantry-verify".to_string(),
            generate_name: "gantry-".to_string(),
            builder_image: None,
            base_url: Some("https://argo.example.com/".to_string()),
        };
        let backend = ArgoBackend::new(config);
        let handle = RunHandle::new("test-workflow-abc123");

        let description = backend.describe(&handle);
        assert_eq!(
            description,
            "https://argo.example.com/workflows/my-namespace/test-workflow-abc123"
        );
    }

    /// Write an executable mock kubectl into `dir` and return its path
    /// (same idiom as tests/command_backend_integration.rs).
    fn write_mock_kubectl(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("mock-kubectl");
        fs::write(&path, body).expect("write mock kubectl");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("make mock kubectl executable");
        path
    }

    /// Retry a mock-backed call a few times when exec fails with ETXTBSY
    /// ("Text file busy"). Under the parallel test harness, exec of a
    /// freshly-written mock can transiently race a still-open write handle
    /// from another test's fork/exec traffic; the condition clears once every
    /// straggler handle closes. Production exec paths deliberately do NOT get
    /// this treatment — they must surface real spawn errors loudly.
    fn with_exec_retry<T>(
        mut f: impl FnMut() -> Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        let mut attempt = 0;
        loop {
            match f() {
                Err(e) if attempt < 4 && e.reason.contains("Text file busy") => {
                    attempt += 1;
                    thread::sleep(Duration::from_millis(50 * attempt));
                }
                other => return other,
            }
        }
    }

    // --- the in-memory kubectl fake -----------------------------------------

    use std::sync::{Arc, Mutex};

    /// One recorded kubectl invocation: the subcommand argv (without
    /// connection flags) and the stdin payload submit piped in, if any.
    #[derive(Debug, Clone, PartialEq)]
    struct RecordedCall {
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
    }

    /// An in-memory [`KubectlRunner`] backing `submit()` unit tests: no
    /// process, no filesystem, no cluster. Serves scripted outcomes in call
    /// order (the final outcome repeats once the script runs out, so polling
    /// loops see a steady state instead of running past the script) and
    /// records every invocation into a shared log the test can read after
    /// the fake has been moved behind the backend.
    struct FakeKubectl {
        outcomes: Mutex<Vec<KubectlOutcome>>,
        calls: Arc<Mutex<Vec<RecordedCall>>>,
    }

    impl FakeKubectl {
        /// Build a fake serving `outcomes`, plus the handle to its call log.
        fn serving(outcomes: Vec<KubectlOutcome>) -> (Box<Self>, Arc<Mutex<Vec<RecordedCall>>>) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            (
                Box::new(FakeKubectl {
                    outcomes: Mutex::new(outcomes),
                    calls: Arc::clone(&calls),
                }),
                calls,
            )
        }
    }

    impl KubectlRunner for FakeKubectl {
        fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<KubectlOutcome, BackendError> {
            self.calls
                .lock()
                .expect("fake call log lock")
                .push(RecordedCall {
                    args: args.iter().map(|s| s.to_string()).collect(),
                    stdin: stdin.map(<[u8]>::to_vec),
                });
            let mut outcomes = self.outcomes.lock().expect("fake outcomes lock");
            if outcomes.is_empty() {
                panic!("fake kubectl ran past its scripted outcomes");
            }
            if outcomes.len() > 1 {
                Ok(outcomes.remove(0))
            } else {
                Ok(outcomes[0].clone())
            }
        }
    }

    /// Snapshot a fake's recorded calls for assertions.
    fn calls_of(calls: &Arc<Mutex<Vec<RecordedCall>>>) -> Vec<RecordedCall> {
        calls.lock().expect("fake call log lock").clone()
    }

    /// A successful kubectl outcome carrying `stdout`.
    fn ok_outcome(stdout: &str) -> KubectlOutcome {
        KubectlOutcome {
            code: Some(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    /// A failed kubectl outcome carrying `stderr`.
    fn failed_outcome(stderr: &str) -> KubectlOutcome {
        KubectlOutcome {
            code: Some(1),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// A backend around an injected fake runner with default config.
    fn backend_with_runner(kubectl: Box<dyn KubectlRunner>) -> ArgoBackend {
        ArgoBackend::with_runner(ArgoConfig::default(), kubectl)
    }

    /// The RunSpec every submit test submits.
    fn submit_spec() -> RunSpec {
        RunSpec::new(
            "cargo",
            "test",
            vec![],
            "https://github.com/example/repo",
            "abc123",
            "",
        )
    }

    /// A kubectl binary that cannot be spawned (nonexistent path) is a loud error.
    #[test]
    fn test_submit_kubectl_spawn_failure_is_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: tmp
                .path()
                .join("no-such-kubectl")
                .to_string_lossy()
                .into_owned(),
            ..ArgoConfig::default()
        });
        let spec = RunSpec::new(
            "cargo",
            "test",
            vec![],
            "https://github.com/example/repo",
            "abc123",
            "",
        );

        let err = backend
            .submit(&spec)
            .expect_err("submit must fail when kubectl cannot spawn");
        assert!(
            err.reason.contains("failed to spawn kubectl"),
            "{}",
            err.reason
        );
    }

    // --- submit() error paths through the in-memory fake --------------------
    //
    // The fake exercises the seam contract exactly — argv `create -f -` plus
    // the manifest on stdin — with no temp files, no exec, and therefore no
    // ETXTBSY retries.

    /// Happy path through the seam: submit pipes the manifest into
    /// `create -f -` and parses kubectl's
    /// `workflow.argoproj.io/<name> created` stdout into a RunHandle carrying
    /// the generated workflow name.
    #[test]
    fn submit_yields_run_handle_with_workflow_name() {
        let (fake, calls) = FakeKubectl::serving(vec![ok_outcome(
            "workflow.argoproj.io/gantry-abc123 created\n",
        )]);
        let backend = backend_with_runner(fake);

        let handle = backend.submit(&submit_spec()).expect("submit must succeed");
        assert_eq!(handle, RunHandle::new("gantry-abc123"));

        // Exactly one kubectl call, shaped `create -f -` with the manifest
        // on stdin (never spliced into argv).
        let log = calls_of(&calls);
        assert_eq!(log.len(), 1, "submit makes exactly one kubectl call");
        assert_eq!(log[0].args, vec!["create", "-f", "-"]);
        let manifest: serde_json::Value = serde_json::from_str(
            std::str::from_utf8(log[0].stdin.as_deref().expect("manifest on stdin"))
                .expect("manifest is utf-8"),
        )
        .expect("stdin payload is the Workflow manifest JSON");
        assert_eq!(manifest["metadata"]["generateName"], "gantry-");
    }

    /// kubectl create exiting non-zero surfaces kubectl's stderr in the
    /// BackendError — the API server's refusal is the diagnosable event, not
    /// a generic failure.
    #[test]
    fn submit_reports_nonzero_exit_with_stderr() {
        let (fake, calls) = FakeKubectl::serving(vec![failed_outcome(
            "Error from server (Forbidden): workflows is forbidden",
        )]);
        let backend = backend_with_runner(fake);

        let err = backend
            .submit(&submit_spec())
            .expect_err("non-zero kubectl exit must fail submit");
        assert!(
            err.reason.contains("workflow submission failed")
                && err.reason.contains("Error from server (Forbidden)"),
            "{}",
            err.reason
        );

        // The refusal came from the `create -f -` submission call.
        assert_eq!(calls_of(&calls)[0].args, vec!["create", "-f", "-"]);
    }

    /// stdout that does not carry the `workflow.argoproj.io/` prefix (e.g. an
    /// unexpected message) is a loud error, not a garbage handle.
    #[test]
    fn test_submit_rejects_stdout_without_workflow_prefix() {
        let (fake, _) = FakeKubectl::serving(vec![ok_outcome("error: unrecognized resource\n")]);
        let backend = backend_with_runner(fake);

        let err = backend
            .submit(&submit_spec())
            .expect_err("submit must fail on unrecognized stdout");
        assert!(
            err.reason.contains("missing workflow name"),
            "{}",
            err.reason
        );
    }

    /// A prefix with no name after the slash (`workflow.argoproj.io/` on its
    /// own) is a loud error, not a garbage handle.
    #[test]
    fn test_submit_rejects_bare_prefix_without_name() {
        let (fake, _) = FakeKubectl::serving(vec![ok_outcome("workflow.argoproj.io/\n")]);
        let backend = backend_with_runner(fake);

        let err = backend
            .submit(&submit_spec())
            .expect_err("submit must fail on a nameless workflow prefix");
        assert!(
            err.reason.contains("missing workflow name"),
            "{}",
            err.reason
        );
    }

    /// An empty name token followed by the status word (`workflow.argoproj.io/
    /// created`) is a loud error: taking the first whitespace token would
    /// otherwise return the status word itself as the handle.
    #[test]
    fn test_submit_rejects_empty_name_before_status_word() {
        let (fake, _) = FakeKubectl::serving(vec![ok_outcome("workflow.argoproj.io/ created\n")]);
        let backend = backend_with_runner(fake);

        let err = backend
            .submit(&submit_spec())
            .expect_err("submit must not return the status word as the handle");
        assert!(
            err.reason.contains("missing workflow name"),
            "{}",
            err.reason
        );
    }

    /// A runner that cannot execute (spawn failure) propagates its error to
    /// the caller unchanged — submit adds no masking layer on top.
    #[test]
    fn submit_propagates_runner_errors_verbatim() {
        struct FailingKubectl;
        impl KubectlRunner for FailingKubectl {
            fn run(
                &self,
                _args: &[&str],
                _stdin: Option<&[u8]>,
            ) -> Result<KubectlOutcome, BackendError> {
                Err(BackendError::new(
                    "failed to spawn kubectl: no such file or directory",
                ))
            }
        }
        let backend = backend_with_runner(Box::new(FailingKubectl));

        let err = backend
            .submit(&submit_spec())
            .expect_err("a failing runner must fail submit");
        assert_eq!(
            err.reason,
            "failed to spawn kubectl: no such file or directory"
        );
    }

    // --- submit() through the production runner (real spawn) ----------------
    //
    // The fake cannot cover what lives inside ProcessKubectl: actually
    // spawning a process, piping stdin into it, and tolerating a child that
    // exits before reading the manifest. A mock kubectl script exercises
    // those for real.

    /// A failing kubectl surfaces its stderr as the submit error, not a
    /// masked "failed to write workflow": the mock exits before reading
    /// stdin, so the manifest write hits a broken pipe and must fall through
    /// to the captured stderr.
    #[test]
    fn submit_tolerates_kubectl_exiting_before_reading_stdin() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let kubectl = write_mock_kubectl(
            tmp.path(),
            "#!/usr/bin/env bash\necho 'mock kubectl exploded' >&2\nexit 1\n",
        );
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
            ..ArgoConfig::default()
        });

        let err = with_exec_retry(|| backend.submit(&submit_spec()))
            .expect_err("submit must fail when kubectl exits non-zero");
        assert!(
            err.reason.contains("workflow submission failed")
                && err.reason.contains("mock kubectl exploded"),
            "{}",
            err.reason
        );
    }

    /// The happy path: submit pipes the manifest to kubectl's stdin, parses
    /// `workflow.argoproj.io/<name> created` back out as the bare handle, and
    /// every template parameter (repo, revision, args-json, builder-image) is
    /// populated from the RunSpec and config.
    #[test]
    fn test_submit_passes_manifest_and_returns_workflow_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stdin_capture = tmp.path().join("stdin.json");
        let kubectl = write_mock_kubectl(
            tmp.path(),
            &format!(
                "#!/usr/bin/env bash\ncat > {}\necho 'workflow.argoproj.io/gantry-abc123 created'\n",
                stdin_capture.display()
            ),
        );
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
            builder_image: Some("rust:1.83".to_string()),
            ..ArgoConfig::default()
        });
        let spec = RunSpec::new(
            "cargo",
            "test",
            vec!["--nocapture".to_string()],
            "https://github.com/example/repo",
            "abc123",
            "",
        );

        let handle = with_exec_retry(|| backend.submit(&spec)).expect("submit must succeed");
        assert_eq!(handle.handle, "gantry-abc123");

        // The mock received the manifest on stdin: verify the parameter contract.
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&stdin_capture).expect("mock captured stdin"),
        )
        .expect("captured manifest must be valid JSON");
        let params = manifest["spec"]["arguments"]["parameters"]
            .as_array()
            .expect("parameters array");
        let value_of = |name: &str| {
            params
                .iter()
                .find(|p| p["name"] == name)
                .expect("parameter present")["value"]
                .as_str()
                .unwrap()
        };
        assert_eq!(value_of("repo"), "https://github.com/example/repo");
        assert_eq!(value_of("revision"), "abc123");
        assert_eq!(value_of("args-json"), r#"["--nocapture"]"#);
        // The handshake rides every submission: the template echoes this back
        // in verdict.json and the client reads a different echo as drift.
        assert_eq!(value_of("contract-version"), CONTRACT_VERSION);
        assert_eq!(value_of("builder-image"), "rust:1.83");
    }

    /// wait() reads status.phase from the nested `status` stanza of the real
    /// kubectl Workflow object and prefers the verdict output parameter.
    #[test]
    fn test_wait_parses_nested_status_and_verdict_output() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workflow_json = serde_json::json!({
            "apiVersion": "argoproj.io/v1alpha1",
            "kind": "Workflow",
            "metadata": {
                "name": "gantry-abc123",
                "generateName": "gantry-"
            },
            "spec": { "entrypoint": "gantry-verify" },
            "status": {
                "phase": "Succeeded",
                "startedAt": "2026-09-23T17:00:00Z",
                "finishedAt": "2026-09-23T17:05:00Z",
                "outputs": {
                    "parameters": [
                        {
                            "name": "verdict",
                            "value": r#"{"schema_version": 1, "phase": "Succeeded", "exit_code": 0, "oom": false, "deadline_exceeded": false}"#
                        }
                    ]
                },
                "nodes": {
                    "gantry-abc123": {
                        "id": "gantry-abc123",
                        "name": "gantry-abc123",
                        "displayName": "gantry-abc123",
                        "type": "DAG",
                        "phase": "Succeeded"
                    }
                }
            }
        });
        let kubectl = write_mock_kubectl(
            tmp.path(),
            &format!(
                "#!/usr/bin/env bash\ncat <<'JSON'\n{}\nJSON\n",
                workflow_json
            ),
        );
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
            ..ArgoConfig::default()
        });
        let handle = RunHandle::new("gantry-abc123");

        let verdict =
            with_exec_retry(|| backend.wait(&handle, Instant::now() + Duration::from_secs(5)))
                .expect("wait must return on terminal phase");
        assert_eq!(verdict, Verdict::Pass);
    }

    /// A workflow the controller has not reconciled yet has no status stanza
    /// (or a phase-less one) — that is pending, not a parse error: the next
    /// poll sees the terminal phase and returns the verdict.
    #[test]
    fn test_wait_treats_missing_or_empty_status_as_pending() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let counter = tmp.path().join("calls");
        let kubectl = write_mock_kubectl(
            tmp.path(),
            &format!(
                "#!/usr/bin/env bash\n\
                 n=$(($(cat {}) + 1))\n\
                 echo $n > {}\n\
                 if [ $n -eq 1 ]; then\n\
                   echo '{{\"apiVersion\":\"argoproj.io/v1alpha1\",\"kind\":\"Workflow\",\"metadata\":{{\"name\":\"gantry-abc123\"}}}}'\n\
                 elif [ $n -eq 2 ]; then\n\
                   echo '{{\"status\":{{}}}}'\n\
                 else\n\
                   echo '{{\"status\":{{\"phase\":\"Failed\"}}}}'\n\
                 fi\n",
                counter.display(),
                counter.display()
            ),
        );
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
            ..ArgoConfig::default()
        });
        let handle = RunHandle::new("gantry-abc123");

        let verdict =
            with_exec_retry(|| backend.wait(&handle, Instant::now() + Duration::from_secs(30)))
                .expect("wait must survive pending polls");
        assert_eq!(verdict, Verdict::TestFailure);
    }

    /// The `Error` phase (e.g. the controller could not resolve the template)
    /// is terminal: wait() stops polling and classifies it as InfraFailure —
    /// the workflow itself broke, so no test result exists to report.
    #[test]
    fn test_wait_error_phase_is_terminal_infra_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let kubectl = write_mock_kubectl(
            tmp.path(),
            "#!/usr/bin/env bash\n\
             echo '{\"status\":{\"phase\":\"Error\",\"message\":\"template not found\"}}'\n",
        );
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
            ..ArgoConfig::default()
        });
        let handle = RunHandle::new("gantry-abc123");

        let verdict =
            with_exec_retry(|| backend.wait(&handle, Instant::now() + Duration::from_secs(5)))
                .expect("wait must return on the terminal Error phase");
        assert_eq!(verdict, Verdict::InfraFailure);
        assert!(verdict.is_infra_failure());
    }

    /// An already-passed deadline errors out immediately — before any kubectl
    /// call — instead of polling forever.
    #[test]
    fn test_wait_deadline_exceeded_is_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let kubectl = write_mock_kubectl(tmp.path(), "#!/usr/bin/env bash\nexit 1\n");
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
            ..ArgoConfig::default()
        });
        let handle = RunHandle::new("gantry-abc123");

        let err = backend
            .wait(&handle, Instant::now() - Duration::from_secs(1))
            .expect_err("wait must fail once the deadline has passed");
        assert!(err.reason.contains("deadline"), "{}", err.reason);
    }

    /// An OOM verdict.json output classifies the run as InfraFailure even
    /// though the workflow phase is Failed.
    #[test]
    fn test_wait_oom_verdict_output_is_infra_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let workflow_json = serde_json::json!({
            "status": {
                "phase": "Failed",
                "outputs": {
                    "parameters": [
                        {
                            "name": "verdict",
                            "value": r#"{"schema_version": 1, "phase": "Failed", "exit_code": 137, "oom": true, "deadline_exceeded": false}"#
                        }
                    ]
                }
            }
        });
        let kubectl = write_mock_kubectl(
            tmp.path(),
            &format!(
                "#!/usr/bin/env bash\ncat <<'JSON'\n{}\nJSON\n",
                workflow_json
            ),
        );
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
            ..ArgoConfig::default()
        });
        let handle = RunHandle::new("gantry-abc123");

        let verdict =
            with_exec_retry(|| backend.wait(&handle, Instant::now() + Duration::from_secs(5)))
                .expect("wait must return on terminal phase");
        assert_eq!(verdict, Verdict::InfraFailure);
    }

    /// A verdict.json echoing a foreign contract_version is contract drift:
    /// wait() classifies it InfraFailure no matter what the rest of the
    /// document claims — here a *passing* document (Succeeded, exit 0) flips
    /// to infra, the "never a misread verdict" half of the plan's handshake
    /// rule. The explicit drift message itself is stderr surface (the
    /// parse-level drift semantics are pinned in src/verdict.rs tests).
    #[test]
    fn test_wait_contract_drift_echo_is_infra_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let drifted = status_json_with_verdict(
            "Succeeded",
            r#"{"schema_version":1,"phase":"Succeeded","exit_code":0,"oom":false,"deadline_exceeded":false,"contract_version":"9"}"#,
        );
        let kubectl = write_mock_kubectl(
            tmp.path(),
            &format!("#!/usr/bin/env bash\ncat <<'JSON'\n{}\nJSON\n", drifted),
        );
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
            ..ArgoConfig::default()
        });
        let handle = RunHandle::new("gantry-abc123");

        let verdict =
            with_exec_retry(|| backend.wait(&handle, Instant::now() + Duration::from_secs(5)))
                .expect("wait must return on terminal phase");
        assert_eq!(verdict, Verdict::InfraFailure);
        assert!(verdict.is_infra_failure());
    }

    // --- the status.phase ladder --------------------------------------------
    //
    // These go through the in-memory fake: polling loops run many kubectl
    // calls, and the fake scripts them in call order with no temp files and
    // no ETXTBSY retries. with_runner's fast poll intervals keep multi-rung
    // scenarios in milliseconds.

    /// A workflow object carrying only `status.phase`.
    fn status_json(phase: &str) -> String {
        serde_json::json!({ "status": { "phase": phase } }).to_string()
    }

    /// A workflow object carrying `status.phase` and a `verdict` output
    /// parameter (the verdict.json the remote template exports).
    fn status_json_with_verdict(phase: &str, verdict: &str) -> String {
        serde_json::json!({
            "status": {
                "phase": phase,
                "outputs": {
                    "parameters": [ { "name": "verdict", "value": verdict } ]
                }
            }
        })
        .to_string()
    }

    /// The schema-1 verdict.json for a passing run.
    fn passing_verdict_json() -> String {
        r#"{"schema_version":1,"phase":"Succeeded","exit_code":0,"oom":false,"deadline_exceeded":false}"#
            .to_string()
    }

    /// The full phase ladder parses rung by rung: the two phase-less pending
    /// shapes read as no-phase-yet, Pending and Running are known-pending,
    /// Succeeded/Failed/Error are known-terminal, and anything else is a
    /// loud error rather than a silent "keep waiting".
    #[test]
    fn workflow_phase_ladder_classifies_every_status_value() {
        // Phase-less pending shapes.
        assert_eq!(WorkflowPhase::parse(None), Ok(None));
        assert_eq!(WorkflowPhase::parse(Some("")), Ok(None));
        // Known-pending rungs.
        assert_eq!(
            WorkflowPhase::parse(Some("Pending")),
            Ok(Some(WorkflowPhase::Pending))
        );
        assert_eq!(
            WorkflowPhase::parse(Some("Running")),
            Ok(Some(WorkflowPhase::Running))
        );
        // Known-terminal rungs.
        assert_eq!(
            WorkflowPhase::parse(Some("Succeeded")),
            Ok(Some(WorkflowPhase::Succeeded))
        );
        assert_eq!(
            WorkflowPhase::parse(Some("Failed")),
            Ok(Some(WorkflowPhase::Failed))
        );
        assert_eq!(
            WorkflowPhase::parse(Some("Error")),
            Ok(Some(WorkflowPhase::Error))
        );

        // Terminality splits exactly at the three terminal rungs.
        assert!(!WorkflowPhase::Pending.is_terminal());
        assert!(!WorkflowPhase::Running.is_terminal());
        assert!(WorkflowPhase::Succeeded.is_terminal());
        assert!(WorkflowPhase::Failed.is_terminal());
        assert!(WorkflowPhase::Error.is_terminal());

        // An unrecognized phase string names itself in the error — it must
        // never classify as pending (that would poll forever).
        let err = WorkflowPhase::parse(Some("Zombie")).expect_err("unknown phase must error");
        assert!(
            err.reason.contains("unknown workflow status phase") && err.reason.contains("Zombie"),
            "{}",
            err.reason
        );
    }

    /// Phase-only fallback verdicts: Succeeded is a Pass, Failed is a
    /// TestFailure, and Error is an InfraFailure (the workflow itself broke,
    /// so no test result exists). The pending rungs classify as infra too,
    /// should one ever reach this terminal fallback.
    #[test]
    fn workflow_phase_fallback_verdicts_map_terminal_rungs() {
        assert_eq!(WorkflowPhase::Succeeded.fallback_verdict(), Verdict::Pass);
        assert_eq!(
            WorkflowPhase::Failed.fallback_verdict(),
            Verdict::TestFailure
        );
        let error_verdict = WorkflowPhase::Error.fallback_verdict();
        assert_eq!(error_verdict, Verdict::InfraFailure);
        assert!(error_verdict.is_infra_failure());
        assert_eq!(
            WorkflowPhase::Pending.fallback_verdict(),
            Verdict::InfraFailure
        );
        assert_eq!(
            WorkflowPhase::Running.fallback_verdict(),
            Verdict::InfraFailure
        );
    }

    /// The full pending ladder: a workflow that reports Pending, then
    /// Running, then Succeeded with a verdict.json walks every pending rung
    /// and lands on Pass — one kubectl poll per rung.
    #[test]
    fn wait_walks_pending_running_succeeded_to_pass() {
        let (fake, calls) = FakeKubectl::serving(vec![
            ok_outcome(&status_json("Pending")),
            ok_outcome(&status_json("Running")),
            ok_outcome(&status_json_with_verdict(
                "Succeeded",
                &passing_verdict_json(),
            )),
        ]);
        let backend = backend_with_runner(fake);
        let handle = RunHandle::new("gantry-abc123");

        let verdict = backend
            .wait(&handle, Instant::now() + Duration::from_secs(30))
            .expect("wait must walk the ladder to a verdict");
        assert_eq!(verdict, Verdict::Pass);

        // One `get workflow -o json` poll per observed rung.
        let log = calls_of(&calls);
        assert_eq!(log.len(), 3, "one poll per rung, got {:?}", log.len());
        for call in &log {
            assert_eq!(
                call.args,
                vec!["get", "workflow", "gantry-abc123", "-o", "json"]
            );
        }
    }

    /// A terminal Failed phase with no `verdict` output parameter falls back
    /// to the phase ladder: TestFailure — tests ran and failed.
    #[test]
    fn wait_failed_phase_without_verdict_param_is_test_failure() {
        let (fake, _) = FakeKubectl::serving(vec![ok_outcome(&status_json("Failed"))]);
        let backend = backend_with_runner(fake);
        let handle = RunHandle::new("gantry-abc123");

        let verdict = backend
            .wait(&handle, Instant::now() + Duration::from_secs(30))
            .expect("wait must classify the terminal Failed phase");
        assert_eq!(verdict, Verdict::TestFailure);
    }

    /// A malformed `verdict` output parameter degrades through the shared
    /// exit-code-only path — the typed parse error is not a wait error, and
    /// the terminal phase classifies the run (Succeeded ⇒ Pass).
    #[test]
    fn wait_malformed_verdict_param_degrades_to_exit_code_only() {
        let (fake, _) = FakeKubectl::serving(vec![ok_outcome(&status_json_with_verdict(
            "Succeeded",
            "{not json",
        ))]);
        let backend = backend_with_runner(fake);
        let handle = RunHandle::new("gantry-abc123");

        let verdict = backend
            .wait(&handle, Instant::now() + Duration::from_secs(30))
            .expect("malformed verdict.json must degrade, not fail the wait");
        assert_eq!(verdict, Verdict::Pass);
    }

    /// A `verdict` output parameter carrying a schema_version this parser
    /// does not know degrades through the same path: typed BackendError from
    /// parse, then the phase classifies — the unusable document's own exit
    /// code must never leak into the verdict.
    #[test]
    fn wait_unsupported_schema_version_degrades_to_exit_code_only() {
        // Claims exit 1 — but a document this parser rejects is not
        // interpreted at all: the Succeeded phase decides, and that is Pass.
        let future = r#"{"schema_version": 3, "phase": "Succeeded", "exit_code": 1}"#;
        let (fake, _) = FakeKubectl::serving(vec![ok_outcome(&status_json_with_verdict(
            "Succeeded",
            future,
        ))]);
        let backend = backend_with_runner(fake);
        let handle = RunHandle::new("gantry-abc123");

        let verdict = backend
            .wait(&handle, Instant::now() + Duration::from_secs(30))
            .expect("unsupported schema must degrade, not fail the wait");
        assert_eq!(verdict, Verdict::Pass);
    }

    /// The degradation matrix at the wait() level: for every shape of "no
    /// usable verdict.json" (parameter absent, malformed JSON, empty string,
    /// unsupported schema_version) the terminal phase alone decides —
    /// Succeeded → Pass, Failed → TestFailure, Error → InfraFailure. All
    /// three shapes funnel through one shared path
    /// ([`VerdictJson::from_exit_code`]).
    #[test]
    fn wait_degradation_shapes_all_land_on_the_phase_ladder() {
        let shapes: Vec<Option<String>> = vec![
            None,
            Some("{not json".to_string()),
            Some(String::new()),
            Some(r#"{"schema_version": 3, "phase": "Succeeded", "exit_code": 1}"#.to_string()),
        ];
        for shape in &shapes {
            let status_for = |terminal: &str| match shape {
                None => status_json(terminal),
                Some(value) => status_json_with_verdict(terminal, value),
            };
            let wait = |terminal: &str| -> Verdict {
                let (fake, _) = FakeKubectl::serving(vec![ok_outcome(&status_for(terminal))]);
                backend_with_runner(fake)
                    .wait(
                        &RunHandle::new("gantry-abc123"),
                        Instant::now() + Duration::from_secs(30),
                    )
                    .expect("degradation must return a verdict, not an error")
            };
            assert_eq!(wait("Succeeded"), Verdict::Pass, "shape {shape:?}");
            assert_eq!(wait("Failed"), Verdict::TestFailure, "shape {shape:?}");
            assert_eq!(wait("Error"), Verdict::InfraFailure, "shape {shape:?}");
        }
    }

    /// Deadline expiry during polling: a workflow that stays Running forever
    /// must surface a loud deadline error at the deadline — not loop
    /// past it. The poll interval is set far larger than the remaining
    /// budget, so a correct implementation clamps the pending sleep to the
    /// deadline and returns immediately after the first poll; an
    /// implementation that sleeps the full interval first would blow the
    /// elapsed bound.
    #[test]
    fn wait_deadline_exceeded_during_polling_errors_not_loops() {
        // The final outcome repeats forever: the workflow never completes.
        let (fake, calls) = FakeKubectl::serving(vec![ok_outcome(&status_json("Running"))]);
        let mut backend = backend_with_runner(fake);
        backend.status_poll = Duration::from_secs(10);
        let handle = RunHandle::new("gantry-abc123");

        let start = Instant::now();
        let err = backend
            .wait(&handle, start + Duration::from_millis(150))
            .expect_err("wait must fail once the deadline passes mid-poll");
        let elapsed = start.elapsed();

        assert!(err.reason.contains("deadline"), "{}", err.reason);
        // The clamp: the pending sleep was cut to the ~150ms remaining, so
        // the full 10s interval never ran.
        assert!(
            elapsed < Duration::from_secs(5),
            "wait slept past its deadline: {:?}",
            elapsed
        );
        // Exactly one poll happened: the clamped sleep lands at (or past)
        // the deadline, so the loop errors before a second kubectl call.
        assert_eq!(calls_of(&calls).len(), 1, "wait polled past its deadline");
    }

    /// The expiry error is the structured deadline expiry AND it names the
    /// run URL: the watch is abandoned, the workflow keeps running on the
    /// cluster, and the operator needs describe() to find it (features.md
    /// v1.x "a clear timed-out, here's-the-run-URL message").
    #[test]
    fn wait_expiry_error_is_structured_and_carries_the_run_url() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let kubectl = write_mock_kubectl(tmp.path(), "#!/usr/bin/env bash\nexit 1\n");
        let base = "https://argo.example.com";
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
            base_url: Some(base.to_string()),
            ..ArgoConfig::default()
        });
        let handle = RunHandle::new("gantry-abc123");

        let err = backend
            .wait(&handle, Instant::now() - Duration::from_secs(1))
            .expect_err("wait must fail once the deadline has passed");
        assert!(
            err.deadline_exceeded,
            "expiry must be the structured deadline error, got: {}",
            err.reason
        );
        let expected_url = format!(
            "{}/workflows/{}/gantry-abc123",
            base, backend.config.namespace
        );
        assert_eq!(
            err.run_url.as_deref(),
            Some(expected_url.as_str()),
            "the expiry error must carry describe()'s run URL"
        );
    }

    /// A run that never reaches a terminal phase stops being watched at the
    /// deadline: the structured expiry (with the run URL) comes out, and no
    /// poll happens past the deadline — the watch loop cannot hang forever
    /// on a workflow that never finishes.
    #[test]
    fn wait_midpoll_expiry_stops_watching_and_names_the_run_url() {
        let (fake, _calls) = FakeKubectl::serving(vec![ok_outcome(&status_json("Running"))]);
        let base = "https://argo.example.com";
        let backend = ArgoBackend::with_runner(
            ArgoConfig {
                base_url: Some(base.to_string()),
                ..ArgoConfig::default()
            },
            fake,
        );
        let handle = RunHandle::new("gantry-abc123");

        let err = backend
            .wait(&handle, Instant::now() + Duration::from_millis(150))
            .expect_err("a never-finishing run must hit the deadline");
        assert!(
            err.deadline_exceeded,
            "mid-poll expiry must be the structured deadline error, got: {}",
            err.reason
        );
        assert!(
            err.run_url
                .as_deref()
                .is_some_and(|u| u.contains("/workflows/") && u.ends_with("/gantry-abc123")),
            "the expiry must carry describe()'s run URL, got: {:?}",
            err.run_url
        );
    }

    /// Without a configured base_url, describe() degrades to the bare
    /// `workflow/<name>` identifier — the timeout line still points the
    /// operator at something findable (`kubectl get workflow <name>`),
    /// never at nothing.
    #[test]
    fn wait_expiry_without_base_url_falls_back_to_the_workflow_identifier() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let kubectl = write_mock_kubectl(tmp.path(), "#!/usr/bin/env bash\nexit 1\n");
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
            ..ArgoConfig::default()
        });
        let handle = RunHandle::new("gantry-abc123");

        let err = backend
            .wait(&handle, Instant::now() - Duration::from_secs(1))
            .expect_err("wait must fail once the deadline has passed");
        assert!(err.deadline_exceeded);
        assert_eq!(
            err.run_url.as_deref(),
            Some("workflow/gantry-abc123"),
            "no base_url must degrade to the workflow identifier"
        );
    }

    /// An unrecognized phase string is a loud error naming the value — not a
    /// silent retry that polls until the deadline.
    #[test]
    fn wait_unknown_phase_is_loud_error_not_silent_retry() {
        let (fake, calls) = FakeKubectl::serving(vec![ok_outcome(&status_json("Zombie"))]);
        let backend = backend_with_runner(fake);
        let handle = RunHandle::new("gantry-abc123");

        // The deadline is far away: the error must come from the ladder, not
        // from expiry.
        let err = backend
            .wait(&handle, Instant::now() + Duration::from_secs(30))
            .expect_err("an unknown phase must error instead of polling");
        assert!(
            err.reason.contains("unknown workflow status phase") && err.reason.contains("Zombie"),
            "{}",
            err.reason
        );
        assert_eq!(
            calls_of(&calls).len(),
            1,
            "wait must not retry after an unknown phase"
        );
    }

    /// Malformed JSON from `kubectl get workflow -o json` is a loud error on
    /// the first occurrence — never a silent retry (gantry never guesses at
    /// a status it cannot parse).
    #[test]
    fn wait_malformed_workflow_json_is_error_not_retry() {
        let (fake, calls) =
            FakeKubectl::serving(vec![ok_outcome(r#"{"status":{"phase":"Succeeded""#)]);
        let backend = backend_with_runner(fake);
        let handle = RunHandle::new("gantry-abc123");

        let err = backend
            .wait(&handle, Instant::now() + Duration::from_secs(30))
            .expect_err("malformed workflow JSON must error instead of retrying");
        assert!(
            err.reason.contains("failed to parse workflow status"),
            "{}",
            err.reason
        );
        assert_eq!(
            calls_of(&calls).len(),
            1,
            "wait must not retry after a malformed status document"
        );
    }

    // --- status() ------------------------------------------------------------

    /// The known phase ladder maps onto the coarse RunStatus rung for rung:
    /// Pending and Running are themselves, and all three terminal rungs are
    /// Completed (which verdict follows is wait()'s job, not status's).
    #[test]
    fn status_maps_known_phase_ladder_onto_run_status() {
        let cases = [
            ("Pending", RunStatus::Pending),
            ("Running", RunStatus::Running),
            ("Succeeded", RunStatus::Completed),
            ("Failed", RunStatus::Completed),
            ("Error", RunStatus::Completed),
        ];
        for (phase, expected) in cases {
            let (fake, _) = FakeKubectl::serving(vec![ok_outcome(&status_json(phase))]);
            let backend = backend_with_runner(fake);

            let status = backend
                .status(&RunHandle::new("gantry-abc123"))
                .unwrap_or_else(|e| panic!("known phase {phase} must answer, not error: {e}"));
            assert_eq!(status, expected, "phase {phase}");
        }
    }

    /// The phase-less pending shapes (no `status` stanza at all, or a bare
    /// one the controller has not filled in) give a point-in-time query
    /// nothing to read: Unknown — not an error, and not Pending (status has
    /// no pending/serving distinction to defend the way wait() does).
    #[test]
    fn status_maps_missing_phase_shapes_to_unknown() {
        for body in [r#"{}"#, r#"{"status":{}}"#] {
            let (fake, _) = FakeKubectl::serving(vec![ok_outcome(body)]);
            let backend = backend_with_runner(fake);

            let status = backend
                .status(&RunHandle::new("gantry-abc123"))
                .expect("a missing phase must degrade, never error");
            assert_eq!(status, RunStatus::Unknown, "body {body}");
        }
    }

    /// An unrecognized phase string is Unknown, not an error — the deliberate
    /// contrast with wait(), where the same string errors loudly (a waiter
    /// cannot afford to poll forever; a status poller just retries). The
    /// answer comes from the same single `get workflow -o json` poll the
    /// verdict path uses, with no retry.
    #[test]
    fn status_maps_unknown_phase_to_unknown_not_error() {
        let (fake, calls) = FakeKubectl::serving(vec![ok_outcome(&status_json("Zombie"))]);
        let backend = backend_with_runner(fake);
        let handle = RunHandle::new("gantry-abc123");

        let status = backend
            .status(&handle)
            .expect("an unknown phase must degrade to Unknown, not error");
        assert_eq!(status, RunStatus::Unknown);

        let log = calls_of(&calls);
        assert_eq!(log.len(), 1, "status must not retry");
        assert_eq!(
            log[0].args,
            vec!["get", "workflow", "gantry-abc123", "-o", "json"]
        );
    }

    /// Every unanswerable query degrades to Unknown rather than erroring:
    /// kubectl exiting non-zero (transient API failure, RBAC, the workflow
    /// not created yet), a runner that cannot spawn, and a malformed status
    /// document. A status snapshot that cannot be taken is "no news".
    #[test]
    fn status_maps_unanswerable_queries_to_unknown() {
        // kubectl exits non-zero.
        let (fake, _) = FakeKubectl::serving(vec![failed_outcome("Error from server: timeout")]);
        let status = backend_with_runner(fake)
            .status(&RunHandle::new("gantry-abc123"))
            .expect("a failed query must degrade to Unknown, not error");
        assert_eq!(status, RunStatus::Unknown);

        // The runner itself cannot execute.
        struct FailingKubectl;
        impl KubectlRunner for FailingKubectl {
            fn run(
                &self,
                _args: &[&str],
                _stdin: Option<&[u8]>,
            ) -> Result<KubectlOutcome, BackendError> {
                Err(BackendError::new(
                    "failed to spawn kubectl: no such file or directory",
                ))
            }
        }
        let status = backend_with_runner(Box::new(FailingKubectl))
            .status(&RunHandle::new("gantry-abc123"))
            .expect("a spawn failure must degrade to Unknown, not error");
        assert_eq!(status, RunStatus::Unknown);

        // A malformed status document.
        let (fake, _) = FakeKubectl::serving(vec![ok_outcome(r#"{"status":{"phase":"Succeeded"#)]);
        let status = backend_with_runner(fake)
            .status(&RunHandle::new("gantry-abc123"))
            .expect("a malformed document must degrade to Unknown, not error");
        assert_eq!(status, RunStatus::Unknown);
    }

    /// stream_logs discovers the workflow's pod and pipes its logs to the
    /// writer.
    ///
    /// The mock dispatches on the whole argv, not $1/$2: ArgoBackend prepends
    /// the global `-n <namespace>` flag before the subcommand, so positional
    /// checks never see `get`/`logs` (they see `-n`).
    #[test]
    fn test_stream_logs_emits_pod_logs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let kubectl = write_mock_kubectl(
            tmp.path(),
            "#!/usr/bin/env bash\n\
             case \" $* \" in\n\
               *' pods '*) echo '{\"items\":[{\"metadata\":{\"name\":\"gantry-abc123-1234567890\"}}]}' ;;\n\
               *' logs '*) printf 'running 137 tests\\nall passed\\n' ;;\n\
               *) exit 1 ;;\n\
             esac\n",
        );
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
            ..ArgoConfig::default()
        });
        let handle = RunHandle::new("gantry-abc123");

        let mut out: Vec<u8> = Vec::new();
        with_exec_retry(|| backend.stream_logs(&handle, &mut out))
            .expect("stream_logs must succeed against the mock");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "running 137 tests\nall passed\n"
        );
    }

    /// Pod discovery gives up (loud error) once the timeout expires instead
    /// of spinning forever on a workflow that never schedules. The mock's
    /// `get workflow` fails, so the workflow never goes terminal and the
    /// podGone shortcut cannot fire — only the timeout can end the wait.
    #[test]
    fn test_pod_discovery_gives_up_after_timeout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let kubectl = write_mock_kubectl(
            tmp.path(),
            "#!/usr/bin/env bash\n\
             case \" $* \" in\n\
               *' pods '*) echo '{\"items\":[]}' ;;\n\
               *) exit 1 ;;\n\
             esac\n",
        );
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
            ..ArgoConfig::default()
        });
        let handle = RunHandle::new("gantry-abc123");

        let err =
            with_exec_retry(|| backend.discover_pod_or_terminal(&handle.handle, Duration::ZERO))
                .expect_err("discovery must give up after the timeout");
        assert!(
            err.reason.contains("no pod found for workflow"),
            "{}",
            err.reason
        );
    }

    #[test]
    fn test_verdict_full_ladder_coverage() {
        // Test all four main verdict variants from verdict.json

        // Pass
        let pass_json = r#"{
            "schema_version": 1,
            "phase": "Succeeded",
            "exit_code": 0,
            "oom": false,
            "deadline_exceeded": false
        }"#;
        assert_eq!(
            VerdictJson::parse(pass_json).unwrap().to_verdict(),
            Verdict::Pass
        );

        // TestFailure
        let test_json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "oom": false,
            "deadline_exceeded": false,
            "failure_class": "test-failure"
        }"#;
        assert_eq!(
            VerdictJson::parse(test_json).unwrap().to_verdict(),
            Verdict::TestFailure
        );

        // GateFailure
        let gate_json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "oom": false,
            "deadline_exceeded": false,
            "failure_class": "gate-failure"
        }"#;
        assert_eq!(
            VerdictJson::parse(gate_json).unwrap().to_verdict(),
            Verdict::GateFailure
        );

        // InfraFailure (OOM)
        let oom_json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "oom": true,
            "deadline_exceeded": false
        }"#;
        assert_eq!(
            VerdictJson::parse(oom_json).unwrap().to_verdict(),
            Verdict::InfraFailure
        );

        // InfraFailure (deadline)
        let deadline_json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "oom": false,
            "deadline_exceeded": true
        }"#;
        assert_eq!(
            VerdictJson::parse(deadline_json).unwrap().to_verdict(),
            Verdict::InfraFailure
        );
    }
}
