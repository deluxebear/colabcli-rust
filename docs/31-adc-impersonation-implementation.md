# ADC IAM impersonation

更新日期：2026-11-17。新增 impersonated_service_account ADC。该路径用来源凭据获取 IAM 目标 token，不改变本地账户的计算/Drive 绑定，也不自动授予 IAM 权限。Google 真实授权及 Colab 对目标身份的接受情况仍未验收。

## 协议和输入

[Google IAM Credentials 协议](https://docs.cloud.google.com/iam/docs/reference/credentials/rest/v1/projects.serviceAccounts/generateAccessToken) 规定向 generateAccessToken POST 源 Bearer、scope、可选 delegates 与 lifetime，返回 accessToken 和 RFC3339 expireTime。调用者需要目标上的 getAccessToken 权限，delegates 链要求逐级相应权限。本实现只请求 token，不修改 IAM policy。

```sh
colab account add target --adc-file /absolute/path/impersonated-adc.json
colab account login target
colab -a target sessions --json
colab -a target whoami --json
colab account status --json
```

文件发现顺序和 64 KiB 上限沿用 [ADC 模块](30-adc-service-account-implementation.md)。type 必须为 impersonated_service_account；必须有 service_account_impersonation_url 和内嵌 source_credentials。来源仅接受 authorized_user 或 service_account，分别沿用已有用户刷新和 PKCS#8 RSA/JWT 交换；递归 impersonation、external_account、外部可执行来源与 metadata 均明确拒绝，不执行 credential_source。

[impersonation.rs](../src/auth/adc/impersonation.rs) 只接受 Google iamcredentials.googleapis.com HTTPS 端点，或测试用 loopback HTTP(S)。禁止 query、fragment、URL 认证信息和非标准 Google 端口。路径必须为 /v1/projects/-/serviceAccounts/{email_or_unique_id}:generateAccessToken；URL 编码的目标先解码再检查，拒绝斜线、控制字符和非法标识，不能通过 %2F 等扩大资源路径。

delegates 可缺省、为 null 或空数组，均表示直接调用且省略该请求字段；非空时保留给定顺序，逐项要求 projects/-/serviceAccounts/{email_or_unique_id}。不接受实际 project ID 替代 -。目标 scope 固定为 PUBLIC_SCOPES，lifetime 固定为 3600s；不把 JSON 任意字段透传到请求，不配置超过一小时的组织策略例外。quota_project_id 仍未用于额外请求头。

## 缓存、状态、失败与清理

每个 AdcProvider 持有自己的源和目标缓存，同一实例的完整交换由原互斥锁串行；不同账户不共享来源，也不修改进程环境。普通调用在目标剩余超过 60 秒时复用目标，即使来源已经过期也不额外请求它；需要目标刷新时才检查/刷新来源。whoami 强制刷新目标，但仍可复用有效来源。

来源刷新成功后在内存保留其新 token/轮换 refresh token；随后 IAM 失败不会回滚这个已成功的来源更新。目标只在 IAM 完整成功后替换。强制目标刷新失败时返回错误，普通调用之后仍可复用尚有效的旧目标；过期目标不作为成功返回。不自动重试、不打开授权交互，不以源 token 或另一个账户作为目标请求失败的回退。

两步请求分别沿用 TokenClient 的连接/总期限和禁重定向策略，不构成一个新的跨步骤总期限。IAM 响应最多 64 KiB，使用零化缓冲区；必须有非空、无空白/控制字符的 accessToken，且 expireTime 可解析并晚于当前时间。未知字段丢弃，错误仅显示类型/HTTP 状态，不显示 Bearer、JWT、原始 body 或带凭据 URL。

源和目标 token 不写入 ADC 文件、OAuth 文件或 keyring；内嵌用户来源的轮换仅在 provider 生命周期内保留，与既有只读 ADC 约定一致。后续 CLI 调用重新加载源文件；外部来源的持久轮换由该凭据的管理方负责。命令不创建/释放 VM，无远端资源需清理；只读会话同步仍由既有 SessionService 负责。whoami 查询目标 token，ADC 身份不回填 OAuth 元数据。

ext doctor 仅报告 URL 非空且来源类型受支持时的 impersonated_service_account_configured；不验证 URI、来源完整性、权限或有效 token，不调用 IAM。ext version 增加 impersonated_service_account_adc 能力项。

## 验证矩阵

| 来源/身份 | 本地加载与协议 | token 获取 | 实际 scope/Colab 接受 |
| --- | --- | --- | --- |
| authorized_user | 已有刷新/账户隔离测试 | loopback 通过 | 真实 ADC 未验收 |
| service_account | 临时 RSA 密钥、独立验签 | loopback JWT bearer 通过 | 真实 ADC 未验收 |
| impersonated_service_account | 受支持内嵌来源、资源/delegates 校验与 JSON 交换 | loopback IAM 通过 | 真实 IAM 权限、scope 及 Colab 接受均未知 |
| external / metadata / 递归 impersonation | 未实现，拒绝 | 未实现 | 未知 |

新增七项 Rust 测试，覆盖双账户并发及各自目标缓存、delegates/编码目标、来源刷新与轮换后 IAM 失败恢复、RSA 来源真实签名与独立验签、错误来源/端点静态拒绝、IAM 重定向/超大/无效响应脱敏、来源失败不调用 IAM，以及 login/sessions/whoami/account status 完整 CLI。源文件、active/绑定、OAuth 文件与进程 env 的隔离分别核对。

CLI 差分新增六项 Rust 扩展场景，观察源 OAuth、IAM 请求及后续目标身份；IAM 失败时不请求 Colab，doctor 完全离线。既有官方启动器仍禁止真实 keyring、浏览器与非 loopback 网络；没有把合成 IAM 结果视为真实 Google 接受证据。

本阶段结束时仍待完成 external/metadata 等 ADC 来源、update/发行渠道、合并版 help 与完整线上/平台验收；原生更新的后续实现见 [32](32-native-update-and-release-implementation.md)。任务重试、长期保活、自动重建和检查点继续由脚本或外部调度器负责。

最终验证：默认特性 294 项、无默认特性 292 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 287 个场景为 92 一致、53 保留差异、142 Rust 扩展，无断言失败，原有 281 场景分类不变；root/account/drive/ext 清单一致，报告二进制哈希与当前 debug 文件相符。

后续 [52](52-adc-quota-project-implementation.md) 已接入 ADC quota_project_id / GOOGLE_CLOUD_QUOTA_PROJECT 的请求元数据，区分 IAM 来源与目标、保留 metadata 请求隔离。本篇此前的配额项目限制保留为当时范围；实际项目权限与服务接受性仍未验收。
