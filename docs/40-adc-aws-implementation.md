# AWS 外部身份 ADC

## 配置与身份输入

顶层 external_account 的 aws1 来源现接入环境凭据、EC2 IMDSv1/v2 和本地 SigV4，复用现有账户登录、STS、可选 IAM 目标及缓存。实现见 [aws.rs](../src/auth/adc/aws.rs)。协议核对 [AIP-4117](https://google.aip.dev/auth/4117)、[AWS SigV4](https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html) 与 [google-auth AWS 实现](https://github.com/googleapis/google-auth-library-python/blob/main/google/auth/aws.py)。

subject_token_type 必须为 urn:ietf:params:aws:token-type:aws4_request，credential_source.environment_id 必须为 aws1；其他 AWS 版本明确拒绝。仅 workload audience，不能混用文件/可执行/证书/format/headers 来源。regional_cred_verification_url 必须为下列固定模板之一：

```text
https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15
https://sts.{region}.amazonaws.com.cn?Action=GetCallerIdentity&Version=2011-06-15
```

本阶段不实现任意服务/任意请求签名、AWS credential_process、共享配置、ECS 容器来源、Web Identity 文件或自定义 supplier。配置只为 Google federation 提供签名主体，不作为通用 AWS SDK。

Environment 启动时将 AWS_REGION（优先）、AWS_DEFAULT_REGION、AWS_ACCESS_KEY_ID、AWS_SECRET_ACCESS_KEY 和可选 AWS_SESSION_TOKEN 快照到 AwsEnvironment；敏感字段用 Secret，Debug 脱敏。账户上下文/provider 持有副本，不修改进程环境。两个 key 字段都非空才整体使用环境凭据；不完整时整体从 metadata 获取，不将半套环境凭据与远端 key 混配。完整但不合法的环境凭据直接拒绝，不悄悄切换机器身份。库 AdcProvider::new 不自动读取环境，使用默认空快照；from_context 消费显式快照。

同一进程的默认 AWS 快照可能让多个 profile 对应同一个云端身份；profile 仍隔离 token 缓存，但不会创造多个 AWS 账户。需要不同 key 的库调用方必须传独立上下文；CLI 不读取 profile 专属 AWS key 文件。

## Metadata 和刷新

| 配置 | 用途 |
| --- | --- |
| region_url | /latest/meta-data/placement/availability-zone，去除末尾单个区域字母得到 region |
| url | /latest/meta-data/iam/security-credentials，先读单个 role，再读取该 role 的 JSON |
| imdsv2_session_token_url | /latest/api/token，配置时先 PUT，TTL header=300，后续 GET 附带 session header |

仅需 region 或 key 时才访问对应 metadata；两者均有环境值时不请求 IMDSv2。部分缺失则先按需获得一次 IMDSv2 token，本次主体生成内共享。配置 v2 但 PUT 失败时不得降级 v1；只有不配置 v2 URL 时使用无 token 的 v1。刷新时重新取 metadata 凭据和 session，不跨 STS 刷新缓存 AWS 凭据。region 缺失且无 region_url、key 缺失且无 url 均明确报错。

metadata URL 只允许 HTTP、上述固定路径、EC2 link-local IPv4/IPv6（169.254.169.254 / fd00:ec2::254），测试允许 loopback；各 metadata 端点必须同源。拒绝 userinfo、query、fragment 和非标准远端端口。即使完整环境可跳过 metadata，提供的 URL 仍先校验。独立客户端禁用代理和重定向，连接 3 秒、每请求 20 秒，不是完整认证操作的统一期限；无自动重试。

响应限 64 KiB，role 仅允许单个受限名称，不能拼接路径或多行角色，单独的 . 和 .. 也拒绝。凭据必需 access/secret；token、Code 和 Expiration 可选，若提供分别检查字符、Success 和 RFC3339 未过期。签名前将读取耗时计入当前时刻，避免只按获取开始时间接受已过期凭据。环境没有 expiry 字段，真实有效性由服务端确认。错误不输出 metadata URL、role、响应原文或密钥。

## 签名、缓存与清理

生成空 body 的 POST GetCallerIdentity 请求：固定 canonical URI=/、排序 query、SHA256 空载荷、按日期/region/sts/aws4_request 派生 HMAC-SHA256 key。签名包含 host、x-amz-date、可选 x-amz-security-token 和 x-goog-cloud-target-resource。后一项绑定目标 audience；与目前 google-auth 在签名后追加该字段的默认路径有所不同，本项目采用其建议的完整性保护，将目标 header 纳入 SignedHeaders。固定 SDK signer 对照也显式将该 header 放入 additional_headers。

签名请求序列化 JSON 后 percent quote，作为 STS subject_token 再经外层表单编码。AWS secret key 不发给 Google；access key、临时 session token 和签名会包含在签名主体中，由 Google 验证。IMDSv2 token 只发 metadata，不进 STS 或 IAM。客户端不直接发送 GetCallerIdentity；Google STS/IAM 最终 token 才用于 Colab 请求。原 PUBLIC_SCOPES/可选 IAM cloud-platform 交换规则保持。

每个 provider 的 STS/最终缓存保持原隔离与到期余量，强制刷新重新取主体。获取凭据/签名/STS/IAM 失败不覆盖有效旧最终 token；本次显式刷新仍报错。可用取消上下文在各 metadata 请求前后和签名前检查，HTTP 内部仍受自身期限。没有 ADC/keyring/metadata 写入或云资源创建；IMDSv2 session 由 AWS 按 TTL 管理，不由客户端撤销。部分敏感缓冲区清零，但不保证 HTTP/JSON 库全部内存副本清零。

doctor 仅报告 external_account_aws_configured 的基本字段存在，online_validity=not_checked；不读 key 内容、不访问 metadata、不计算签名或校验实际身份。

## 验证与未完成项

[10 项 Rust 集成测试](../tests/adc_aws.rs) 覆盖环境/metadata 混合选择、provider 并发缓存隔离、IMDSv2 顺序/header、无 v2 时的 v1、凭据轮换、坏配置/region/role/响应、过期凭据、重定向/超大响应、v2 失败不降级、强刷失败保留缓存、Secret Debug 和预取消。均使用临时文件、合成 key 和 loopback，无真实 AWS/Google 请求。

[独立签名探针](../tools/probe_aws_signing.py) 用 Python 标准库 HMAC 与安装的 google-auth signer 相互核对，当前版本记录在 [三组样本](../research/aws-signing-probe.json)；Rust 单元测试逐项匹配 Authorization、各签名 header 和区域 URL，覆盖有/无 session、公共/中国区域。该探针不加载默认凭据或发送请求，执行方法：

```sh
/path/to/google-colab-cli-venv/bin/python3 -I tools/probe_aws_signing.py --output research/aws-signing-probe.json
```

CLI 差分新增七项 Rust 扩展：login、sessions（DEFAULT_REGION 回退，其他路径检查 REGION 优先）、whoami、IAM account status、metadata、v2失败和离线 doctor。loopback 后端独立按实际签名时间重算 HMAC，核对主体 quote 层、精确 STS form、scope、身份和请求顺序。旧 358 场景输入与分类须保持。真实 AWS/IMDS、GCP federation policy 和 Colab 接受性仍待验收；证书来源、其他 ADC 能力、真实平台与完整业务等价仍未完成。

开发中新增角色点段回归时，已启动的旧构建在无默认特性测试中暴露了 . 被 URL 归一化为父路径的失败；修正后的默认特性专项通过。源码冻结后重新执行两种 Clippy 和无默认特性全套，不用修复前运行作最终证据。

本机发行校验脚本新增优化二进制的 AWS 环境凭据→签名→loopback STS 登录，后端用标准库按实际时间独立重算签名；候选工作流沿用该校验入口，尚未远程执行。

最终验证：默认特性 378 项、无默认特性 376 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和 Python 语法检查通过。完整 CLI 差分 365 个场景为 92 一致、54 保留差异、219 Rust 扩展，零断言失败，旧 358 场景输入与分类不变；业务场景 301 个。合并 help 83 路径无缺失、短 help 无失败，两份报告哈希与当前 debug 一致。研究工具 55 项中 54 通过、1 项 Linux 专属跳过；三组双参考 SigV4 样本由 Rust 单元测试核对。

本机 release 的 12 项打包/运行检查通过，包括 AWS 环境凭据签名→loopback STS、可执行 ADC 过期拒绝、version、别名/联合授权帮助、补全、manifest/SHA256SUMS 和字节一致性。release 哈希与发行记录一致，未推送或发布；真实身份和跨平台尚未验收。
