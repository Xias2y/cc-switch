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

### 0.1.5：说明本地化与地址设置

CCS 中文界面下，发布模型目录时会翻译 Fast／Ultrafast 的已知说明，例如
“为对延迟敏感的任务提供最快响应。”；英文界面保留上游文本。缓存仍保存原始元数据，
语言切换后重新应用当前模式，再完整重启 Codex 读取新目录。只修改说明，不更改
档位 ID、可用档位、模型名、工具或上下文标记，也不把显示文案当成实际可用性检测。

当前验证过的配置是：Krill Base URL 填线路裸域名，例如
`https://api.kbsfdxc.net`，**关闭“完整 URL”**，CCS 会拼成 `/v1/responses`。
原生通道也可填 `https://api.kbsfdxc.net/backend-api/codex` 并关闭该开关，
CCS 会追加 `/responses`。只有填入包含 `/responses` 的完整端点地址时才使用该开关；
不要给裸域名打开它。更换接口路径不是启用 Ultrafast 的必要条件。
保留客户端本地代理地址；不要把 Codex 配置中的 `127.0.0.1` 换成上游地址。

### 2026-11-11 实际 HTTP 行为与 WS 限制

后续独立 HTTP 短请求确认：Sol 的 Fast 完成响应返回 `priority`。
请求 `ultrafast` 时，经 CCS 聚合和直连 Krill 原生接口均可完成，但响应仍返回 `default`。
Krill 只读用量接口将这两次请求记为 `gpt-6.1-sol-ultrafast`；请求数及输入／输出 token 数
与本次独立样本完全一致。这证明 Krill 按 Ultrafast 归类和计费，不能单凭响应里的
`default` 判定未命中 Ultrafast。与此同时，计费也不能证明上游实际采用了独立加速：
三轮相同输出对照中 Ultrafast 与 Fast 的正文输出速度接近，尚未证实额外提速。

三个公开线路的 WS 握手均返回 `426`，错误正文明确指出当前账号最多 5 个并发 WS
会话、名额已满。该次错误应解释为连接名额限制，不代表线路不支持 WS。
空闲连接关闭并释放名额后才能继续验证 WS 下的档位；CCS 的正常 HTTP 回退保留。

旧版验收报告仍保留为当时记录：[0.1.4 验收报告](validation/krill-speed-tiers-2026-11-11.json)。
本次修复未新增主动探测，不为状态标签生成模型请求。`service_tier` 由客户端选择，
不要为了开启 Ultrafast 添加固定 Body 覆盖，否则会阻止切回 Standard／Fast。

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

### 0.1.5: localized descriptions and endpoint settings

When CCS publishes a model catalog, Chinese CCS UI languages translate known Fast/Ultrafast
descriptions; English keeps the upstream text. The cache retains original metadata. After
changing language, reapply the current mode and fully restart Codex to reload its catalog.
Only descriptions change; tier IDs, offered tiers, models, tools and native context markers
stay intact. Display metadata is not an availability probe.

The verified Krill setup uses a line origin such as `https://api.kbsfdxc.net` with **Full URL
off**; CCS appends `/v1/responses`. A native base such as
`https://api.kbsfdxc.net/backend-api/codex` also keeps Full URL off and gets `/responses`
appended. Use Full URL only when supplying the entire endpoint ending in `/responses`.
Changing paths is not required to request Ultrafast. Keep the local proxy address in Codex.

### Actual HTTP behavior and WS limit on 2026-11-11

Isolated Sol HTTP requests confirmed Fast completed with `priority`. Requests for `ultrafast`
completed with `default`, both through CCS aggregation and directly against Krill's native
endpoint. Krill's read-only usage API classified the two requests as
`gpt-6.1-sol-ultrafast`; request counts and input/output token totals exactly matched these
isolated samples. This confirms Krill's tier classification and billing, not independent
server-side acceleration. A `default` completion alone does not establish that Krill ignored
the requested tier. Three matched-output rounds showed Ultrafast throughput close to Fast;
additional acceleration remains unverified.

All three public lines refused WS with HTTP 426 and an explicit account limit of five
concurrent WS sessions. This was a session-capacity error, not proof of unsupported WS.
Free a connection slot before further WS tier verification. Existing HTTP fallback is retained.

The [original 0.1.4 acceptance report](validation/krill-speed-tiers-2026-11-11.json) remains
a historical record. This fix adds no active capability probing or generated badge requests.
Let the client choose each tier; a fixed Body override would prevent switching back to
Standard or Fast.
