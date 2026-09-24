// gantry — Argo Workflows backend (plan §"Phase 1a", Components §5 "RemoteBackend trait").
//
// Phase 1a: builds Workflow manifests with serde (no string splicing, S-4), submits via
// kubectl, streams pod logs, polls status.phase for verdict, falls back to output parameters
// when podGC ate the logs.
//
// The Argo backend implements RemoteBackend using kubectl as the execution layer:
// - submit: builds Workflow manifest with serde, runs kubectl create -f -
// - stream_logs: kubectl logs -f on the workflow's pod
// - wait: polls kubectl get workflow status.phase until terminal or deadline
// - describe: returns the workflow name/UI URL
// - cancel: kubectl delete workflow

use crate::backend::{BackendError, RemoteBackend, RunSpec, Verdict, VerdictJson};
use std::io::Write;
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
/// template contract: parameters (repo, revision, args-json, builder-image),
/// generateName, entrypoint, and a workflowTemplateRef to the cluster's
/// WorkflowTemplate. Workflow-level arguments are merged with the template's
/// arguments (argo-workflows docs §"Workflow Templates"): names the workflow
/// supplies take effect; names it omits keep the template's default — which is
/// how an unconfigured builder-image falls back to the template default.
mod workflow {
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

    impl Workflow {
        /// Create a new Workflow manifest for gantry.
        ///
        /// Parameters follow the gantry-verify template contract (plan §argo):
        /// repo, revision, args-json always; builder-image only when configured
        /// (omitting it lets the WorkflowTemplate default apply).
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

/// The Argo Workflows backend implementation.
pub struct ArgoBackend {
    /// Argo-specific configuration.
    config: ArgoConfig,
}

impl ArgoBackend {
    /// Create a new ArgoBackend with configuration.
    pub fn new(config: ArgoConfig) -> Self {
        ArgoBackend { config }
    }

    /// Create a new ArgoBackend with default config.
    pub fn default_config() -> Self {
        ArgoBackend {
            config: ArgoConfig::default(),
        }
    }

    /// Run kubectl with arguments and return output.
    fn kubectl(&self, args: &[&str]) -> Result<Output, BackendError> {
        let mut cmd = Command::new(&self.config.kubectl_path);

        // Add kubeconfig flag if set
        if !self.config.kubeconfig.is_empty() {
            cmd.arg("--kubeconfig").arg(&self.config.kubeconfig);
        }

        // Add namespace flag
        cmd.arg("-n").arg(&self.config.namespace);

        // Add the arguments
        cmd.args(args);

        cmd.output()
            .map_err(|e| BackendError::new(&format!("failed to run kubectl: {}", e)))
    }

    /// Discover the pod name for a workflow by listing pods, retrying until
    /// `timeout` elapses. Best-effort streaming must not hang forever on a
    /// workflow that never schedules, so expiry is a loud error, not a hang.
    fn discover_pod_with_retry(
        &self,
        workflow_name: &str,
        timeout: Duration,
    ) -> Result<String, BackendError> {
        let start = Instant::now();
        loop {
            match self.discover_pod(workflow_name)? {
                Some(pod) => return Ok(pod),
                None => {
                    if start.elapsed() >= timeout {
                        return Err(BackendError::new(&format!(
                            "no pod found for workflow {} within {:?}",
                            workflow_name, timeout
                        )));
                    }
                    thread::sleep(POD_DISCOVERY_POLL);
                }
            }
        }
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

        if !output.status.success() {
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
    /// Builds the Workflow manifest with serde and runs kubectl create -f -.
    /// The workflow name (stdout) becomes the handle.
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

        // Submit via kubectl create -f -
        let mut cmd = Command::new(&self.config.kubectl_path);

        // Add kubeconfig flag if set
        if !self.config.kubeconfig.is_empty() {
            cmd.arg("--kubeconfig").arg(&self.config.kubeconfig);
        }

        // Add namespace flag
        cmd.arg("-n").arg(&self.config.namespace);
        cmd.args(["create", "-f", "-"]);

        // Spawn kubectl with stdin piped
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| BackendError::new(&format!("failed to spawn kubectl: {}", e)))?;

        // Write the manifest to kubectl's stdin. A broken pipe means kubectl
        // exited before reading it (e.g. the manifest was rejected client-side)
        // — fall through so wait_with_output surfaces kubectl's stderr as the
        // real error instead of masking it.
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(workflow_json.as_bytes()) {
                if e.kind() != std::io::ErrorKind::BrokenPipe {
                    return Err(BackendError::new(&format!(
                        "failed to write workflow: {}",
                        e
                    )));
                }
            }
            // Drop stdin to signal EOF, then wait for kubectl to finish.
        }

        // Wait for completion and get output
        let output = child
            .wait_with_output()
            .map_err(|e| BackendError::new(&format!("failed to wait for kubectl: {}", e)))?;

        if !output.status.success() {
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
    /// Discovers the pod name, then streams kubectl logs -f.
    /// Best-effort: failures don't fail the overall run (wait is authoritative).
    fn stream_logs(
        &self,
        h: &crate::backend::RunHandle,
        out: &mut dyn Write,
    ) -> Result<(), BackendError> {
        // Discover the pod name, bounded — a workflow that never schedules
        // must fail streaming loudly instead of hanging the run.
        let pod_name = self.discover_pod_with_retry(&h.handle, POD_DISCOVERY_TIMEOUT)?;

        // Stream logs from the pod
        let output = self
            .kubectl(&["logs", "-f", &pod_name])
            .map_err(|e| BackendError::new(&format!("failed to stream logs: {}", e)))?;

        // Write log output to the writer
        out.write_all(&output.stdout)
            .map_err(|e| BackendError::new(&format!("failed to write logs: {}", e)))?;

        Ok(())
    }

    /// Wait for the workflow to complete and return its verdict.
    ///
    /// Polls kubectl get workflow status.phase until terminal or deadline.
    /// Reads verdict.json from outputs.parameters if available.
    /// Attributions gate failures to "[gantry] gate:" in output.
    fn wait(
        &self,
        h: &crate::backend::RunHandle,
        deadline: Instant,
    ) -> Result<Verdict, BackendError> {
        loop {
            // Check deadline
            if Instant::now() > deadline {
                return Err(BackendError::new("workflow deadline exceeded"));
            }

            // Get workflow status
            let output = self.kubectl(&["get", "workflow", &h.handle, "-o", "json"])?;

            if !output.status.success() {
                thread::sleep(STATUS_POLL);
                continue;
            }

            // kubectl returns the whole Workflow object; the phase lives in
            // its `status` stanza, which is absent until the controller first
            // reconciles the workflow (and may be phase-less right after).
            let json = String::from_utf8_lossy(&output.stdout);
            let obj: WorkflowObject = serde_json::from_str(&json).map_err(|e| {
                BackendError::new(&format!("failed to parse workflow status: {}", e))
            })?;
            let Some(status) = obj.status else {
                thread::sleep(STATUS_POLL);
                continue;
            };

            // Check if terminal phase
            match status.phase.as_deref() {
                Some("Succeeded" | "Failed" | "Error") => {
                    // Terminal phase - try to read verdict.json
                    if let Some(outputs) = status.outputs {
                        if let Some(parameters) = outputs.parameters {
                            for param in parameters {
                                if param.name == "verdict" {
                                    if let Some(value) = param.value {
                                        match VerdictJson::parse(&value) {
                                            Ok(vj) => {
                                                let verdict = vj.to_verdict();
                                                // Attributions for gate failures
                                                if verdict == Verdict::GateFailure {
                                                    eprintln!("[gantry] gate: quality gate failed");
                                                }
                                                return Ok(verdict);
                                            }
                                            Err(e) => {
                                                // Fall back to exit code if verdict.json parsing fails
                                                eprintln!(
                                                    "[gantry] failed to parse verdict.json: {}",
                                                    e
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Fall back to phase-based classification
                    return Ok(match status.phase.as_deref() {
                        Some("Succeeded") => Verdict::Pass,
                        Some("Failed") => Verdict::TestFailure,
                        _ => Verdict::InfraFailure,
                    });
                }
                _ => {
                    // Not terminal - sleep and retry
                    thread::sleep(STATUS_POLL);
                }
            }
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

        if !output.status.success() {
            return Err(BackendError::new(&format!(
                "failed to cancel workflow: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::RunHandle;

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
    /// all four parameters (repo, revision, args-json, builder-image). kubectl
    /// receives JSON on stdin, and JSON is a YAML subset, so this pins the YAML
    /// manifest shape too. Object key order is irrelevant to the comparison.
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
                        { "name": "builder-image", "value": "rust:1.83" },
                    ]
                }
            }
        });

        assert_eq!(actual, expected);
    }

    /// Unconfigured builder image must omit the parameter entirely, so the
    /// WorkflowTemplate default applies (an empty-value override would break it).
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

        // The whole manifest must match the three-parameter shape exactly:
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

    #[test]
    fn test_verdict_json_parse_pass() {
        let json = r#"{
            "schema_version": 1,
            "phase": "Succeeded",
            "exit_code": 0,
            "oom": false,
            "deadline_exceeded": false
        }"#;

        let vj = VerdictJson::parse(json).unwrap();
        assert_eq!(vj.to_verdict(), Verdict::Pass);
    }

    #[test]
    fn test_verdict_json_parse_test_failure() {
        let json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "oom": false,
            "deadline_exceeded": false,
            "failure_class": "test-failure"
        }"#;

        let vj = VerdictJson::parse(json).unwrap();
        assert_eq!(vj.to_verdict(), Verdict::TestFailure);
    }

    #[test]
    fn test_verdict_json_parse_oom_is_infra_failure() {
        let json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "oom": true,
            "deadline_exceeded": false
        }"#;

        let vj = VerdictJson::parse(json).unwrap();
        assert_eq!(vj.to_verdict(), Verdict::InfraFailure);
    }

    #[test]
    fn test_verdict_json_parse_unsupported_version() {
        let json = r#"{
            "schema_version": 2,
            "phase": "Succeeded",
            "exit_code": 0
        }"#;

        let result = VerdictJson::parse(json);
        assert!(result.is_err());
    }

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

    #[test]
    fn test_verdict_json_parse_gate_failure() {
        let json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "oom": false,
            "deadline_exceeded": false,
            "failure_class": "gate-failure"
        }"#;

        let vj = VerdictJson::parse(json).unwrap();
        assert_eq!(vj.to_verdict(), Verdict::GateFailure);
    }

    #[test]
    fn test_verdict_json_gate_failure_with_tests_passed() {
        // Scenario: tests passed (exit 0) but clippy gate failed (overall exit 1)
        // The verdict.json explicitly indicates gate failure
        let json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "oom": false,
            "deadline_exceeded": false,
            "failure_class": "gate-failure"
        }"#;

        let vj = VerdictJson::parse(json).unwrap();
        let verdict = vj.to_verdict();

        assert_eq!(verdict, Verdict::GateFailure);
        // Verify gate failure never triggers local fallback (infra-only)
        assert!(!verdict.is_infra_failure());
        // Verify gate failure is considered a test result (tests ran)
        assert!(verdict.has_test_result());
    }

    #[test]
    fn test_verdict_json_test_failure_vs_gate_failure() {
        // Test failure: no failure_class, exit 1, phase Failed
        let test_json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "oom": false,
            "deadline_exceeded": false
        }"#;

        let test_vj = VerdictJson::parse(test_json).unwrap();
        assert_eq!(test_vj.to_verdict(), Verdict::TestFailure);

        // Gate failure: explicit failure_class
        let gate_json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "oom": false,
            "deadline_exceeded": false,
            "failure_class": "gate-failure"
        }"#;

        let gate_vj = VerdictJson::parse(gate_json).unwrap();
        assert_eq!(gate_vj.to_verdict(), Verdict::GateFailure);
    }

    #[test]
    fn test_verdict_json_gate_failure_not_infra_failure() {
        // Gate failures should NOT trigger local fallback
        let json = r#"{
            "schema_version": 1,
            "phase": "Failed",
            "exit_code": 1,
            "oom": false,
            "deadline_exceeded": false,
            "failure_class": "gate-failure"
        }"#;

        let vj = VerdictJson::parse(json).unwrap();
        let verdict = vj.to_verdict();

        assert_eq!(verdict, Verdict::GateFailure);
        assert_eq!(verdict.to_exit_code(), 1); // Same exit code as test failure
        assert!(!verdict.is_infra_failure()); // Does NOT trigger local fallback
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

    /// A failing kubectl surfaces its stderr as the submit error, not a
    /// masked "failed to write workflow" (the manifest write may hit a
    /// broken pipe because kubectl exited before reading stdin).
    #[test]
    fn test_submit_surfaces_kubectl_stderr_on_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let kubectl = write_mock_kubectl(
            tmp.path(),
            "#!/usr/bin/env bash\necho 'mock kubectl exploded' >&2\nexit 1\n",
        );
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
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

        let err = with_exec_retry(|| backend.submit(&spec))
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
        assert_eq!(value_of("builder-image"), "rust:1.83");
    }

    /// stdout that does not carry the `workflow.argoproj.io/` prefix (e.g. an
    /// unexpected message) is a loud error, not a garbage handle.
    #[test]
    fn test_submit_rejects_stdout_without_workflow_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let kubectl = write_mock_kubectl(
            tmp.path(),
            "#!/usr/bin/env bash\necho 'error: unrecognized resource'\n",
        );
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
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

        let err = with_exec_retry(|| backend.submit(&spec))
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
        let tmp = tempfile::tempdir().expect("tempdir");
        let kubectl = write_mock_kubectl(
            tmp.path(),
            "#!/usr/bin/env bash\necho 'workflow.argoproj.io/'\n",
        );
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
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

        let err = with_exec_retry(|| backend.submit(&spec))
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
        let tmp = tempfile::tempdir().expect("tempdir");
        let kubectl = write_mock_kubectl(
            tmp.path(),
            "#!/usr/bin/env bash\necho 'workflow.argoproj.io/ created'\n",
        );
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
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

        let err = with_exec_retry(|| backend.submit(&spec))
            .expect_err("submit must not return the status word as the handle");
        assert!(
            err.reason.contains("missing workflow name"),
            "{}",
            err.reason
        );
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
    /// of spinning forever on a workflow that never schedules.
    #[test]
    fn test_pod_discovery_gives_up_after_timeout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let kubectl =
            write_mock_kubectl(tmp.path(), "#!/usr/bin/env bash\necho '{\"items\":[]}'\n");
        let backend = ArgoBackend::new(ArgoConfig {
            kubectl_path: kubectl.to_string_lossy().into_owned(),
            ..ArgoConfig::default()
        });
        let handle = RunHandle::new("gantry-abc123");

        let err =
            with_exec_retry(|| backend.discover_pod_with_retry(&handle.handle, Duration::ZERO))
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
