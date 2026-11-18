# WebSocket 的 HTTP CONNECT 代理

## 输入与选择

此前 HTTP 请求使用 reqwest 的代理设置，Jupyter/SSH WebSocket 却自行 TCP 直连。现在两种 WebSocket 共用 [连接层](../src/websocket_transport.rs)，默认在连接时捕获代理环境快照；库调用者也可显式传 ProxyEnvironment，测试不需要修改进程环境或认证身份。

```sh
HTTPS_PROXY=http://127.0.0.1:7890 colab exec -s work -f script.py
ALL_PROXY=http://127.0.0.1:7890 NO_PROXY=localhost,127.0.0.1 colab ssh -s work
```

ws 使用 HTTP_PROXY/http_proxy，wss 使用 HTTPS_PROXY/https_proxy；缺少或空的专用值回退 ALL_PROXY/all_proxy。每对变量先检查大写；大写存在但为空不再读取小写。NO_PROXY/no_proxy 也以大写存在为优先，可用逗号分隔域名、前导点域名、IPv4/IPv6、CIDR 或全局 *。域名只匹配自身与子域，不匹配同后缀但没有域边界的主机。

NO_PROXY 直接复用锁定 hyper-util 0.1.21 的 Matcher，与当前 reqwest 的匹配引擎相同；该包此前已是间接依赖，本轮将其声明为直接依赖，没有新增包版本。REQUEST_METHOD 存在时，与当前 HTTP matcher 一样不使用环境代理。它是 CGI 环境规则，不是账户选择规则。

此处支持 http:// 代理，以及省略协议的 host:port。代理地址最多 8192 字节，不允许路径、query 或 fragment；用户名/密码可采用 URL percent 编码，解码后发送 Basic Proxy-Authorization，用户名不能包含冒号。目前不支持 https:// 代理连接、SOCKS、PAC 或系统设置自动发现；命中这些不支持的配置会明确失败，不能静默直连。HTTPS_PROXY 这个变量的名称表示目标协议，可以填写 http:// 代理；目标 wss 的 TLS 继续由原端点握手处理。

## 连接、认证与状态

CONNECT 的请求目标只包含原端点 host:port（包括默认端口和 IPv6 方括号），不包含 runtime path/query、Bearer、runtime token 或 SSH 公钥。代理认证头仅用于 CONNECT，不写进随后 Jupyter 或 SSH 的 WS 握手。隧道建立后仍使用原来的 WS URI、端点凭据、TLS 验证、Jupyter session ID、公钥和 agent，HTTP 代理不是新的认证身份。

按 [RFC 9110 CONNECT](https://www.rfc-editor.org/rfc/rfc9110.html#section-9.3.6) 接受 HTTP/1.0 或 1.1 的最终 2xx，处理 1xx 中间响应（不接受 101 作为隧道成功）。累计响应头最多 16 KiB，逐字节读取至 header 结束，避免吞掉紧随其后的隧道数据。407、重定向或坏响应只报告状态码/固定错误，不显示正文、Location 或含凭据代理 URI；不自动重试认证，不跟随重定向，也不在代理失败后退回直连。

TCP 尝试每个地址最多 5 秒，代理建立阶段每次读写都扣除同一连接 deadline；持续滴流的头不能不断延长期限。系统 DNS 仍是阻塞解析，没有独立可取消期限。隧道后的 TLS/upgrade 沿用原有阻塞 I/O 和剩余超时设置，不把它宣称为覆盖所有 TLS 往返的绝对可取消期限。对于 wss，端点 TLS 在隧道里建立；ws 本身没有 TLS，不能宣称代理看不到隧道内的明文。

失败时释放本次 TCP 连接；不创建后台重连或重放执行。Jupyter 仍在 WS 之前持久化 kernel ID，CONNECT 失败后的显式重试通过 GET 复用 kernel，不再次 POST。SSH 继续使用既有 owner/--rm 清理规则，连接层不增加 VM 分配、释放或任务恢复策略。

## 验证

[10 项测试](../tests/websocket_proxy.rs) 使用显式环境快照、临时账户、合成凭据和 loopback 服务，覆盖：

- 环境大小写/专用代理/ALL_PROXY 优先级、CGI、域边界、IPv4/IPv6/CIDR/* 的 NO_PROXY。
- 真正直连绕过代理、带 percent 编码 Basic 的 CONNECT、100→204 响应和不预读隧道数据。
- Jupyter 请求/输出及 SSH 二进制经过 CONNECT；端点 WS 请求不携带 Proxy-Authorization，CONNECT 不携带端点 token/公钥。
- 407、302、坏状态行、超大头与敏感正文不泄露，也不退回可达的直连目标。
- 滴流响应的绝对 CONNECT deadline，以及隧道建立后先发送 TLS ClientHello 而非 runtime HTTP 凭据。
- 实际 CLI 的 HTTP_PROXY 环境读取；407 后两次 exec 为 POST→CONNECT→GET→CONNECT，已有 kernel 保留且没有 running/execution_pending。
- 实际 ssh --proxy-mode 读取小写 all_proxy 并保留含 NUL 的双向字节，不分配或释放已有会话。

测试使用临时 HOME、显式 OAuth 配置、文件凭据及不存在服务的 loopback 目标端口，成功请求只能经过本测试代理。代理服务的 accepted socket 显式恢复阻塞模式，SSH 客户端按既有非阻塞约定轮询读取，避免把 macOS 继承的 nonblocking 标志或 WouldBlock 时序当作协议故障。

```sh
cargo test --locked --test websocket_proxy
```

测试证明 HTTP CONNECT 路由、隔离、失败恢复和 TLS 起始字节；没有完成真实企业代理、成功的代理 TLS/证书验收、HTTPS 代理、SOCKS 或跨平台全链路。已有真实 CPU 直连记录不能替代本阶段代理验收。环境变量依据 [reqwest 说明](https://docs.rs/reqwest/latest/reqwest/#proxies)，具体优先级及 NO_PROXY 依据本地锁定依赖源码和上述测试，不宣称与所有 Python/系统代理实现完全一致。

本机 aarch64-apple-darwin 的 release 构建、[16 项发行检查](../research/native-release-validation.json) 通过；[两组优化版 exec/repl 图片差分](../research/release-image-comparison.json) 再次一致，检查的是重构后无代理的直接连接路径，不作为代理分支或成功代理 TLS 的优化版验收。release 8,787,344 字节，默认特性测试用 debug 41,454,248 字节。

最终默认特性 421 项、无默认特性 419 项 Rust 全量回归通过，两种特性 Clippy、格式和研究文档检查通过。既有 CLI 365 场景保持名称、命令和分类（92 一致、54 保留差异、219 Rust 扩展）。代理专项的 10 项测试独立计数；不把基础打包/直连检查解释为代理线上验收。

以上为 HTTP CONNECT 阶段的历史验证范围。后续 [49](49-websocket-socks-proxy-implementation.md) 已补 SOCKS5/SOCKS5h；HTTPS 代理与真实企业代理仍待验收。

后续 [50](50-websocket-https-proxy-implementation.md) 已实现 HTTPS 代理与成功的 loopback 双层 TLS；本篇此前的未实现/未验证结论保留为当时范围。真实企业代理、跨平台和线上证书环境仍未验收。
