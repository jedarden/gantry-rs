# bf-4aph: Argo Backend Implementation

## Summary

Successfully implemented the Argo Workflows backend for gantry-rs in `src/backend/argo.rs`.

## Implementation Details

### Core Components

1. **Workflow Manifest Structures (serde-based, S-4 compliant)**
   - `Workflow`: Complete Argo v1alpha1 Workflow manifest structure
   - `WorkflowSpec`: Entry point, templates, and arguments
   - `Template`: Template references for WorkflowTemplate cluster resources
   - `WorkflowStatus`: Status parsing for wait() polling
   - `Outputs`: Output parameter extraction for verdict.json

2. **RemoteBackend Trait Implementation**
   - `submit()`: Builds Workflow manifest with serde, submits via `kubectl create -f -`
   - `stream_logs()`: Discovers pod name, streams via `kubectl logs -f`
   - `wait()`: Polls `status.phase` until terminal, reads `verdict.json` from `outputs.parameters`
   - `describe()`: Returns human-readable workflow URL using configured `base_url`
   - `cancel()`: Deletes workflow via `kubectl delete workflow`

3. **Configuration**
   - `ArgoConfig`: Kubeconfig path, namespace, template name, generate_name prefix, base_url
   - Default: namespace="argo-workflows", template="gantry-verify", generate_name="gantry-"

4. **Error Handling**
   - Graceful fallback from verdict.json to phase-based classification
   - podGC log recovery via `outputs.parameters["verdict"]`
   - Proper deadline enforcement in wait()

## Acceptance Criteria Status

- ✅ **Compiles**: `cargo build --release` succeeds
- ✅ **Workflow manifest serde structs match Argo schema**: Proper serde structures for v1alpha1 API
- ✅ **submit() creates workflow via kubectl**: Uses `kubectl create -f -` with stdin piping
- ✅ **stream_logs() writes pod output**: Pod discovery + `kubectl logs -f`
- ✅ **wait() polls status.phase and returns Verdict**: Polls until terminal, reads verdict.json
- ✅ **describe() returns human URL**: Returns `base_url/workflows/namespace/name` or `workflow/name`

## Test Results

All 9 argo backend tests pass:
- `test_workflow_manifest_serialization`: Verifies serde serialization
- `test_argo_config_default`: Default configuration values
- `test_verdict_json_parse_*`: verdict.json parsing (Pass, TestFailure, OOM, unsupported version)
- `test_argo_backend_describe_*`: URL generation with/without base_url

## Code Quality

- No string splicing (S-4 compliant): All manifests built via serde structs
- Proper error handling: Returns BackendError with descriptive messages
- Best-effort log streaming: Failures don't fail the overall run
- Verdict.json fallback: Gracefully degrades to exit-code interpretation

## Integration

The argo backend is exported via `src/backend/mod.rs` and implements the `RemoteBackend` trait defined there. It works with the existing `Verdict`, `BackendError`, `RunHandle`, and `RunSpec` types.

## Future Enhancements

- Pod discovery retry with exponential backoff
- Configurable polling interval in wait()
- Support for workflow retry/resume
- Workflow output artifact collection
