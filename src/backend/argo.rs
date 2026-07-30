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
use std::time::Instant;

/// Argo Workflow manifest structures (serde-based, no string splicing).
///
/// Phase 1a implements minimal Workflow submit spec matching the gantry-verify
/// template contract: parameters (repo, revision, args_json), generateName,
/// templateRef, and entrypoint.
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
    pub struct Metadata {
        pub generate_name: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct WorkflowSpec {
        pub entrypoint: String,
        pub templates: Vec<Template>,
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
    pub struct Template {
        pub name: String,
        pub template_ref: Option<TemplateRef>,
        pub arguments: Option<Arguments>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct TemplateRef {
        pub name: String,
        pub cluster_scope: Option<bool>,
    }

    /// Workflow status from kubectl get workflow -o json.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct WorkflowStatus {
        pub phase: String,
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
        pub phase: String,
        pub message: Option<String>,
        pub outputs: Option<Outputs>,
    }

    impl Workflow {
        /// Create a new Workflow manifest for gantry.
        pub fn new(
            generate_name: &str,
            template_name: &str,
            repo_url: &str,
            sha: &str,
            args_json: &str,
        ) -> Self {
            Workflow {
                api_version: "argoproj.io/v1alpha1".to_string(),
                kind: "Workflow".to_string(),
                metadata: Metadata {
                    generate_name: generate_name.to_string(),
                },
                spec: WorkflowSpec {
                    entrypoint: "gantry-verify".to_string(),
                    arguments: Some(Arguments {
                        parameters: vec![
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
                        ],
                    }),
                    templates: vec![Template {
                        name: "gantry-verify".to_string(),
                        template_ref: Some(TemplateRef {
                            name: template_name.to_string(),
                            cluster_scope: Some(false),
                        }),
                        arguments: None,
                    }],
                },
            }
        }
    }
}

use workflow::{Workflow, WorkflowStatus};

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
/// Phase 1a: minimal config (kubeconfig, namespace, template, generate_name, base_url).
#[derive(Debug, Clone, PartialEq)]
pub struct ArgoConfig {
    /// Path to kubeconfig file (empty = use default).
    pub kubeconfig: String,
    /// Kubernetes namespace.
    pub namespace: String,
    /// WorkflowTemplate name to reference.
    pub template: String,
    /// generateName prefix for submitted workflows.
    pub generate_name: String,
    /// Base URL for Argo UI (optional, for describe() to return human-readable URLs).
    pub base_url: Option<String>,
}

impl Default for ArgoConfig {
    fn default() -> Self {
        ArgoConfig {
            kubeconfig: "".to_string(),
            namespace: "argo-workflows".to_string(),
            template: "gantry-verify".to_string(),
            generate_name: "gantry-".to_string(),
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
        let mut cmd = Command::new("kubectl");

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
        );

        // Serialize to JSON
        let workflow_json = serde_json::to_string_pretty(&workflow)
            .map_err(|e| BackendError::new(&format!("failed to serialize workflow: {}", e)))?;

        // Submit via kubectl create -f -
        let mut cmd = Command::new("kubectl");

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

        // Write workflow to stdin
        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(workflow_json.as_bytes())
                .map_err(|e| BackendError::new(&format!("failed to write workflow: {}", e)))?;
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

        // Extract workflow name from stdout
        let stdout = String::from_utf8_lossy(&output.stdout);
        let workflow_name = stdout
            .trim()
            .strip_prefix("workflow.argoproj.io/")
            .ok_or_else(|| BackendError::new("kubectl output missing workflow name"))?
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
        // Discover pod name
        let pod_name = loop {
            match self.discover_pod(&h.handle)? {
                Some(name) => break name,
                None => {
                    thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            }
        };

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
                thread::sleep(std::time::Duration::from_secs(2));
                continue;
            }

            let json = String::from_utf8_lossy(&output.stdout);
            let status: WorkflowStatus = serde_json::from_str(&json).map_err(|e| {
                BackendError::new(&format!("failed to parse workflow status: {}", e))
            })?;

            // Check if terminal phase
            match status.phase.as_str() {
                "Succeeded" | "Failed" | "Error" => {
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
                    return Ok(match status.phase.as_str() {
                        "Succeeded" => Verdict::Pass,
                        "Failed" => Verdict::TestFailure,
                        "Error" => Verdict::InfraFailure,
                        _ => Verdict::InfraFailure,
                    });
                }
                _ => {
                    // Not terminal - sleep and retry
                    thread::sleep(std::time::Duration::from_secs(2));
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
        );

        let json = serde_json::to_string(&workflow);
        assert!(json.is_ok());

        // Verify no string splicing - all fields are structured
        let parsed: Workflow = serde_json::from_str(&json.unwrap()).unwrap();
        assert_eq!(parsed.metadata.generate_name, "gantry-");
    }

    #[test]
    fn test_argo_config_default() {
        let config = ArgoConfig::default();
        assert_eq!(config.namespace, "argo-workflows");
        assert_eq!(config.template, "gantry-verify");
        assert_eq!(config.generate_name, "gantry-");
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
            kubeconfig: "".to_string(),
            namespace: "argo-workflows".to_string(),
            template: "gantry-verify".to_string(),
            generate_name: "gantry-".to_string(),
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
            kubeconfig: "".to_string(),
            namespace: "argo-workflows".to_string(),
            template: "gantry-verify".to_string(),
            generate_name: "gantry-".to_string(),
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
            kubeconfig: "".to_string(),
            namespace: "my-namespace".to_string(),
            template: "gantry-verify".to_string(),
            generate_name: "gantry-".to_string(),
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
