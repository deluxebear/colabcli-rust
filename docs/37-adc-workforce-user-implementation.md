# Workforce 用户 ADC 刷新

## 入口与协议

顶层 external_account_authorized_user JSON 现在可用于现有账户 ADC 登录及计算命令。通过 account add --configure-only --adc-file 登记，account login 验证取 token；无需新增命令或修改进程环境。代码见 [workforce.rs](../src/auth/adc/workforce.rs)。

协议分别核对 Google 的 [Workforce 用户凭据实现](https://github.com/googleapis/google-auth-library-python/blob/main/google/auth/external_account_authorized_user.py)、[STS refresh](https://github.com/googleapis/google-auth-library-python/blob/main/google/oauth2/sts.py) 和 [客户端认证](https://github.com/googleapis/google-auth-library-python/blob/main/google/oauth2/utils.py)。请求只带 grant_type=refresh_token 与 refresh_token 两个表单字段，client_id/client_secret 用 HTTP Basic 发送；不添加 audience、scope 或 subject_token，不把这种身份套用到普通 authorized_user 的请求体认证。

可配置有效 token + expiry 供只读缓存使用，也可提供完整 refresh_token、client_id、client_secret、token_url 以便刷新。可选 audience 若提供，必须为 workforce provider；其他 universe 拒绝。client_id 不允许冒号或控制字符，避免 Basic 用户名分隔歧义。secret 可以含冒号及非 ASCII，测试按 UTF-8 解码验证。

只支持 Google STS 的 https://sts.googleapis.com/v1/oauth/token，测试可用 loopback 同路径；拒绝 query、fragment、userinfo、不安全远端 HTTP 和重定向。使用现有 TokenClient 的连接 5 秒、请求 20 秒期限，无自动重试。配置受 ADC 共用 64 KiB 限制，响应同样最多读 64 KiB。token_info_url/revoke_url 不会自动访问，不执行授权发现或注销；初始用户授权仍由外部 gcloud 等工具取得。

## 状态和清理责任

每个 AdcProvider 的 mutex 管理单独的 Workforce 状态，跨账户不共享客户端 secret、刷新令牌或缓存。有效缓存保留 60 秒到期余量；显式 refresh 总是尝试刷新，无完整刷新字段时返回需要授权，不悄悄返回旧 token。仅有缓存的配置到期后同样需要重新授权。

响应必须有非空、无空白/控制字符的 access_token、正整数 expires_in；日期溢出拒绝。token_type 若提供必须是 Bearer；缺省可接受。新 refresh_token 若提供必须非空，缺省则保留旧值。所有字段和到期时间验证后才一起采用新状态；失败不能覆盖旧 access token 或轮换源。仍有效的旧缓存可供后续普通请求使用，本次显式刷新失败仍返回错误。

刷新不请求扩大既有授权范围。当前不重新解释 scopes、quota_project_id 或 trust_boundary 元数据，也未实现相应 quota header/边界服务；不能把已支持 token 获取解释为完整 google-auth 对象等价。此类型尚不能作为 impersonated_service_account 的嵌入来源。不会回写 gcloud 文件、保存 token 到钥匙串或创建远端资源；刷新令牌轮换只保留在 provider 内存，进程结束即丢失，下一进程仍读取原文件。若提供方撤销旧刷新令牌，需由原凭据管理工具更新文件；本项目不跨进程持久化这种轮换。

错误只含静态原因或 HTTP 状态，不回显 Basic 值、secret、刷新令牌、URI 或响应正文。Secret 包装和部分缓冲区会清零，不承诺 HTTP 库内部全部副本清零。doctor 只看白名单字段，external_account_authorized_user_configured 表示缓存字段或刷新字段基本存在，online_validity=not_checked；不联网、不校验实际到期时间或访问 introspection/revoke URL。

## 相邻协议兼容

上一阶段 external_account 的主体类型新增 id_token 拼写，与已有 idToken 一起原样发送。[STS 文档](https://docs.cloud.google.com/iam/docs/reference/sts/rest/v1/TopLevel/token) 的不同字段描述出现两种写法；客户端不擅自改写请求值，由服务端最终判断。新增 loopback 测试逐一检查两者。

同时对齐 Workforce external_account 的 options.userProject 编码：序列化 JSON 后先 percent quote，再进行外层表单编码，与 google-auth STS 路径一致。测试检查表单解码后仍保留 quote 层，再解码为原 JSON；不把合成用例视为真实 STS 接受性证明。

## 测试与边界

[七项 Workforce 测试](../tests/adc_workforce.rs) 覆盖并发缓存和身份隔离、精确 Basic/form、非 ASCII secret、内存轮换、强制刷新失败保留状态、token-only/到期余量、错误响应不采用轮换、无效配置本地拒绝、HTTP 错误/重定向/超大响应和恢复。原用户、服务账户、IAM 与 external_account 测试继续回归；诊断表驱动测试新增完整刷新字段、缓存字段和缺字段三种情况，均不求值远端 URL。

CLI 差分增加六项 Rust 扩展：login、sessions、whoami、account status、刷新失败及离线 doctor。协议与命令接线只用合成凭据、临时配置和 loopback，不代表官方双端等价，不访问真实 Google 或 OS 凭据库。旧 338 场景输入与分类须保持。真实 Workforce、STS、Colab 接受性和跨平台尚未验收；AWS/证书来源、metadata TLS 及其余设计要求仍有缺口。

纯内存参考库探针见 [probe_sts_auth.py](../tools/probe_sts_auth.py) 与 [执行记录](../research/sts-auth-probe.json)。用固定 CLI 虚拟环境的 Python 加 -I 执行，不加载 ADC、keyring 或 transport；当前观察版本为 google-auth 2.62.2，不能替代固定 CLI/EXT/KERNEL 的业务基线。复跑命令：

```sh
/path/to/google-colab-cli-venv/bin/python3 -I tools/probe_sts_auth.py --output research/sts-auth-probe.json
```

最终验证：默认特性 344 项、无默认特性 342 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建通过。完整 CLI 差分 344 个场景为 92 一致、54 保留差异、198 Rust 扩展，零断言失败，旧 338 场景输入与分类不变；业务场景 280 个。合并帮助 83 路径无缺失、短 help 无失败；研究工具 55 项中 54 通过、1 项 Linux 专属跳过，另有四项纯内存参考库协议核对通过。

本机 release 重建、打包字节/manifest/SHA256SUMS、version、账户别名与联合授权帮助、五种补全输出通过；两份 debug 报告哈希与当前二进制一致。未推送代码或发布 Release。

## Workforce 用户作为服务账号模拟来源

`impersonated_service_account` ADC 的 `source_credentials` 可使用 `external_account_authorized_user`。加载后复用 Workforce 的 STS Basic 刷新和缓存，再调用既有 IAM `generateAccessToken` 流程。IAM 使用源 token 和嵌入配置的配额项目，provider 对外返回 IAM 签发的目标 token 和外层配额项目。`ext doctor` 能离线识别该结构。Google Auth 2.62.2 的纯内存配置核对通过。参考 [Google Auth 实现](https://googleapis.dev/python/google-auth/latest/_modules/google/auth/impersonated_credentials.html)。

loopback 测试核对 STS 表单、IAM 身份、委托、scopes 和配额的分离。第一次 IAM 返回 503 时，重试复用仍有效的源 token，随后缓存目标 token。模拟 Colab 的会话查询收到目标 token 和外层配额项目。原始 ADC 文件保持逐字节一致。真实 Workforce 身份的 IAM 授权和 Colab 接受性仍待验收。详见 [ADC 回归](../tests/adc_workforce.rs)。

后续已接入 metadata ADC 的默认文件缺失发现，见 [38](38-adc-metadata-implementation.md)；不代表真实平台或 mTLS 验收完成。

Unix external_account 可执行来源现支持显式 opt-in、非交互执行和输出缓存，见 [39](39-adc-executable-implementation.md)；不会在 doctor 中执行。

后续已接入 AWS aws1 的环境/EC2 与 SigV4 主体，见 [40](40-adc-aws-implementation.md)；并未验证真实云端接受性。
