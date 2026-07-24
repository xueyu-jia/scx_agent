#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
cd "$REPO_ROOT"

CONFIG="bench/configs/local_profiles/oe2403sp4_6_6_scx"
OUTPUT_ROOT="bench/results/oe2403sp4_6_6_scx/real_llm/$(date +%Y%m%dT%H%M%S%z)"
UPSTREAM_URL=${OPENAI_COMPAT_UPSTREAM:-https://api.deepseek.com}
GATEWAY_ENDPOINT="127.0.0.1:17002"
RELAY_ENDPOINT="192.168.122.1:17001"
API_KEY="local-test"
MODEL="deepseek-v4-flash"
PROGRESS_INTERVAL=30
PREFLIGHT_ONLY=0
DRY_RUN=0
GROUP_GATE=0
SYSTEMD_SOCKET_PROXYD="/usr/lib/systemd/systemd-socket-proxyd"
GATEWAY_PID=""
RELAY_PID=""
OWN_GATEWAY=0
OWN_RELAY=0
VALIDATION_FAILURES=0

usage() {
  cat <<'EOF'
Usage: bench/scripts/run_oe2403sp4_real_llm.sh [options]

Runs the openEuler A/A noise comparisons followed by the real-LLM measured
comparisons. The output directory must be new or empty.

Options:
  --config PATH       benchmark config directory
  --output PATH       output root (default: timestamped bench/results path)
  --upstream URL      OpenAI-compatible HTTPS API (default: DeepSeek API)
  --gateway HOST:PORT host gateway endpoint (default: 127.0.0.1:17002)
  --relay HOST:PORT   VM-visible relay endpoint (default: 192.168.122.1:17001)
  --progress SECONDS  run.py progress interval (default: 30)
  --preflight-only    check host and one real LLM tool call without running VMs
  --group-gate        run one LATENCY, BATCH, and MIX candidate pair
  --dry-run           generate all 44 runs without starting VMs or calling LLM
                      continue remaining experiment groups after validation failures
  -h, --help          show this help
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "required command is missing: $1"
}

record_validation_failure() {
  VALIDATION_FAILURES=$((VALIDATION_FAILURES + 1))
  printf 'validation failure recorded; continuing remaining experiments (%d total)\n' \
    "$VALIDATION_FAILURES" >&2
}

while (($# > 0)); do
  case "$1" in
    --config)
      (($# >= 2)) || die "--config requires a path"
      CONFIG=$2
      shift 2
      ;;
    --output)
      (($# >= 2)) || die "--output requires a path"
      OUTPUT_ROOT=$2
      shift 2
      ;;
    --upstream)
      (($# >= 2)) || die "--upstream requires a URL"
      UPSTREAM_URL=$2
      shift 2
      ;;
    --gateway)
      (($# >= 2)) || die "--gateway requires HOST:PORT"
      GATEWAY_ENDPOINT=$2
      shift 2
      ;;
    --relay)
      (($# >= 2)) || die "--relay requires HOST:PORT"
      RELAY_ENDPOINT=$2
      shift 2
      ;;
    --progress)
      (($# >= 2)) || die "--progress requires seconds"
      PROGRESS_INTERVAL=$2
      shift 2
      ;;
    --preflight-only)
      PREFLIGHT_ONLY=1
      shift
      ;;
    --group-gate)
      GROUP_GATE=1
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

[[ -d "$CONFIG" ]] || die "config directory does not exist: $CONFIG"
[[ "$PROGRESS_INTERVAL" =~ ^[0-9]+$ ]] || die "--progress must be a non-negative integer"
[[ "$GATEWAY_ENDPOINT" =~ ^[^:]+:[0-9]+$ ]] || die "--gateway must be HOST:PORT"
[[ "$RELAY_ENDPOINT" =~ ^[^:]+:[0-9]+$ ]] || die "--relay must be HOST:PORT"
[[ "$GATEWAY_ENDPOINT" != "$RELAY_ENDPOINT" ]] || die "gateway and relay endpoints must differ"
[[ "$UPSTREAM_URL" == https://* ]] || die "--upstream must be an HTTPS URL"
[[ "$UPSTREAM_URL" != *\?* && "$UPSTREAM_URL" != *\#* ]] || die \
  "--upstream must not contain a query or fragment"
UPSTREAM_URL=${UPSTREAM_URL%/}
((PREFLIGHT_ONLY == 0 || DRY_RUN == 0)) || die \
  "--preflight-only and --dry-run cannot be combined"
((PREFLIGHT_ONLY == 0 || GROUP_GATE == 0)) || die \
  "--preflight-only and --group-gate cannot be combined"

require_command curl
require_command jq
require_command python3
require_command systemd-socket-activate
require_command setsid
require_command realpath
require_command sha256sum
[[ -x "$SYSTEMD_SOCKET_PROXYD" ]] || die "required executable is missing: $SYSTEMD_SOCKET_PROXYD"

mkdir -p "$OUTPUT_ROOT"
if find "$OUTPUT_ROOT" -mindepth 1 -print -quit | grep -q .; then
  die "output directory is not empty: $OUTPUT_ROOT"
fi

mapfile -t CONFIG_VALUES < <(python3 - "$CONFIG" <<'PY'
import sys
from pathlib import Path
from urllib.parse import urlparse

from bench.core.config import load_config_data

config = load_config_data(Path(sys.argv[1]))
names = (
    "scx_agent_classed_llm_latency",
    "scx_agent_classed_llm_batch",
    "scx_agent_classed_llm_mixed",
)
values = []
for name in names:
    scheduler = config["schedulers"][name]
    env = scheduler.get("env", {})
    base_url = env.get("SCX_REAL_LLM_BASE_URL", "http://192.168.122.1:17001")
    model = env.get("SCX_REAL_LLM_MODEL", "deepseek-v4-flash")
    api_key = env.get("SCX_REAL_LLM_API_KEY", "local-test")
    parsed = urlparse(base_url)
    if parsed.scheme not in {"http", "https"} or not parsed.hostname or parsed.path not in {"", "/"}:
        raise SystemExit(f"invalid SCX_REAL_LLM_BASE_URL for {name}: {base_url}")
    values.extend((base_url.rstrip("/"), model, api_key))
values.append(str(config["libvirt"]["kernel"]))
print("\n".join(values))
PY
)
(( ${#CONFIG_VALUES[@]} == 10 )) || die "could not read the LLM and kernel configurations"

for ((index = 0; index < 9; index += 3)); do
  [[ "${CONFIG_VALUES[index]}" == "${CONFIG_VALUES[0]}" ]] || die "LLM base URLs differ between scheduler variants"
  [[ "${CONFIG_VALUES[index + 1]}" == "$MODEL" ]] || die "scheduler model must be $MODEL"
  [[ "${CONFIG_VALUES[index + 2]}" == "$API_KEY" ]] || die "scheduler API key must match the script gateway token"
done

KERNEL_IMAGE=${CONFIG_VALUES[9]}
[[ -f "$KERNEL_IMAGE" ]] || die "configured kernel image is missing: $KERNEL_IMAGE"

RELAY_URL="${CONFIG_VALUES[0]}"
[[ "$RELAY_URL" == "http://${RELAY_ENDPOINT}" ]] || die \
  "config relay ${RELAY_URL} does not match requested relay http://${RELAY_ENDPOINT}"

probe_models() {
  local endpoint=$1
  curl -fsS --connect-timeout 15 --max-time 30 "http://${endpoint}/v1/models" \
    -H "Authorization: Bearer ${API_KEY}" 2>/dev/null |
    jq -e --arg model "$MODEL" '.data | any(.[]; .id == $model)' >/dev/null
}

wait_for_models() {
  local endpoint=$1
  for _ in {1..10}; do
    probe_models "$endpoint" && return 0
    sleep 0.5
  done
  return 1
}

probe_tool_call() {
  local endpoint=$1
  local response
  response=$(
    jq -nc --arg model "$MODEL" '{
      model: $model,
      messages: [{role: "user", content: "You must call preflight_ping now; do not answer with text."}],
      tools: [{type: "function", function: {
        name: "preflight_ping",
        description: "Validate OpenAI-compatible tool calling.",
        parameters: {type: "object", properties: {}, additionalProperties: false}
      }}],
      tool_choice: "auto",
      stream: false
    }' |
      curl -fsS --max-time 120 "http://${endpoint}/v1/chat/completions" \
        -H "Authorization: Bearer ${API_KEY}" \
        -H 'Content-Type: application/json' \
        --data-binary @-
  ) || return 1
  jq -e '
    .choices[0].message.tool_calls |
    any(.[]; .type == "function" and .function.name == "preflight_ping")
  ' >/dev/null <<<"$response"
}

stop_process_group() {
  local pid=$1
  kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}

cleanup() {
  local status=$?
  if [[ "$OWN_RELAY" == 1 && -n "$RELAY_PID" ]]; then
    stop_process_group "$RELAY_PID"
  fi
  if [[ "$OWN_GATEWAY" == 1 && -n "$GATEWAY_PID" ]]; then
    stop_process_group "$GATEWAY_PID"
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for executable in \
  schedule/scx_agent_classed/target/release/scx_agent_classed \
  schedule/scx_agent_classed_mcp/target/release/scx_agent_classed_mcp \
  tuning_agent/target/release/tuning-agent \
  bench/workloads/bin/perf \
  bench/workloads/bin/schbench \
  bench/workloads/bin/stress-ng
do
  [[ -x "$executable" ]] || die "required benchmark executable is missing: $executable"
done

python3 -m bench.env verify --config "$CONFIG"
python3 -m bench.env isolation status --config "$CONFIG" |
  tee "$OUTPUT_ROOT/isolation-status.txt"

if ((DRY_RUN)); then
  printf 'dry run: skipping the LLM gateway, relay and network preflight\n'
else
  UPSTREAM_API_KEY=${DEEPSEEK_API_KEY:-${OPENAI_COMPAT_UPSTREAM_API_KEY:-}}
  [[ -n "$UPSTREAM_API_KEY" ]] || die \
    "DEEPSEEK_API_KEY is not set; export it in this shell before running the experiment"

  if probe_models "$GATEWAY_ENDPOINT"; then
    printf 'OpenAI-compatible gateway is already ready: http://%s (%s)\n' \
      "$GATEWAY_ENDPOINT" "$MODEL"
  else
    printf 'starting OpenAI-compatible gateway %s -> %s\n' \
      "$GATEWAY_ENDPOINT" "$UPSTREAM_URL"
    OPENAI_COMPAT_UPSTREAM_API_KEY="$UPSTREAM_API_KEY" \
      OPENAI_COMPAT_PROXY_TOKEN="$API_KEY" \
      setsid python3 bench/integrations/tuning_agent/openai_compat_gateway.py \
        --host "${GATEWAY_ENDPOINT%:*}" \
        --port "${GATEWAY_ENDPOINT##*:}" \
        --upstream "$UPSTREAM_URL" \
        --strip-v1 \
        >"$OUTPUT_ROOT/openai-gateway.log" 2>&1 &
    GATEWAY_PID=$!
    OWN_GATEWAY=1
    wait_for_models "$GATEWAY_ENDPOINT" || die \
      "OpenAI-compatible gateway did not become ready"
  fi

  if probe_models "$RELAY_ENDPOINT"; then
    printf 'LLM relay is already ready: http://%s (%s)\n' "$RELAY_ENDPOINT" "$MODEL"
  else
    printf 'starting relay %s -> %s\n' "$RELAY_ENDPOINT" "$GATEWAY_ENDPOINT"
    setsid systemd-socket-activate \
      -l "$RELAY_ENDPOINT" \
      "$SYSTEMD_SOCKET_PROXYD" \
      --connections-max=16 \
      "$GATEWAY_ENDPOINT" \
      >"$OUTPUT_ROOT/llm-relay.log" 2>&1 &
    RELAY_PID=$!
    OWN_RELAY=1
    wait_for_models "$RELAY_ENDPOINT" || die "LLM relay did not become ready"
  fi

  printf 'validating one real OpenAI-compatible tool call through the VM relay\n'
  probe_tool_call "$RELAY_ENDPOINT" || die "LLM tool-call preflight failed"
  unset UPSTREAM_API_KEY DEEPSEEK_API_KEY OPENAI_COMPAT_UPSTREAM_API_KEY
fi

if ((PREFLIGHT_ONLY)); then
  printf 'preflight passed; no benchmark VM was started\n'
  exit 0
fi

GIT_COMMIT=$(git rev-parse HEAD)
if [[ -n "$(git status --porcelain)" ]]; then
  GIT_DIRTY=true
else
  GIT_DIRTY=false
fi
CONFIG_SHA256=$(
  find "$CONFIG" -maxdepth 1 -type f -name '*.config' -print0 |
    sort -z |
    xargs -0 sha256sum |
    sha256sum |
    cut -d' ' -f1
)
KERNEL_SHA256=$(sha256sum "$KERNEL_IMAGE" | cut -d' ' -f1)
SCHEDULER_SHA256=$(sha256sum schedule/scx_agent_classed/target/release/scx_agent_classed | cut -d' ' -f1)
MCP_SHA256=$(sha256sum schedule/scx_agent_classed_mcp/target/release/scx_agent_classed_mcp | cut -d' ' -f1)
AGENT_SHA256=$(sha256sum tuning_agent/target/release/tuning-agent | cut -d' ' -f1)
jq -n \
  --arg config "$(realpath "$CONFIG")" \
  --arg output "$(realpath "$OUTPUT_ROOT")" \
  --arg git_commit "$GIT_COMMIT" \
  --argjson git_dirty "$GIT_DIRTY" \
  --arg config_sha256 "$CONFIG_SHA256" \
  --arg kernel_sha256 "$KERNEL_SHA256" \
  --arg scheduler_sha256 "$SCHEDULER_SHA256" \
  --arg mcp_sha256 "$MCP_SHA256" \
  --arg agent_sha256 "$AGENT_SHA256" \
  --arg gateway_sha256 "$(sha256sum bench/integrations/tuning_agent/openai_compat_gateway.py | cut -d' ' -f1)" \
  --argjson dry_run "$DRY_RUN" \
  --arg model "$MODEL" \
  --arg relay "http://${RELAY_ENDPOINT}" \
  --arg gateway "http://${GATEWAY_ENDPOINT}" \
  --arg upstream "$UPSTREAM_URL" \
  --argjson group_gate "$GROUP_GATE" \
  '{schema_version: 1, config: $config, config_sha256: $config_sha256,
    output: $output, git_commit: $git_commit, git_dirty: $git_dirty,
    artifacts: {kernel_sha256: $kernel_sha256, scheduler_sha256: $scheduler_sha256,
                mcp_sha256: $mcp_sha256, tuning_agent_sha256: $agent_sha256,
                openai_gateway_sha256: $gateway_sha256},
    llm: {protocol: "openai-compatible", model: $model, relay: $relay,
          gateway: $gateway, upstream: $upstream},
    order: "alternating", parallel: 1, dry_run: ($dry_run == 1),
    mode: (if $group_gate == 1 then "group_gate" else "full" end),
    experiments: (if $group_gate == 1 then
      ["gate_latency", "gate_batch", "gate_mixed"] else
      ["aa_latency", "aa_batch", "aa_mixed",
       "measured_latency", "measured_batch", "measured_mixed"] end)}' \
  >"$OUTPUT_ROOT/experiment-manifest.json"

COMMAND_LOG="$OUTPUT_ROOT/commands.log"
touch "$COMMAND_LOG"

run_plan() {
  local plan=$1
  local baseline=$2
  local candidate=$3
  local baseline_treatment=${4:-}
  local candidate_treatment=${5:-}
  local output=$6

  local -a command=(
    python3 bench/scripts/run.py
    --config "$CONFIG"
    --plan "$plan"
    --baseline "$baseline"
    --candidate "$candidate"
    --order alternating
    --parallel 1
    --progress-interval "$PROGRESS_INTERVAL"
    --output "$output"
  )
  if [[ -n "$baseline_treatment" ]]; then
    command+=(--baseline-treatment "$baseline_treatment")
    command+=(--candidate-treatment "$candidate_treatment")
  fi
  if ((DRY_RUN)); then
    command+=(--dry-run)
  fi

  printf '\n$' | tee -a "$COMMAND_LOG"
  printf ' %q' "${command[@]}" | tee -a "$COMMAND_LOG"
  printf '\n' | tee -a "$COMMAND_LOG"
  set +e
  "${command[@]}" 2>&1 | tee -a "$COMMAND_LOG"
  local status=${PIPESTATUS[0]}
  set -e
  ((status == 0)) || die "run.py failed for $plan: $output"
}

validate_pass_results() {
  local output=$1
  local expected_runs=$2
  local expected_results=$((expected_runs * 2))
  local -a results=()
  mapfile -d '' results < <(find "$output/runs" -name result.json -print0 | sort -z)
  if (( ${#results[@]} != expected_results )); then
    printf 'validation failed: %s produced %d result files; expected %d\n' \
      "$output" "${#results[@]}" "$expected_results" >&2
    return 1
  fi
  local result
  local expected_status=PASS
  if ((DRY_RUN)); then
    expected_status=DRY_RUN
  fi
  for result in "${results[@]}"; do
    if ! jq -e --arg status "$expected_status" '.status == $status' "$result" >/dev/null; then
      printf 'validation failed: unexpected run status in %s; expected %s\n' \
        "$result" "$expected_status" >&2
      return 1
    fi
  done
}

validate_llm_results() {
  local output=$1
  local candidate_label=$2
  local expected_runs=$3
  if ((DRY_RUN)); then
    return
  fi
  local candidate_dir="$output/runs/$candidate_label"
  local -a verifications=()
  mapfile -d '' verifications < <(
    find "$candidate_dir" -path '*/real_llm/perf-verification.json' -print0 | sort -z
  )
  if (( ${#verifications[@]} != expected_runs )); then
    printf 'validation failed: %s contains %d LLM verification files; expected %d\n' \
      "$output" "${#verifications[@]}" "$expected_runs" >&2
    return 1
  fi

  local verification outcome
  for verification in "${verifications[@]}"; do
    if ! jq -e --arg model "$MODEL" '
      .model == $model and
      .classification_mode == "group" and
      .audit_episode_count == 1 and
      (.episodes | length == 1) and
      (.activation_comms | length) as $targets |
      .mutation_count == $targets and
      .episodes[0].mutation_count == $targets and
      .episodes[0].phase == "committed" and
      .episodes[0].verdict == "improved"
    ' "$verification" >/dev/null; then
      printf 'validation failed: LLM verification failed: %s\n' "$verification" >&2
      return 1
    fi

    outcome=${verification%/real_llm/perf-verification.json}/treatment/outcome.json
    if [[ ! -f "$outcome" ]]; then
      printf 'validation failed: missing treatment outcome: %s\n' "$outcome" >&2
      return 1
    fi
    if ! jq -e '
      .version == 2 and .disposition == "proceed" and
      .details.quiet_state.episode_count == 1
    ' "$outcome" >/dev/null; then
      printf 'validation failed: LLM treatment did not reach quiet state: %s\n' \
        "$outcome" >&2
      return 1
    fi
  done
}

if ((GROUP_GATE)); then
  printf 'starting one-pair group activation gates\n'
  run_plan single_latency_candidate_gate default scx_agent_classed_llm_latency \
    llm_latency_control llm_latency_classify "$OUTPUT_ROOT/gate_latency"
  validate_pass_results "$OUTPUT_ROOT/gate_latency" 1 || record_validation_failure
  validate_llm_results "$OUTPUT_ROOT/gate_latency" \
    scx_agent_classed_llm_latency__llm_latency_classify 1 || record_validation_failure

  run_plan single_batch_candidate_gate default scx_agent_classed_llm_batch \
    llm_batch_control llm_batch_classify "$OUTPUT_ROOT/gate_batch"
  validate_pass_results "$OUTPUT_ROOT/gate_batch" 1 || record_validation_failure
  validate_llm_results "$OUTPUT_ROOT/gate_batch" \
    scx_agent_classed_llm_batch__llm_batch_classify 1 || record_validation_failure

  run_plan mixed_candidate_gate default scx_agent_classed_llm_mixed \
    llm_mixed_control llm_mixed_classify "$OUTPUT_ROOT/gate_mixed"
  validate_pass_results "$OUTPUT_ROOT/gate_mixed" 1 || record_validation_failure
  validate_llm_results "$OUTPUT_ROOT/gate_mixed" \
    scx_agent_classed_llm_mixed__llm_mixed_classify 1 || record_validation_failure
  if ((VALIDATION_FAILURES)); then
    printf '\ngroup activation gates completed with validation failures\n' >&2
  elif ((DRY_RUN)); then
    printf '\ngroup activation gate dry run passed\n'
  else
    printf '\ngroup activation gates passed\n'
  fi
  printf 'results: %s\nreports:\n' "$(realpath "$OUTPUT_ROOT")"
  find "$OUTPUT_ROOT" -path '*/analysis/report.html' -print | sort
  if ((VALIDATION_FAILURES)); then
    exit 1
  fi
  exit 0
fi

printf 'starting A/A EEVDF noise comparisons\n'
run_plan single_latency_core_priming alt_default default "" "" "$OUTPUT_ROOT/aa_latency"
validate_pass_results "$OUTPUT_ROOT/aa_latency" 2 || record_validation_failure

run_plan single_batch_core_priming alt_default default "" "" "$OUTPUT_ROOT/aa_batch"
validate_pass_results "$OUTPUT_ROOT/aa_batch" 2 || record_validation_failure

run_plan mixed_fixed_rps_core_priming alt_default default "" "" "$OUTPUT_ROOT/aa_mixed"
validate_pass_results "$OUTPUT_ROOT/aa_mixed" 2 || record_validation_failure

printf 'starting formal real-LLM comparisons\n'
run_plan single_latency_core_measured default scx_agent_classed_llm_latency \
  llm_latency_control llm_latency_classify "$OUTPUT_ROOT/measured_latency"
validate_pass_results "$OUTPUT_ROOT/measured_latency" 8 || record_validation_failure
validate_llm_results "$OUTPUT_ROOT/measured_latency" \
  scx_agent_classed_llm_latency__llm_latency_classify 8 || record_validation_failure

run_plan single_batch_core_measured default scx_agent_classed_llm_batch \
  llm_batch_control llm_batch_classify "$OUTPUT_ROOT/measured_batch"
validate_pass_results "$OUTPUT_ROOT/measured_batch" 4 || record_validation_failure
validate_llm_results "$OUTPUT_ROOT/measured_batch" \
  scx_agent_classed_llm_batch__llm_batch_classify 4 || record_validation_failure

run_plan mixed_fixed_rps_core_measured default scx_agent_classed_llm_mixed \
  llm_mixed_control llm_mixed_classify "$OUTPUT_ROOT/measured_mixed"
validate_pass_results "$OUTPUT_ROOT/measured_mixed" 4 || record_validation_failure
validate_llm_results "$OUTPUT_ROOT/measured_mixed" \
  scx_agent_classed_llm_mixed__llm_mixed_classify 4 || record_validation_failure

if ((VALIDATION_FAILURES)); then
  printf '\nall experiments completed with %d validation failure(s)\n' \
    "$VALIDATION_FAILURES" >&2
elif ((DRY_RUN)); then
  printf '\nall experiment dry runs passed\nresults: %s\n' "$(realpath "$OUTPUT_ROOT")"
else
  printf '\nall experiments passed\nresults: %s\n' "$(realpath "$OUTPUT_ROOT")"
fi
printf 'reports:\n'
find "$OUTPUT_ROOT" -path '*/analysis/report.html' -print | sort
if ((VALIDATION_FAILURES)); then
  exit 1
fi
