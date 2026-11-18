# ADC service_account 签名与交换

更新日期：2026-11-17。本阶段在 authorized_user 之外实现 service_account ADC。格式加载、离线签名/交换通过，不表示 Google 或 Colab 接受服务账户身份；整体 ADC 矩阵仍未完成。

## 协议和输入

[Google 服务账户协议](https://developers.google.com/identity/protocols/oauth2/service-account) 定义 RS256 JWT bearer 交换：JWT 包含 iss、scope、aud、iat、exp，JWT 最长一小时；token 请求以表单携带 grant_type 和 assertion。本实现使用 ring 的 RSA PKCS#1 v1.5/SHA-256 签名，不自行实现 RSA 运算。

```sh
colab account add service --adc-file /absolute/path/service-account.json
colab account login service
colab -a service sessions --json
colab -a service whoami --json
colab account status --json
```

来源发现顺序不变：预置 GOOGLE_APPLICATION_CREDENTIALS、账户文件、CLOUDSDK_CONFIG、用户 gcloud 默认文件。路径和 provider 都显式传递，不通过修改进程环境切换身份；仍未接入 metadata 自动发现。

[adc.rs](../src/auth/adc.rs) 读取最多 64 KiB JSON，接受 service_account 的 client_email、private_key、可选 private_key_id、可选 token_uri；默认 token endpoint 为 https://oauth2.googleapis.com/token。私钥必须是单个未加密 PKCS#8 RSA PEM，使用 ring 的密钥尺寸和数学有效性校验；PKCS#1、加密 PEM、拼接块及损坏编码拒绝。email/key id 做长度与控制字符检查。unknown JSON 字段不作为 JWT claim 透传。

JWT header 固定 alg=RS256、typ=JWT，存在 key id 才加 kid。iss 是 service email，iat 是当前时间，exp=iat+3600，scope 固定为现有 PUBLIC_SCOPES。aud 始终为 Google token endpoint，即使显式 token_uri 用于 loopback 测试或配置另一传输地址；它不是 runtime URL。没有 sub，不做 domain-wide delegation，不支持其他 universe；显式 subject 或非 googleapis.com universe 拒绝。quota_project_id 尚未用于额外配额请求头。

token_uri 仅允许现有 HTTPS 或 loopback HTTP 规则，禁止认证信息、fragment 和 query。JWT 只发送到 token endpoint，随后控制面只携带交换得到的 access token，不使用 self-signed JWT 直接访问 Colab。

## 缓存、状态与失败

AdcProvider 的公开接口保持不变，内部分为 User 与 Service 两种来源。每个 provider 独立持有来源和内存缓存，同一实例用互斥锁串行初始化/刷新；不同实例、不同账户不共享缓存。普通 access_token 在剩余有效期超过 60 秒时复用，临界点重新签署并交换；refresh_access_token 强制交换，供 whoami 使用。

成功响应要求非空 Bearer token、正的 expires_in，且到期时间可表示；缺少期限拒绝。响应 scope 存在时记录其字段，否则缓存请求的 scope 集合；这不是线上 scope 验收。service_account 不采用响应 refresh_token；下一次由密钥重新签署 JWT。交换失败不覆盖之前仍有效的 token，过期 token 不会作为有效结果返回。源文件在 provider 生命周期内读取一次；修改或轮换文件后需创建新 provider（后续 CLI 调用会重新加载），不会在一次执行中途切换来源。

TokenClient 延续禁重定向、连接/总期限、64 KiB 响应上限和静态错误；未新增自动 HTTP 重试或交互。ADC 文件输入、PEM 解码缓冲区、token 响应缓冲区使用零化容器；assertion 用 Secret 保存，表单副本在请求结束后清除。不能据此承诺操作系统或 HTTP 库不存在其他内存副本。

本路径不写 OAuth 文件、keyring 或 ADC 源文件，不持久化私钥、JWT、access token，不改变 active 或账户绑定。密钥解析/签名失败在联网前退出；服务端错误不打印 JWT、私钥、原始 body 或带秘密的 URL。其他未支持 ADC 类型继续静态拒绝，未知 type 不回显其原文。

ext doctor 只检查 service_account 基本字段存在，报告 service_account_configured；不解析 RSA、不签名、不请求 token，也不证明私钥有效。ext version 增加 service_account_adc 能力项。

## 验证矩阵

| 类型 | 格式/本地密码学 | token 获取 | scope 实际授予 | Colab 接受身份 |
| --- | --- | --- | --- | --- |
| authorized_user | 已有隔离测试 | loopback refresh 通过 | 真实 ADC 未验收 | 未验收 |
| service_account | 临时 RSA 密钥、独立实现验签通过 | loopback JWT bearer 通过 | 仅检查请求 scope，真实授予未知 | 未验收 |
| impersonated / external / metadata | 未实现 | 未实现 | 未知 | 未知 |

新增五项 Rust 集成测试：两套临时 RSA 密钥的并发来源隔离、RS256 独立验签与精确 claim/表单；到期临界点与强制刷新失败缓存；密钥、身份、端点和超限源文件联网前拒绝；重定向/超长/无效 token 响应脱敏；真实 CLI login、sessions、whoami 与 account status 使用交换 token。测试生成的密钥仅存在内存和临时目录，不提交私钥或 JWT 样本。RSA 生成与独立验签库仅为 dev dependency，生产签名使用已用于 TLS 的 ring。

原认证/utility 测试继续覆盖授权轮换、失败暂存、账户并发与 ADC 不回写。doctor 原只读测试增加 service_account 非解析样本。CLI 差分新增四项 Rust 扩展场景：无效私钥、只读基本配置、状态行错误及 external_account 禁止执行；成功的密码学路径由 Rust 测试验证，未将模拟验签当作真实 Google 授权。

后续已接入 IAM impersonation，见 [31](31-adc-impersonation-implementation.md)；本章矩阵记录服务账户阶段的验收状态。

仍待完成：其他 ADC 来源、真实身份及 Colab/scope 验收、update/发行渠道、合并版 help 和完整平台验收。长期保活、自动重建与任务检查点仍由脚本或外部调度器负责。

最终验证：默认特性 287 项、无默认特性 285 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 281 个场景为 92 一致、53 保留差异、136 Rust 扩展，无断言失败，原有 277 场景分类不变；root/account/drive/ext 清单一致，报告二进制哈希与当前 debug 文件相符。

后续 [52](52-adc-quota-project-implementation.md) 已接入 ADC quota_project_id / GOOGLE_CLOUD_QUOTA_PROJECT 的请求元数据，区分 IAM 来源与目标、保留 metadata 请求隔离。本篇此前的配额项目限制保留为当时范围；实际项目权限与服务接受性仍未验收。
