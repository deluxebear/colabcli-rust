# SSH 三种模式与清理实现

本章记录 Unix SSH CLI 的实际实现。后续已完成单次 macOS ARM64 到真实 Colab CPU 的五项 SSH 检查及资源释放，见 [56 真实验收](56-live-ssh-validation-and-guarded-cleanup.md)。后续单次真实登录 shell PTY 与命令 SIGTERM 已通过；仍有两次原因未定位的尝试失败记录。Windows 原生 stdio/ProxyCommand、其他交互/信号故障和各平台完整验收仍未完成，不能把 loopback 测试视为线上等价证明。固定上游契约与源码证据见 [03 第 8 节](03-runtime-and-wire-protocols.md)。

## 入口与创建矩阵

```sh
colab ssh -s cpu-demo -i ~/.ssh/id_ed25519
colab ssh -s cpu-demo -- 'python -c "print(42)"'
colab ssh -s cpu-demo -- python script.py 'argument with spaces'
colab ssh --proxy-mode -s ephemeral --rm -i ~/.ssh/id_ed25519
```

| 模式 | 选择与自动创建 | --rm | PTY |
| --- | --- | --- | --- |
| 交互，无 -s 且本地会话为空 | 自动创建六位十六进制名称的 VM | 仅释放本次自动创建的 VM | 系统 ssh 使用 -t |
| 交互，已存在本地会话或显式 -s | 使用唯一会话或指定会话；指定缺失不创建 | 提示忽略，不释放已有 VM | -t |
| 一次性命令 -- CMD | 只连接已有会话 | 提示忽略 | stdin/stdout 都是 TTY 才 -t，否则 -T |
| --proxy-mode -s NAME | 名称不存在时创建，存在时复用 | 断开时释放选中的 VM | 透明 stdio 桥接 |
| --proxy-mode，不指定 -s | 只选择已有唯一会话；空或歧义报错 | 对选中的 VM 生效 | 透明 stdio 桥接 |

--proxy-mode 和 CMD 冲突时在认证前返回 2。内部 --colab-internal-mode=cmd 只连接已有会话，避免 OpenSSH 子进程重新创建 VM。--gpu/--tpu/--high-mem 只在自动创建时生效，复用时提示忽略；创建路径沿用 new 的协议、TPU 优先和硬件参数验证。

交互和命令模式调用系统 OpenSSH；不在本地实现 SSH 协议或远端 sshd。交互 shell 在 /content 启动 login shell；单个 CMD 字符串保留 shell 语义，多个 CMD 参数逐个 shell-quote 后组成精确 argv。命令模式返回 OpenSSH 的退出码。使用原版的 StrictHostKeyChecking=no、空 known_hosts 文件和 LogLevel=ERROR 配置。

## 身份、账户与 ProxyCommand

默认按 ~/.ssh/id_ed25519.pub、id_ecdsa.pub 的顺序选择公钥。--identity 支持 .pub 或私钥路径，优先读取对应 .pub；缺失或显式公钥为空时用系统 ssh-keygen -y -f 导出。支持 ~ 路径展开；读取的公钥必须是非空单行 UTF-8，最多 64 KiB，防止 header 注入。公钥的密码学有效性及服务端支持的类型仍由 ssh-keygen/服务端判断，不把库内字符串检查宣称为密钥验证。

Rust 不读取或持久化私钥内容。公钥导出子进程受请求期限和信号控制；等待期间监测输出文件大小，读取最多 64 KiB。外部进程的瞬时写入量不构成硬配额保证；超限、失败或取消时终止并等待回收。临时输出在受控返回时删除，错误不显示导出正文、私钥或 token。

账户从现有显式上下文解析；不修改父进程环境切换身份。OpenSSH 的 ProxyCommand 使用当前可执行文件，显式携带绝对配置目录、所选账户、auth provider、凭据后端、keyring 等待期限、控制面地址、请求期限及自定义 OAuth 配置，身份路径也转为绝对路径。代理子进程携带捕获的 endpoint，只连接父命令选中的 VM；名称缺失或 endpoint 不匹配时拒绝连接，禁止自动创建替代实例。不继承 --rm，释放责任留在父命令。

ProxyCommand 经 POSIX shell 转义，额外把字面 % 写成 %%，因为 OpenSSH 在 shell 执行前展开 %h 等占位符。可执行文件、配置与密钥路径含空格、中文、单引号及百分号时也必须保留原值。没有使用 Windows cmd.exe 的转义规则，Windows CLI 因此明确在认证或分配前拒绝 SSH，不会创建一个无法连接的 VM。

## WebSocket 和字节流

SSH 独立连接 /colab/ssh，丢弃 runtime URL 原有 path。query 包含选中会话的 colab-runtime-proxy-token，保留两个同名 colab-client-agent 值：colab-cli 和 ssh-mode-interactive/cmd/proxy。header 只发送 X-Colab-Ssh-Pubkey 与 X-Colab-Client-Agent；不将控制面 OAuth 或 Jupyter 的 Authorization header 发往此端点。连接不跟随重定向，网络错误不展示 URL、query 或原始响应正文。

HTTP CONNECT 环境代理与 NO_PROXY 已接入，代理头隔离和范围见 [48](48-websocket-http-proxy-implementation.md)。TCP 每地址连接最多 5 秒，TLS/upgrade 沿用请求期限；DNS 解析尚无独立可取消期限。WebSocket frame/message 上限为 8 MiB；HTTP upgrade 的限制由传输依赖负责，尚未单独设置按字节统计的 header 总配额。客户端每 60 秒发送协议 PING，自动回复服务端 PING；控制帧不写入 SSH stdout，心跳不代表 VM 续租。

Unix 桥接暂设 stdio 为非阻塞，退出恢复原 flags；网络由一个循环拥有。stdin 原始字节发送为 binary WS，binary/text payload 原样写到 stdout，不转换换行、ANSI 或文本编码。一次只持有一个待写输出消息，输出阻塞会反压网络读取，但继续检查取消。socket 部分写入后由 tungstenite 保存未发送部分，不能重发整个 frame；已有受限 socket buffer 的回归证明数据不重复或丢失。

stdin EOF 关闭整个 WS，与原版一致；这不是 SSH 传输半关闭。远端正常 Close 返回 0。Rust 将异常断线、broken pipe 和读写失败返回非零，原版会吞掉这些错误并返回 0；这是明确保留的错误传播修复。stdout 不打印账户、会话选择、创建或释放提示，所有 proxy 生命周期提示写 stderr。官方自动选择唯一会话会污染 stdout，Rust 保留修复；没有会话时的错误同样放 stderr。

## 清理、信号与恢复

VM 创建使用已有的持久化分配事务和资源所有权标记；SSH owner 以 ssh: 前缀生成，存储在 ssh_owner，与 run_owner 分开。VM 成功创建后，公钥解析、WS 握手、stdio 初始化、子进程启动和桥接全部在清理范围内。--rm 按名称及捕获的 endpoint 精确释放；并发替换后的同名 VM 不会被旧命令释放，其他账户的同名会话也不受影响。

正常断开及受控失败都只进行一次清理；失败写入现有释放事务日志，退出时明确提示同一账户 stop -s NAME 恢复。SSH 创建阶段结果未知时，保留分配日志，先按相同硬件参数 new 调和，再核查并 stop；不自动重放远端命令，不手删日志，也不自动申请替代 VM。分配尚无 endpoint 时不能假装已经释放。

代理模式 --rm 的 SIGINT/SIGTERM/SIGHUP 路径关闭桥接、清理 VM，清理成功返回 0；失败返回非零并保留恢复线索。未启用 --rm 的代理信号返回 128+signal。父交互/命令模式会把信号转发给所拥有的 OpenSSH 子进程，等待退出，超出约 1 秒再终止并回收，返回 128+signal；交互自动创建且 --rm 时仍进行释放。公钥导出等待也响应信号并回收子进程。

进程 SIGKILL、系统崩溃、DNS/TLS/HTTP 中的即时取消和整个后代进程树回收不作保证。释放过程中强制终止依靠已落盘事务恢复；尚未用真实长期 SSH 连接验证服务端行为。原版公钥解析/握手发生在 finally 之前，会跳过 --rm；差分保留两个已观测的早期失败清理差异，Rust 不复制资源泄漏。

## 验证

[SSH 测试](../tests/ssh.rs) 包含 21 项，覆盖三种 mode 的真实 loopback wire、重复 query、token 隔离、400/401/403/404/429/502、429 limit 类型和范围、重定向拒绝与错误脱敏、binary/text/control、双向反压、stdin EOF、异常断线、broken pipe、输出阻塞取消、公钥来源与真实 ssh-keygen 导出、精确 argv、四种 stdin/stdout TTY 组合、退出码、账户与 endpoint 清理隔离、分配后公钥失败清理、释放日志恢复、三种代理信号、公钥导出与 OpenSSH 子进程取消和回收，以及旧 endpoint/缺失会话的子进程拒绝路径。

其中使用本机原生 OpenSSH，并用 -F /dev/null 隔离开发者 SSH 配置，验证包含空格、中文和 %h 的可执行文件/配置/密钥路径能够启动实际 ProxyCommand 并到达 loopback WS；服务器只模拟 SSH banner，不提供完整 sshd 或执行远端命令。其余 OpenSSH 调用矩阵使用临时替身程序，不能据此声称已经完成真实 Colab SSH 登录。

[官方差分](14-cli-comparison.md) 新增 20 个 SSH 双端行为场景、两个 endpoint 绑定的 Rust 扩展场景及 help；proxy stdout 额外保存并比较 base64 原始字节，非 UTF-8 不经损失性解码。服务端检查重复 query、公钥/agent header 和输入字节。原版缺陷与 Rust 修复分开记录，不把不同结果改标为 matched。测试使用临时配置、合成 token 与密钥、本地服务，没有创建新 Google 资源。Linux 全链路、真实 SSH 认证、Windows 原生实现及长会话仍待验证。

该阶段最终默认 209 项、无默认特性 207 项测试通过；两种特性的 Clippy、fmt、默认构建和研究文档检查通过。175 个差分/扩展场景无断言失败，其中 88 个一致、39 个保留差异、48 个 Rust 扩展。差异分类不表示完整 V06 或全部设计已经验收完成。

SOCKS5/SOCKS5h 环境代理已随后接入，共享 HTTP/WS 的代理路径；DNS 模式、认证和验证范围见 [49](49-websocket-socks-proxy-implementation.md)。

HTTPS 代理连接与独立的双层 TLS 已接入，库调用者可显式添加代理 CA；默认验证、类型变化及 loopback 证据见 [50](50-websocket-https-proxy-implementation.md)。

Linux ARM64 后续已运行包含真实本机 OpenSSH ProxyCommand 的完整离线测试，见 [54](54-linux-container-validation.md)。该容器证据补齐 Linux 用户态 loopback 运行，仍未连接真实 Colab sshd，不替代真实 SSH 认证、其他 Linux 架构或 Windows 验收。

真实 SSH 的开发验证脚本与 endpoint 条件保护后续见 [56](56-live-ssh-validation-and-guarded-cleanup.md)。父命令/显式交互也可接收隐藏的 endpoint 条件；现有 raw proxy 条件仍保留。由于当前文件后端未登录，真实 Colab SSH 仍未新增验收证据。
