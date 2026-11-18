# ADC 外部身份令牌交换

## 输入和范围

顶层 external_account ADC 接入文件/URL 主体令牌、STS 交换和可选 IAM impersonation；通过现有 account add --configure-only --adc-file 登记，account login 取 token，其余计算命令复用同一 AdcProvider。配置解析与其他 ADC 共用 64 KiB 限制，不修改进程环境或回写 ADC。代码见 [external.rs](../src/auth/adc/external.rs) 与 [adc.rs](../src/auth/adc.rs)。

契约参考 [AIP-4117](https://google.aip.dev/auth/4117) 和 [Google STS token](https://docs.cloud.google.com/iam/docs/reference/sts/rest/v1/TopLevel/token)。支持 workload/workforce audience；subject_token_type 精确接受 jwt、idToken/id_token、saml2 对应 URN。客户端不验主体签名或 claims，由 STS 决定可接受性。配置必须提供 token_url，仅支持 Google 公共 universe；client_id/client_secret 的后续支持见 [70](70-external-account-client-auth.md)。

| 来源或能力 | 当前行为 |
| --- | --- |
| credential_source.file | UTF-8 text 或指定顶层字符串的 JSON；允许投影文件 symlink |
| credential_source.url | HTTPS、loopback HTTP；自定义 headers、query 和相同格式 |
| Azure IMDS | 接受特定 HTTP 地址 169.254.169.254、路径 /metadata/identity/oauth2/token、Metadata:true；未实际访问 Azure |
| workforce_pool_user_project | 数字项目号，只用于 workforce；没有客户端认证时附加 STS options.userProject（编码对齐见 [37](37-adc-workforce-user-implementation.md)） |
| service_account_impersonation_url | STS 后用 IAM 换目标 token，计算请求只用目标身份 |
| service_account_impersonation.token_lifetime_seconds | 600–43200，默认 3600；较长有效期仍取决于远端组织策略 |
| certificate | 后续接入 X.509 workload 主体与 STS/IAM mTLS，见 [71](71-x509-adc-and-mtls.md) |
| AWS | 后续支持 aws1 环境/EC2 与 SigV4，见 [40](40-adc-aws-implementation.md) |
| executable | 后续支持显式 opt-in 的 Unix 非交互来源，见 [39](39-adc-executable-implementation.md) |
| external_account_authorized_user | 后续已接入，见 [37](37-adc-workforce-user-implementation.md) |
| 嵌套 external 来源 | 尚未实现 |
| metadata 自动发现 | 后续已接入，见 [38](38-adc-metadata-implementation.md) |

文件和 URL 同时出现时以文件为准，忽略 URL，采用 AIP 的优先级。相对路径基于 ADC 所在目录，text 去除首尾空白，JSON 字符串保持原值；这些行为不宣称与任意 google-auth 版本逐字节一致。JSON 字段必须存在、为字符串且非全空白。文件必须为普通文件，Unix 使用非阻塞打开以拒绝 FIFO；单次最多读取 64 KiB。文件轮换只在下一次需要 STS 时读取；ADC 配置本身在 provider 首次成功加载后固定。

## 状态与失败路径

每个 provider 独立持有最终 token 和内部 STS token，并由原有 mutex 串行刷新；不同账户不会共用缓存。直接交换使用 Colab 的 PUBLIC_SCOPES；带目标时，STS 使用 cloud-platform，IAM 再申请 PUBLIC_SCOPES。初期 STS 请求不带 Authorization；后续已接入可选 HTTP Basic 客户端认证，身份隔离和 workforce options 规则见 [70](70-external-account-client-auth.md)。来源 URL 的 header 只发给该来源。IAM 目标、delegates 和响应校验抽取到 [impersonation.rs](../src/auth/adc/impersonation.rs)，原用户/服务账户来源保持原协议。

普通请求复用有效最终 token；接近到期才刷新。直接模式强制刷新会重读主体并交换 STS；IAM 模式强制目标刷新可复用有效 STS。STS 或 IAM 失败对本次调用返回错误，不降级为源身份。仍有效的旧最终 token 保留供后续普通调用使用；IAM 失败后已获得的 STS 可在重试时复用。不会把 token 写入 keyring、主体文件、gcloud ADC 或账户配置。

STS 端点限定 https://sts.googleapis.com/v1/token（测试允许 loopback 同路径），拒绝 query、fragment、userinfo 和重定向。URL 来源使用独立 no_proxy 客户端、不跟随重定向；连接 5 秒、请求 20 秒。主体、STS、IAM 各有请求期限，没有新的全流程统一截止时间或自动重试。Header 限 32 项、16 KiB，拒绝大小写重复和会改变路由/报文边界的字段。配置是受信任输入，允许 HTTPS IdP；并非任意 URL 的沙箱。

STS/IAM 响应最多读取 64 KiB。STS 必须返回 Bearer、指定 issued_token_type、非空 token 和正 expires_in，溢出拒绝。错误只输出静态原因或 HTTP 状态，不回显 URL、headers、主体、响应原文。敏感包装和部分临时缓冲区在释放时清零；不承诺 HTTP/JSON 库全部副本内存清零。

无持久事务或远端资源创建。调用结束释放内存缓存；不删除外部主体文件。doctor 只读取 ADC 白名单字段，报告 external_account_configured 表示基本字段存在；不读取主体文件、不请求 URL、不验证 audience/endpoint/真实身份，online_validity 始终 not_checked。

## 验证与剩余缺口

[Rust 测试](../tests/adc_external.rs) 使用临时文件和 loopback HTTP，覆盖精确 STS form、并发缓存/账户隔离、相对路径/file 优先、JSON 投影文件轮换、强制刷新失败保留旧 token、workforce options、URL header 隔离、IAM 失败重试/目标强刷、到期余量、无效配置/文件/FIFO、错误/重定向/超大响应和恢复。诊断表驱动测试验证不存在文件和 URL 不被求值、输出不含私有字段。原 IAM 七项回归也须通过。

CLI 差分新增六个 Rust 扩展场景：login、sessions、URL 来源 whoami、IAM account status、IAM 失败与本地 doctor。只验证合成协议及命令接线，不称为与官方双端等价。先前 332 场景保持输入与分类。真实 STS、IdP/SAML、Azure、IAM 组织策略及 Colab 对外部身份的接受性尚未验证；AWS、可执行来源和其余 ADC 类型仍有缺口，完整设计目标尚未完成。

最终验证：默认特性 336 项、无默认特性 334 项 Rust 测试通过；两种特性 Clippy、fmt 和默认构建通过。完整 CLI 差分 338 个场景为 92 一致、54 保留差异、192 Rust 扩展，零断言失败，旧 332 场景输入和分类不变；业务场景 274 个。合并帮助 83 路径无缺失、短 help 无失败；研究工具 55 项中 54 通过、1 项 Linux 专属跳过。

本机 release 重建及打包后 version、账户别名帮助、联合授权参数和五种补全输出通过；manifest、SHA256SUMS、打包字节一致性与两份 debug 报告哈希均已核对。未推送代码或发布 Release。

后续已接入 metadata ADC 的默认文件缺失发现，见 [38](38-adc-metadata-implementation.md)；不代表真实平台或 mTLS 验收完成。

可执行来源已由后续阶段接入，默认仍不执行，见 [39](39-adc-executable-implementation.md)。

后续 [52](52-adc-quota-project-implementation.md) 已接入 ADC quota_project_id / GOOGLE_CLOUD_QUOTA_PROJECT 的请求元数据，区分 IAM 来源与目标、保留 metadata 请求隔离。本篇此前的配额项目限制保留为当时范围；实际项目权限与服务接受性仍未验收。
