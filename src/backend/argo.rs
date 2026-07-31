// gantry — Argo Workflows backend (plan §"Phase 1a", Components §5 "RemoteBackend trait").
//
// Phase 1a: implements the Argo backend that builds Workflow manifests with serde,
// submits via kubectl, polls pod logs for streaming, and polls status.phase for the
// authoritative verdict. Falls back to reading outputs.parameters.output when podGC
// ate the logs.
//
// The backend uses a WorkflowTemplate (configurable) with parameters:
// - repo: repository URL
// - revision: commit SHA to check out
// - args_json: JSON array of command arguments
// - builder_image: container image for the Rust toolchain
//
// The template emits a structured verdict.json output parameter with:
// - schema_version: version identifier for the verdict schema
// - phase: workflow phase (Succeeded/Failed/Error)
// - exit_code: the test run's exit code
// - oom: true if the pod was OOMKilled
// - deadline_exceeded: true if the workflow exceeded its deadline
// - failure_class: optional classification (compile-error, test-failure, doctest, harness-panic)

use crate::backend::{BackendError, RemoteBackend, RunHandle, RunSpec, Verdict, VerdictJson};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Instant;

/// Argo-specific configuration for the backend.
#[derive(Debug, Clone)]
pub struct ArgoConfig {
    /// Path to kubectl config file (uses default if None).
    pub kubeconfig: Option<String>,
    /// Kubernetes namespace for workflows.
    pub namespace: String,
    /// WorkflowTemplate name to use.
    pub template: String,
    /// Generate name prefix for workflows (e.g., "gantry-").
    pub generate_name: String,
    /// Builder image to use (overrides template default if set).
    pub builder_image: Option<String>,
    /// Workflow deadline in minutes.
    pub deadline_minutes: u64,
}

impl Default for ArgoConfig {
    fn default() -> Self {
        ArgoConfig {
            kubeconfig: None,
            namespace: "argo-workflows".to_string(),
            template: "gantry-verify".to_string(),
            generate_name: "gantry-".to_string(),
            builder_image: None,
            deadline_minutes: 40,
        }
    }
}

/// The Argo Workflow backend implementation.
pub struct ArgoBackend {
    /// Argo-specific configuration.
    config: ArgoConfig,
}

impl ArgoBackend {
    /// Create a new ArgoBackend with the given configuration.
    pub fn new(config: ArgoConfig) -> Self {
        ArgoBackend { config }
    }

    /// Create a new ArgoBackend with default configuration.
    pub fn default_config() -> Self {
        ArgoBackend {
            config: ArgoConfig::default(),
        }
    }

    /// Build the kubectl command with optional kubeconfig.
    fn kubectl_cmd(&self) -> Command {
        let mut cmd = Command::new("kubectl");
        if let Some(kubeconfig) = &self.config.kubeconfig {
            cmd.arg("--kubeconfig").arg(kubeconfig);
        }
        cmd
    }

    /// Build a Workflow manifest using serde (no string splicing per S-4).
    fn build_workflow_manifest(&self, spec: &RunSpec) -> Workflow {
        let generate_name = format!("{}-", self.config.generate_name);

        Workflow {
            apiVersion: "argoproj.io/v1alpha1".to_string(),
            kind: "Workflow".to_string(),
            metadata: WorkflowMetadata {
                generateName: generate_name,
                namespace: self.config.namespace.clone(),
                labels: vec![
                    (["gantry", "run", "tool"].join("/"), spec.tool.clone()),
                    (["gantry", "run", "sha"].join("/"), spec.sha.clone()),
                ],
            },
            spec: WorkflowSpec {
                workflowTemplateRef: Some(WorkflowTemplateRef {
                    name: self.config.template.clone(),
                }),
                entrypoint: "main".to_string(),
                arguments: WorkflowArguments {
                    parameters: vec![
                        WorkflowParameter {
                            name: "repo".to_string(),
                            value: spec.repo_url.clone(),
                        },
                        WorkflowParameter {
                            name: "revision".to_string(),
                            value: spec.sha.clone(),
                        },
                        WorkflowParameter {
                            name: "args_json".to_string(),
                            value: serde_json::to_string(&spec.args).unwrap_or_else(|_| "[]".to_string()),
                        },
                    ],
                },
                // Set activeDeadlineSeconds to avoid runaway workflows
                activeDeadlineSeconds: Some(self.config.deadline_minutes * 60),
            },
        }
    }

    /// Submit a workflow to Argo via kubectl create -f -.
    fn submit_workflow(&self, workflow: &Workflow) -> Result<String, BackendError> {
        let yaml = serde_yaml::to_string(workflow)
            .map_err(|e| BackendError::new(&format!("failed to serialize workflow: {}", e)))?;

        let mut child = self.kubectl_cmd()
            .arg("create")
            .arg("-f")
            .arg("-")
            .arg("-n")
            .arg(&self.config.namespace)
            .arg("-o")
            .arg("jsonpath={.metadata.name}")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| BackendError::new(&format!("failed to spawn kubectl: {}", e)))?;

        // Write YAML to stdin
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(yaml.as_bytes())
                .map_err(|e| BackendError::new(&format!("failed to write workflow: {}", e)))?;
        }

        let output = child
            .wait_with_output()
            .map_err(|e| BackendError::new(&format!("failed to wait for kubectl: {}", e)))?;

        if !output.status.success() {
            return Err(BackendError::new(&format!(
                "kubectl create failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let workflow_name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if workflow_name.is_empty() {
            return Err(BackendError::new("kubectl returned empty workflow name"));
        }

        Ok(workflow_name)
    }

    /// Get the workflow status via kubectl get.
    fn get_workflow_status(&self, workflow_name: &str) -> Result<WorkflowStatus, BackendError> {
        let output = self
            .kubectl_cmd()
            .arg("get")
            .arg("workflow")
            .arg(workflow_name)
            .arg("-n")
            .arg(&self.config.namespace)
            .arg("-o")
            .arg("json")
            .output()
            .map_err(|e| BackendError::new(&format!("failed to get workflow: {}", e)))?;

        if !output.status.success() {
            return Err(BackendError::new(&format!(
                "kubectl get workflow failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        serde_json::from_slice(&output.stdout).map_err(|e| {
            BackendError::new(&format!(
                "failed to parse workflow status: {}",
                e
            ))
        })
    }

    /// Get the pod name for a workflow (for log streaming).
    fn get_workflow_pod(&self, workflow_name: &str) -> Result<Option<String>, BackendError> {
        let output = self
            .kubectl_cmd()
            .arg("get")
            .arg("pod")
            .arg(&format!("workflows.argoproj.io/workflow={}", workflow_name))
            .arg("-n")
            .arg(&self.config.namespace)
            .arg("-o")
            .arg("jsonpath={.items[0].metadata.name}")
            .output()
            .map_err(|e| BackendError::new(&format!("failed to get pod: {}", e)))?;

        if !output.status.success() {
            // Pod may not exist yet — this is OK
            return Ok(None);
        }

        let pod_name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if pod_name.is_empty() {
            Ok(None)
        } else {
            Ok(Some(pod_name))
        }
    }

    /// Parse verdict.json from workflow output parameters.
    fn parse_verdict_json(&self, status: &WorkflowStatus) -> Option<VerdictJson> {
        // Try to get the verdict from output parameters
        if let Some(outputs) = &status.status.outputs {
            for param in &outputs.parameters {
                if param.name == "output" {
                    if let Ok(Some(verdict)) = VerdictJson::parse(&param.value) {
                        return Some(verdict);
                    }
                }
            }
        }
        None
    }
}

impl RemoteBackend for ArgoBackend {
    /// Submit a run to Argo Workflows.
    fn submit(&self, spec: &RunSpec) -> Result<RunHandle, BackendError> {
        let workflow = self.build_workflow_manifest(spec);
        let workflow_name = self.submit_workflow(&workflow)?;
        Ok(RunHandle::argo(&workflow_name))
    }

    /// Stream logs from the workflow pod.
    fn stream_logs(&self, h: &RunHandle, out: &mut dyn Write) -> Result<(), BackendError> {
        let (workflow_name, pod_name) = match h {
            RunHandle::Argo {
                workflow_name,
                pod_name,
            } => (workflow_name, pod_name),
            _ => return Err(BackendError::new("argo backend received non-Argo handle")),
        };

        // If we don't have a pod name yet, try to find it
        let pod = if let Some(p) = pod_name {
            p.clone()
        } else {
            match self.get_workflow_pod(workflow_name) {
                Ok(Some(p)) => p,
                Ok(None) => return Err(BackendError::new("pod not found for workflow")),
                Err(e) => return Err(e),
            }
        };

        // Stream logs from the pod
        let output = self
            .kubectl_cmd()
            .arg("logs")
            .arg(&pod)
            .arg("-n")
            .arg(&self.config.namespace)
            .arg("--follow")
            .output()
            .map_err(|e| BackendError::new(&format!("failed to stream logs: {}", e)))?;

        out.write_all(&output.stdout)
            .map_err(|e| BackendError::new(&format!("failed to write logs: {}", e)))?;

        Ok(())
    }

    /// Wait for the workflow to complete and return its verdict.
    fn wait(&self, h: &RunHandle, deadline: Instant) -> Result<Verdict, BackendError> {
        let workflow_name = match h {
            RunHandle::Argo {
                workflow_name,
                pod_name: _,
            } => workflow_name,
            _ => return Err(BackendError::new("argo backend received non-Argo handle")),
        };

        // Poll workflow status until completion or deadline
        loop {
            if Instant::now() >= deadline {
                return Ok(Verdict::infra_failure("client deadline exceeded".to_string()));
            }

            let status = self.get_workflow_status(workflow_name)?;

            // Check if workflow is complete
            match status.status.phase.as_str() {
                "Succeeded" => {
                    // Try to parse verdict.json from output parameters
                    if let Some(verdict) = self.parse_verdict_json(&status) {
                        return Ok(verdict.to_verdict());
                    }
                    // Fallback: no verdict.json, treat as Pass
                    return Ok(Verdict::Pass);
                }
                "Failed" | "Error" => {
                    // Try to parse verdict.json for detailed classification
                    if let Some(verdict) = self.parse_verdict_json(&status) {
                        return Ok(verdict.to_verdict());
                    }

                    // Check for OOM or deadline exceeded from node status
                    if let Some(nodes) = status.status.nodes {
                        for (_, node) in nodes {
                            if let Some(phase) = node.phase {
                                if phase == "Failed" {
                                    if let Some(message) = node.message {
                                        if message.contains("OOMKilled") {
                                            return Ok(Verdict::infra_failure(
                                                "pod was OOMKilled".to_string(),
                                            ));
                                        }
                                        if message.contains("DeadlineExceeded") {
                                            return Ok(Verdict::infra_failure(
                                                "workflow deadline exceeded".to_string(),
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Default: treat as test failure
                    return Ok(Verdict::test_failure(1));
                }
                "" | "Pending" | "Running" => {
                    // Still running, sleep and retry
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
                _ => {
                    return Ok(Verdict::infra_failure(format!(
                        "unknown workflow phase: {}",
                        status.status.phase
                    )));
                }
            }
        }
    }

    /// Describe the workflow with its URL/name.
    fn describe(&self, h: &RunHandle) -> String {
        match h {
            RunHandle::Argo {
                workflow_name,
                pod_name: _,
            } => {
                format!(
                    "https://argo-server/workflows/{}",
                    workflow_name
                )
            }
            _ => "unknown".to_string(),
        }
    }

    /// Cancel the workflow via kubectl delete.
    fn cancel(&self, h: &RunHandle) -> Result<(), BackendError> {
        let workflow_name = match h {
            RunHandle::Argo {
                workflow_name,
                pod_name: _,
            } => workflow_name,
            _ => return Err(BackendError::new("argo backend received non-Argo handle")),
        };

        let output = self
            .kubectl_cmd()
            .arg("delete")
            .arg("workflow")
            .arg(workflow_name)
            .arg("-n")
            .arg(&self.config.namespace)
            .output()
            .map_err(|e| BackendError::new(&format!("failed to cancel workflow: {}", e)))?;

        if !output.status.success() {
            return Err(BackendError::new(&format!(
                "kubectl delete failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(())
    }
}

// === Serde types for Workflow manifests (no string splicing per S-4) ===

/// A Workflow resource for Argo Workflows.
#[derive(Debug, Serialize, Deserialize)]
struct Workflow {
    #[serde(rename = "apiVersion")]
    apiVersion: String,
    kind: String,
    metadata: WorkflowMetadata,
    spec: WorkflowSpec,
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkflowMetadata {
    #[serde(rename = "generateName")]
    generateName: String,
    namespace: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    labels: Vec<(String, String)>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkflowSpec {
    workflowTemplateRef: Option<WorkflowTemplateRef>,
    entrypoint: String,
    arguments: WorkflowArguments,
    #[serde(rename = "activeDeadlineSeconds", skip_serializing_if = "Option::is_none")]
    activeDeadlineSeconds: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkflowTemplateRef {
    name: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkflowArguments {
    parameters: Vec<WorkflowParameter>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkflowParameter {
    name: String,
    value: String,
}

/// Workflow status as returned by kubectl get workflow -o json.
#[derive(Debug, Deserialize)]
struct WorkflowStatus {
    status: WorkflowStatusFields,
}

#[derive(Debug, Deserialize)]
struct WorkflowStatusFields {
    phase: String,
    #[serde(rename = "startedAt", default)]
    startedAt: String,
    #[serde(rename = "finishedAt", default)]
    finishedAt: String,
    #[serde(default)]
    outputs: Option<WorkflowOutputs>,
    #[serde(default)]
    nodes: Option<std::collections::HashMap<String, WorkflowNode>>,
}

#[derive(Debug, Deserialize)]
struct WorkflowOutputs {
    #[serde(default)]
    parameters: Vec<OutputParameter>,
}

#[derive(Debug, Deserialize)]
struct OutputParameter {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct WorkflowNode {
    #[serde(rename = "phase", default)]
    phase: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// Parsed verdict from verdict.json output parameter.
#[derive(Debug, Deserialize)]
struct ParsedVerdict {
    #[serde(rename = "schema_version")]
    schema_version: u32,
    #[serde(default)]
    phase: String,
    #[serde(rename = "exit_code")]
    exit_code: i32,
    #[serde(default)]
    oom: bool,
    #[serde(rename = "deadline_exceeded", default)]
    deadline_exceeded: bool,
    #[serde(rename = "failure_class", default)]
    failure_class: Option<String>,
}

impl ParsedVerdict {
    /// Convert the parsed verdict to a Verdict enum.
    fn to_verdict(self) -> Verdict {
        // Check for infrastructure failures first
        if self.oom || self.deadline_exceeded {
            return Verdict::infra_failure(if self.oom {
                "pod was OOMKilled"
            } else {
                "workflow deadline exceeded"
            }.to_string());
        }

        // Parse failure_class if present
        if let Some(class) = self.failure_class {
            match class.as_str() {
                "compile-error" | "doctest" | "harness-panic" => {
                    // These are test failures
                    return Verdict::test_failure(self.exit_code.max(1));
                }
                "test-failure" => {
                    return Verdict::test_failure(self.exit_code.max(1));
                }
                _ => {
                    // Unknown class, treat as test failure
                    return Verdict::test_failure(self.exit_code.max(1));
                }
            }
        }

        // No failure_class, use exit code
        match self.phase.as_str() {
            "Succeeded" => Verdict::Pass,
            "Failed" | "Error" => Verdict::test_failure(self.exit_code.max(1)),
            _ => Verdict::infra_failure(format!("unknown phase: {}", self.phase)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_argo_config_default() {
        let config = ArgoConfig::default();
        assert_eq!(config.namespace, "argo-workflows");
        assert_eq!(config.template, "gantry-verify");
        assert_eq!(config.deadline_minutes, 40);
    }

    #[test]
    fn test_argo_backend_new() {
        let config = ArgoConfig::default();
        let backend = ArgoBackend::new(config.clone());
        assert_eq!(backend.config.namespace, config.namespace);
    }

    #[test]
    fn test_build_workflow_manifest() {
        let backend = ArgoBackend::default_config();
        let spec = RunSpec::new(
            "cargo",
            "test",
            vec!["--".to_string(), "--nocapture".to_string()],
            "https://github.com/test/repo",
            "abc123",
            ".".into(),
        );

        let workflow = backend.build_workflow_manifest(&spec);
        assert_eq!(workflow.apiVersion, "argoproj.io/v1alpha1");
        assert_eq!(workflow.kind, "Workflow");
        assert!(workflow.metadata.generateName.starts_with("gantry-"));
        assert_eq!(workflow.spec.entrypoint, "main");

        // Check parameters
        let param_names: Vec<_> = workflow.spec.arguments.parameters
            .iter()
            .map(|p| p.name.clone())
            .collect();
        assert!(param_names.contains(&"repo".to_string()));
        assert!(param_names.contains(&"revision".to_string()));
        assert!(param_names.contains(&"args_json".to_string()));
    }

    #[test]
    fn test_parsed_verdict_to_verdict_pass() {
        let parsed = ParsedVerdict {
            schema_version: 1,
            phase: "Succeeded".to_string(),
            exit_code: 0,
            oom: false,
            deadline_exceeded: false,
            failure_class: None,
        };
        assert_eq!(parsed.to_verdict(), Verdict::Pass);
    }

    #[test]
    fn test_parsed_verdict_to_verdict_test_failure() {
        let parsed = ParsedVerdict {
            schema_version: 1,
            phase: "Failed".to_string(),
            exit_code: 1,
            oom: false,
            deadline_exceeded: false,
            failure_class: Some("test-failure".to_string()),
        };
        assert!(matches!(parsed.to_verdict(), Verdict::TestFailure { .. }));
    }

    #[test]
    fn test_parsed_verdict_to_verdict_oom() {
        let parsed = ParsedVerdict {
            schema_version: 1,
            phase: "Failed".to_string(),
            exit_code: 137,
            oom: true,
            deadline_exceeded: false,
            failure_class: None,
        };
        assert!(matches!(parsed.to_verdict(), Verdict::InfraFailure { .. }));
    }

    #[test]
    fn test_parsed_verdict_to_verdict_deadline_exceeded() {
        let parsed = ParsedVerdict {
            schema_version: 1,
            phase: "Failed".to_string(),
            exit_code: 1,
            oom: false,
            deadline_exceeded: true,
            failure_class: None,
        };
        assert!(matches!(parsed.to_verdict(), Verdict::InfraFailure { .. }));
    }
}
