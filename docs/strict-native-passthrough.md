# 严格原生透传 / Strict native passthrough

## 中文

从 0.1.3 起，官方账号和名为 `Krill`（不区分大小写）的供应商，其原生 GPT Responses 路径默认严格透传，不需要另开开关。显式配置为 Chat／Anthropic 转换，或关闭原生压缩的第三方路径，继续使用原有兼容逻辑。

- HTTP 与 WebSocket 保留客户端提交的消息、推理、压缩项、工具调用与结果，以及条目 ID、密文和调用配对。CCS 不主动清理这些历史，也不因上游拒绝而删除推理、替换压缩或省略加密片段后重试。
- 严格路径保留原生响应事件，不补写迟到的工具参数。工具参数由上游原样提供；其他供应商保留原来的兼容修复。
- 请求覆盖配置不能改写 `input`、`instructions`、`tools`、`previous_response_id` 或 `include`，发生冲突时本地明确报错。
- 模型别名解析、认证隔离和传输头处理仍然必要；HTTP／WS 的封装及无损 zstd 压缩不属于上下文删减。因此“严格透传”指原生上下文和状态保持，不是整个网络报文字节不变。
- Krill 原生目录中，与当前官方同名模型的压缩标记、工具模式和子代理协议一致时，保留官方压缩标记，避免仅因换供应商就先压缩。其他模型与转换路径继续按原 CCS 规则分组。标记相同不能使不兼容密文变得兼容。
- 换上游仍通过重连续接；目标不接受原生状态时返回错误，保留原历史。状态来源仅作观察，不因供应商或账号不同就预先拦截。
- WS 不可用时仍可回退 HTTP。zstd 被拒绝时仍可用相同 JSON 内容重发；这些回退不允许删改上下文。

此功能保证 CCS 不为切换成功而静默删改上述原生状态，不保证任意上游可互读。官方产生的压缩项曾在指定模型和账号组合下被 Krill 原样接受；Krill 产生的压缩／推理项也出现过官方拒绝。后者在严格模式下应显示真实失败，切回能读取它的来源继续，不通过删项伪装成功。

正常由 Codex 触发的上下文压缩仍然存在。此版本不包含历史重建、摘要恢复、主动探测、损失通知或对 Codex 客户端的修改。旧版本已经生成的 CCS 摘要封装也不会在严格路径自动展开；不兼容时同样保留原样报错。

## English

Starting with 0.1.3, official accounts and providers named `Krill` (case-insensitive) use strict passthrough for native GPT Responses, without an extra switch. Explicit Chat/Anthropic conversion and third-party routes with native compaction disabled retain their existing compatibility behavior.

HTTP and WebSocket preserve submitted messages, reasoning, compaction items, tool calls/results, IDs, ciphertext and call pairing. Strict routes neither scrub this history nor retry by dropping reasoning, substituting compaction or omitting encrypted parts. Native response events also bypass late-argument repair. Other providers retain their existing repairs.

Request overrides that change `input`, `instructions`, `tools`, `previous_response_id` or `include` are rejected locally. Routing aliases, authentication isolation, transport headers, protocol envelopes and lossless zstd compression still operate; strict passthrough refers to context/state preservation, not identical network bytes.

Krill native entries retain their native compaction marker when the same official model, marker, tool mode and sub-agent protocol match. Other entries keep existing CCS grouping. Sharing a marker does not make incompatible ciphertext readable.

Upstream changes still reconnect. Unreadable native state remains intact and produces an error. Ownership observations do not impose a provider/account admission rule. HTTP fallback and zstd rejection fallback remain available without changing context.

This prevents silent context deletion by CCS; it does not guarantee cross-provider compatibility. Ordinary Codex compaction remains enabled. This release adds no history reconstruction, summary recovery, active probes, loss notifications or client modifications. Legacy CCS summary envelopes are preserved on strict routes too, even if the target rejects them.
