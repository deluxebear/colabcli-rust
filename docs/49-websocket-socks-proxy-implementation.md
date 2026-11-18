# SOCKS5/SOCKS5h 环境代理

## 输入与职责

共享 [WebSocket 连接层](../src/websocket_transport.rs) 增加 [SOCKS5 CONNECT](../src/websocket_transport/socks.rs)，同时启用锁定 reqwest 的 socks 特性，使 HTTP 与 Jupyter/SSH 的 WS 都能使用 SOCKS5/SOCKS5h。没有引入额外包版本。环境优先级、CGI 和 NO_PROXY 规则沿用 [48](48-websocket-http-proxy-implementation.md)。

```sh
ALL_PROXY=socks5h://127.0.0.1:1080 colab exec -s work -f script.py
ALL_PROXY=socks5://127.0.0.1:1080 colab ssh -s work
```

socks5 在本地解析目标域名，再发送 IPv4/IPv6 地址；socks5h 将域名交给代理，IP 字面量仍发送 IP。代理未写端口时使用 1080。目标 URI 的 path/query、Bearer 和 SSH 公钥不进入 SOCKS 目标地址。系统 DNS 仍使用阻塞解析，不能取消；不是独立受限的 DNS 服务。环境配置不会切换账户身份。

按 [RFC 1928](https://www.rfc-editor.org/rfc/rfc1928.html) 实现 TCP CONNECT 的无认证和用户名/密码认证；不实现 BIND、UDP ASSOCIATE、GSSAPI 或 SOCKS4。代理地址不接受额外路径、query 或 fragment。HTTPS 代理、系统代理/PAC 自动发现仍未实现。

## 状态、失败与清理

代理 URL 的用户名和密码经 percent 解码为 UTF-8，按字节计数，各要求 1–255 字节；空、超长或无效编码在发送 greeting 前失败。认证包遵循 [RFC 1929](https://www.rfc-editor.org/rfc/rfc1929.html)。配置凭据时只提供用户名/密码方法，不接受服务器降级到无认证；未配置时只提供无认证方法。此认证本身是明文传输，不等于代理链路加密。

CONNECT 的请求和响应分别校验版本、保留字、地址类型与长度。回复支持 IPv4、IPv6、域名绑定地址，精确读取字段，不预读后续隧道数据；绑定地址不触发新连接。失败只返回固定错误或 REP 数字，不显示代理地址、认证包或服务端正文。整个 SOCKS 协商每次 I/O 使用同一 deadline 的剩余时间，滴流不能无限续期；后续端点 TLS/WS 沿用原连接约定。

失败即释放本次 TCP；不重试认证、不换代理、不退回直连，也不重放远端代码。WS 前已保存的 kernel ID 保留，显式重试通过 GET 复用；不增加会话分配、释放、supervisor、任务重建或检查点恢复。HTTP 请求内部使用 reqwest 的 SOCKS 实现；此处不把两者所有畸形输入或错误文字宣称为完全相同。

## 验证与边界

[代理测试](../tests/websocket_proxy.rs) 共 16 项，包含此前 10 项 HTTP CONNECT 测试和新增 6 项 SOCKS 专项；五项既有集成测试增加 SOCKS 配置分支。全部使用 loopback、临时 HOME/配置和合成凭据：

- IPv4/IPv6 请求与三种绑定回复、紧随回复的首字节保留；不存在的 .invalid 域名在 socks5h 原样发送，不做本地解析。
- socks5 的 localhost 本地解析、percent 编码 UTF-8 认证字节。
- 认证方式降级、错误版本/密码、空和超长凭据、坏回复/截断及敏感信息不泄露；失败不触碰可达直连目标。
- 持续滴流的协商仍在共同 deadline 内失败。
- Jupyter 执行与带认证的 SOCKS 隧道；端点只收到原 Bearer，没有 Proxy-Authorization。
- SSH 含 NUL 的双向字节、公钥/运行时 token 仍在端点握手；实际 ssh --proxy-mode 继承 all_proxy。
- 实际 exec 的 HTTP REST 和 WS 都经同一 SOCKS 服务；拒绝后显式重试为 POST→隧道失败→GET→隧道失败，不重复创建 kernel，running/pending 保持为空。
- wss 在 SOCKS 隧道内先发送 TLS ClientHello；服务端随后断开，因此只证明 TLS 起始，没有证明成功 TLS/证书链。

```sh
cargo test --locked --test websocket_proxy
```

没有使用真实 Google 凭据或创建云资源；没有完成真实 SOCKS/企业代理、成功代理 TLS、跨平台验收，也不把普通打包或无代理差分当作 SOCKS 线上证据。

默认特性全量 427 项、无默认特性全量 425 项 Rust 回归通过；原有官方 CLI 365 场景的名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展）。本机 release 的 [16 项发行检查](../research/native-release-validation.json) 与 [两组无代理图片差分](../research/release-image-comparison.json) 通过，release 为 8,837,872 字节、默认特性测试用 debug 为 41,727,048 字节。普通打包和直连检查不作为 SOCKS 分支或成功代理 TLS 的优化版验收；其他合并版专项报告仍保留为各自阶段的历史证据。没有推送或发布。

两种特性的 Clippy 与格式检查通过；研究文档检查覆盖 54 篇 Markdown、571 个相对链接、125 个固定源码引用和 143 个源码哈希，无错误。

后续 [50](50-websocket-https-proxy-implementation.md) 已实现 HTTPS 代理与成功的 loopback 双层 TLS；本篇此前的未实现/未验证结论保留为当时范围。真实企业代理、跨平台和线上证书环境仍未验收。
