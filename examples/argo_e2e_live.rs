//! Live-cluster end-to-end exercise for the Argo backend.
//!
//! Drives the production path against a real cluster: the serde-built
//! Workflow manifest piped into `kubectl create -f -`, pod discovery,
//! `kubectl logs -f` streaming, and `kubectl get workflow -o json` polling to
//! a terminal phase. Not a test — it submits real workloads — so CI never
//! runs it; drive it by hand:
//!
//! ```text
//! GANTRY_E2E_KUBECONFIG=~/.kube/iad-ci.kubeconfig \
//! GANTRY_E2E_TEMPLATE=cgov-install-smoke \
//! cargo run --example argo_e2e_live
//! ```
//!
//! The referenced template only needs to exist in the target namespace; it
//! need not understand gantry's parameters (extra workflow-level arguments
//! are ignored by argo). Log streaming races podGC (`OnPodCompletion`) either
//! way: the shipped gantry-verify template recovers the log from its
//! `output` output parameter, but a foreign template has no such parameter,
//! so this harness waits for the pod's container to start before calling
//! `stream_logs` — the capture itself still rides the backend's
//! `kubectl logs -f` path.

use std::io::stdout;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use gantry::backend::argo::{ArgoBackend, ArgoConfig};
use gantry::backend::{RemoteBackend, RunSpec};

/// Read an environment variable or fall back to `default`.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn main() {
    let config = ArgoConfig {
        kubeconfig: env_or("GANTRY_E2E_KUBECONFIG", ""),
        template: env_or("GANTRY_E2E_TEMPLATE", "gantry-verify"),
        namespace: env_or("GANTRY_E2E_NAMESPACE", "argo-workflows"),
        ..ArgoConfig::default()
    };
    let backend = ArgoBackend::new(config.clone());

    let spec = RunSpec::new(
        "cargo",
        "test",
        vec![],
        &env_or(
            "GANTRY_E2E_REPO",
            "https://git.ardenone.com/jedarden/gantry-rs.git",
        ),
        &env_or("GANTRY_E2E_SHA", "HEAD"),
        "",
    );

    let handle = backend.submit(&spec).expect("workflow submission");
    println!("workflow: {}", handle.handle);
    println!("url: {}", backend.describe(&handle));

    let pod = wait_for_started_pod(&config, &handle.handle);
    let pod_name = pod.as_deref().unwrap_or("<unknown>");
    if pod.is_none() {
        eprintln!("no started pod within the wait budget; streaming will try the output parameter");
    }

    println!("--- pod log stream ({}) ---", pod_name);
    if let Err(e) = backend.stream_logs(&handle, &mut stdout()) {
        eprintln!("log streaming failed (best-effort): {}", e);
    }

    let budget: u64 = env_or("GANTRY_E2E_DEADLINE_SECS", "900")
        .parse()
        .expect("GANTRY_E2E_DEADLINE_SECS is an integer");
    let verdict = backend
        .wait(&handle, Instant::now() + Duration::from_secs(budget))
        .expect("wait for a terminal phase");
    println!("--- verdict: {:?}", verdict);
}

/// Poll until the workflow's pod reports a started container.
///
/// Plain read-only kubectl recon. The backend's own discovery returns the
/// first matching pod without checking whether its container has started, and
/// `kubectl logs -f` on a container that is still being created exits with a
/// BadRequest — which a foreign template's missing `output` parameter cannot
/// recover from.
fn wait_for_started_pod(config: &ArgoConfig, workflow: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let mut cmd = Command::new(&config.kubectl_path);
        if !config.kubeconfig.is_empty() {
            cmd.arg("--kubeconfig").arg(&config.kubeconfig);
        }
        cmd.arg("-n").arg(&config.namespace);
        cmd.args([
            "get",
            "pods",
            "-l",
            &format!("workflows.argoproj.io/workflow={}", workflow),
            "-o",
            "json",
        ]);
        if let Ok(out) = cmd.output() {
            if let Ok(list) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                if let Some(pod) = list["items"].as_array().and_then(|items| items.first()) {
                    let name = pod["metadata"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let phase = pod["status"]["phase"].as_str().unwrap_or_default();
                    let started =
                        pod["status"]["containerStatuses"][0]["state"]["running"].is_object();
                    println!("pod {}: phase={}", name, phase);
                    if started {
                        return Some(name);
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_secs(2));
    }
}
