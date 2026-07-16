# MCP tuning capability contract

本文面向 MCP Server 作者，描述 `tuning-agent` 当前实现接受的 V1 capability 协议。MCP 只注入 provider 操作，不获得 episode 状态迁移、Evaluation Intent 冻结/替换、事务提交或最终 verdict 权限。

## Discovery

Server 必须：

1. 支持 newline-delimited JSON-RPC stdio transport；stdout 只能输出协议帧，日志写 stderr。
2. 在 `initialize` 中声明 `tools` 和 `resources` capability，并接受 `notifications/initialized`。
3. 通过 `resources/read` 的固定 URI `tuning://capabilities/v1` 返回唯一一个 JSON text resource。
4. 通过 `tools/list` 发布 manifest 引用的全部 operation tool。

Runtime 会清空子进程环境，只传入配置中显式声明的变量。Server command 必须是绝对路径。

## Manifest

最小 Probe manifest：

```json
{
  "schema_version": 1,
  "provider": {
    "id": "scheduler-observer",
    "version": "1.0.0"
  },
  "capabilities": [
    {
      "id": "scheduler.snapshot",
      "kind": "probe",
      "effect": "read_only",
      "description": "Read a bounded scheduler snapshot",
      "input_schema": {
        "type": "object",
        "additionalProperties": false,
        "properties": {
          "cpu": { "type": "integer", "minimum": 0 }
        }
      },
      "output_schema": { "type": "object" },
      "allowed_phases": ["clean", "experimenting"],
      "limits": {
        "timeout_ms": 5000,
        "max_output_bytes": 65536
      },
      "deterministic": false,
      "idempotent": false,
      "operations": {
        "probe": "scheduler_observe"
      }
    }
  ]
}
```

Manifest 使用 strict decoding，未知字段会使整个 server 加载失败。Runtime 忽略 server 自报的 provider class，并固定为 `Mcp`。最终 ID 由 Runtime 规范化：

```text
raw capability id: scheduler.snapshot
server config id:  scheduler
Registry id:       mcp/scheduler/scheduler.snapshot
provider id:       mcp/scheduler/<manifest-provider-id>
```

`mcp.servers[].allowed_capabilities` 匹配 raw capability ID；全局 `capabilities.allowed_capabilities` 匹配规范化后的 Registry ID。

四类 capability 的安全声明必须精确满足：

| Kind | Effect | Allowed phases | Additional requirement | Operations |
|---|---|---|---|---|
| `probe` | `read_only` | `clean`, `experimenting` | none | `probe` |
| `mutation` | `reversible_mutation` | `clean`, `experimenting` | `idempotent=true` | `prepare`, `apply`, `status`, `verify`, `restore`, `finalize` |
| `measurement` | `read_only` | `commit_pending` | none | `validate`, `open`, `sample`, `close` |
| `comparison` | `pure_computation` | `commit_pending` | `deterministic=true` | `validate`, `compare` |

MCP mutation 还需要管理员为该 server 配置 `allow_mutations=true`。

`input_schema` 只描述 Agent 可选择的 arguments 或 specification。每个 `tools/list.inputSchema` 则描述下文完整的 Runtime wire request；两者不能混为一个 schema。`tools/list.outputSchema` 可省略，但固定 response DTO 始终会被严格校验。

支持的 JSON Schema 关键字包括 `type`、`properties`、`required`、`additionalProperties`、对象/数组/字符串/数值 bounds、`enum`、`const`、`allOf`、`anyOf`、`oneOf` 和 `not`。未知关键字、boolean schema、超过 64 层或 4096 节点的 schema 会在 discovery 阶段被拒绝。

## Common wire values

所有 operation 都由 Runtime 通过 `tools/call` 调用，并要求结构化 JSON result。公共 context 为：

```json
{
  "episode_id": 42,
  "operation_id": "episode-42/probe-1"
}
```

`operation_id` 是幂等键和关联键。Server 必须原样回传 mutation receipt/status 中的 operation ID，不能自行替换。

`FrozenEvaluationIntent` 及其 digest 是 Runtime 内部 authority，不作为 MCP wire input。Measurement/Comparison provider 只能预验证并执行已冻结的 specification；Mutation provider 只能处理 Runtime 发出的 transaction operation。所有 transaction WAL、A/B evidence 和 commit authorization 都由 Runtime 另行绑定同一个 episode intent pin。

## Probe

Request：

```json
{
  "context": { "episode_id": 42, "operation_id": "episode-42/probe-1" },
  "arguments": { "cpu": 3 }
}
```

Response：

```json
{
  "observed_at_ns": 123456789,
  "data": { "runqueue_depth": 2 },
  "warnings": []
}
```

Probe 必须只读。Runtime 会在返回给 Agent 前再次执行 output size 检查。

## Measurement

Measurement specification 在任何 mutation 之前验证并冻结。Operation 流程固定为：

```text
validate(specification)
open(context, specification) -> session
sample(context, session) -> metric batch, repeated by Runtime plan
close(session) -> cleanup receipt, exactly once after a successful open
```

Wire DTO：

```text
validate request   { specification }
validate response  { valid, message? }

open request       { context, specification }
open response      { id, driver_data? }

sample request     { context, session: { id, driver_data } }
sample response    {
                     started_at_ns, ended_at_ns, quality,
                     workload_fingerprint?, metrics, provenance?
                   }

close request      { id, driver_data }
close response     { session_id, cleaned_up, details? }
```

每个 metric 形如：

```json
{
  "value": 12.5,
  "unit": "ms",
  "kind": "gauge"
}
```

`gauge`/`counter` 必须是有限数值，`boolean` 必须是布尔值。A/B 两侧要产生兼容的 metric name/kind/unit schema。要获得可提交 verdict，domain measurement 必须在两侧返回存在且相同的可信 `workload_fingerprint`；缺失或变化会得到 `Inconclusive`。

当前 Registry 只允许 `read_only` Measurement。需要创建外部资源的 managed observation 尚未开放；它必须先具备独立的 session WAL、幂等 status/close 与启动恢复协议。

## Comparison

`validate` DTO 与 Measurement 相同。`compare` request 包含：

```text
{ context, contract_id, specification, baseline, candidate }
```

Response：

```json
{
  "conclusion": "improved",
  "conditions": [
    { "name": "latency_p99", "passed": true, "details": {} }
  ],
  "details": {}
}
```

`conclusion` 可为 `improved`、`not_improved` 或 `inconclusive`。Comparison 是可注入的 Better 策略，但结果只是 policy evidence：固定系统 guardrail、measurement 完整性、workload 可比性以及最终 `EvaluationVerdict` 仍由可信 Runtime 处理。

## Mutation

Mutation provider 的最低契约：

- `prepare` 无副作用，捕获 canonical resource、baseline、desired 和 opaque `driver_data`；
- `apply`、`restore`、`finalize` 按 `operation_id` 幂等；
- `status` 能在 apply response 丢失后查询同一 operation；
- `verify` 必须 read back 实际资源，而不是复述请求；
- `restore` 只恢复 `prepare` 捕获的 baseline；
- `finalize` 只是无系统状态副作用的 acknowledgement，在中央 commit seal 前不得删除 rollback material。

主要 DTO：

```text
prepare request    { context, arguments }
prepare response   { resource, baseline: { value }, desired: { value }, driver_data? }

apply request      { operation_id, prepared }
apply response     { operation_id, state, observed?: { value }, driver_data? }

status request     { operation_id }
status response    { operation_id, state, observed?: { value }, driver_data? }

verify request     { operation_id, prepared, expected }
verify response    { matched, observed?: { value }, details? }

restore/finalize request and response use the same shapes as apply.
```

Mutation state 可为 `not_applied`、`applied`、`restored`、`finalized` 或 `unknown`。模糊结果必须返回 `unknown`，不能猜测成功。Runtime 会为 baseline/desired 计算 digest、强制本地 provider pin，并将 remote resource 放入 provider namespace。

Server 重启后仍必须能依据持久化的 `prepared.driver_data` 执行 verify/restore。若 provider version 或 manifest digest 变化，pending transaction 会因 pin mismatch 停止自动恢复并阻止新的 activation。

## Deployment boundary

Manifest 是协议和授权输入，不是进程 sandbox。Mutation Server 应使用专用 OS 用户、最小 Linux capability、受限 filesystem/cgroup 和可审计发布流程。不要让两个 provider 对同一底层资源声明不同的 `ResourceKey`；Runtime 无法自动识别跨 provider alias。
