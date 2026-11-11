# Krill speed tiers

## 中文

0.1.4 修复路由／聚合模式中 Krill 模型的速度档位被第三方目录生成逻辑清空的问题。
CCS 使用该供应商的端点和 API Key 读取 `/backend-api/codex/models`，按精确模型名保留
`service_tiers` 和 `additional_speed_tiers`，再发布到本地模型目录。不会借用官方账号的
速度权限，也不会把 Ultrafast 添加到未声明支持的模型或别名上。

模型能力缓存有效期为 6 小时，按端点、凭据指纹和目录协议版本隔离。切换、编辑或启动时
准备路由／聚合目录，会检查缓存并在需要时读取模型接口。读取失败沿用同一来源的旧缓存；
没有旧缓存时不声明速度档位。缓存只保存模型能力，不保存 API Key。

更新后让 CCS 重新应用路由／聚合配置，再重启 Codex 读取新目录，即可在支持的 Krill
模型上选择 Fast 或 Ultrafast。Fast 的请求参数为 `service_tier: "priority"`，Ultrafast
为 `service_tier: "ultrafast"`。HTTP 和 WebSocket 均保留客户端选择，不强制覆盖每次请求。
如果先前手动设置过 Body 覆盖中的 `service_tier`，需删除该覆盖，才能由客户端自由切换。

实时验收是默认忽略的 `live_krill_speed_tiers_acceptance` 测试，需显式运行。使用独立的
`CC_SWITCH_TEST_HOME`，以及包含 `krill_settings` 的私有 `CCS_WS_LIVE_FIXTURE` 文件。
测试创建内存数据库和随机端口 CCS 代理，检查实际生成的聚合目录，再向 Krill 发送简短
请求；要求完成响应中的 `service_tier` 与请求一致。脱敏结果写入 `CCS_WS_LIVE_REPORT`。
测试不需要官方账号，不读写正在使用的 CCS 数据或 Codex 配置。它会消耗少量 Krill 额度。

## English

0.1.4 restores Krill's declared speed tiers in routing and aggregation mode. CCS reads the
provider's native model catalog with its configured endpoint and API key, then overlays only
`service_tiers` and `additional_speed_tiers` on exact matching models. Tools, context windows,
native history markers and model routing remain unchanged. Official account entitlements,
unknown aliases and unrelated providers do not supply Krill's speed tiers.

Capability metadata is cached for six hours per endpoint, credential fingerprint and catalog
protocol version. Route/stack preparation refreshes stale metadata outside the client-file
write lock. A failed refresh keeps only the same source's existing cache. Credentials are not
stored in the cache. Reapply the route/stack and restart Codex to reload the model picker.
Remove a previous manual `service_tier` Body override to let the client choose each request's
tier. Fast uses `priority`; Ultrafast uses `ultrafast`, over both HTTP and WebSocket.

The ignored `live_krill_speed_tiers_acceptance` test requires explicit authorization, an isolated
`CC_SWITCH_TEST_HOME`, a private `CCS_WS_LIVE_FIXTURE` containing `krill_settings`, and a
`CCS_WS_LIVE_REPORT` output path. It uses an in-memory database and an ephemeral CCS proxy port,
verifies the generated aggregation catalog, and requires completed upstream responses to report
the requested service tier. No official account or active installation is used. Live requests
consume a small amount of Krill quota; the report excludes credentials and response bodies.
