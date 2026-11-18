# 验收与证据缺口

测试目标是证明功能、协议和失败恢复满足明确契约。下列 V 编号是完整验收要求；研究阶段执行了静态清单、文档检查和七项纯编解码函数探针。Rust 各阶段回归结果详见 [09](09-foundations-implementation.md)；远端 DriveFS 脚本另有 41 项通过、1 项 Linux 专属跳过，见 [23](23-drivefs-probe-and-recovery.md)。另已完成单次真实 CPU 验收，见 [15](15-live-cpu-validation.md)。没有运行上游完整测试套件，V 编号尚不能整体认定完成。

## 本轮已执行的离线检查

[协议探针结果与样本](../research/kernel-codec-probe.json) 固定源文件 SHA-256，使用合成消息验证：DEFAULT TEXT 往返、含非 UTF-8 与空 buffer 的 BINARY 往返、big-endian 数量与首偏移、N=1 BINARY 被 decoder 拒绝、非法 JSON 偏移、非法 buffer 偏移，以及 V1 little-endian 往返。七项均通过。

这些检查执行的是从固定源码 AST 提取的五个函数，没有导入目标包、联网或访问账户。样本可用于未来 Rust codec 差分，但尚不满足 V05 的完整状态机和真实互操作验收。

## 验证层次

| 层次 | 运行环境 | 可证明的结论 | 不能证明 |
| --- | --- | --- | --- |
| 静态清单 | 本地 Python AST，不导入目标 | 固定源码身份、显式签名和测试函数位置 | 运行时钩子合并结果、真实协议接受情况 |
| 单元与模型 | Rust 本地、伪时钟、临时目录 | codec、路由、状态机、迁移与错误分类 | Google 服务器行为 |
| Loopback 协议 | 本地真实 HTTP/WS server 与 CLI 子进程 | 实际线上报文字节、握手头、超时与关闭 | 账户权限、DriveFS 兼容 |
| 离线差分 | 隔离 Python 基线与 Rust，固定响应 | help、JSON、退出码、参数转发兼容 | 所有未采样输入的等价 |
| 真实端到端 | 授权账户、真实 Colab CPU VM | 当前账户/环境/版本下路径有效 | 其他组织政策、未来稳定性 |
| 长时与故障 | 受控 VM/进程/连接中断 | 刷新、资源恢复和断线处置 | SIGKILL 后当前进程仍可清理 |

现有测试函数位置可查 [清单](../research/source-inventory.json)。静态计数为 CLI 360、EXT 10、KERNEL 14 个 `test_` 函数，不等于参数化后的测试用例数。扩展测试使用临时配置与内存 keyring；不能把这些断言当作已经挂载真实 Drive 的证据。

## V01 命令与输出

| 用例 | 断言 |
| --- | --- |
| V01.1 合并 help | 根命令、所有组、隐藏别名和关键子命令参数存在；保存实际 Python 合并 help |
| V01.2 尾部参数 | 空格、引号、Unicode、反斜线、`--`、`--env`、类似 `-a` 的脚本参数保持正确归属 |
| V01.3 stdout | URL 只有 URL；JSON 无横幅；SSH proxy 字节流完全一致 |
| V01.4 TTY | stdin/stdout TTY 四种组合，REPL EOF、脚本管道、图片重定向 |
| V01.5 非交互 | 缺凭据时不等待 input，不自动打开网页；单账户失败不阻塞 --all |
| V01.6 更新 | 每日检查开关、缓存、超时与提示抑制；Rust 版更新不调用 pip/uv |

## V02 账户与存储

| 用例 | 断言 |
| --- | --- |
| V02.1 账户解析 | 显式、env、active、会话路由；active 内已有同名会话优先，多匹配报错 |
| V02.2 新建 | new/run 不因同名会话跳转其他账户 |
| V02.3 Drive 解析 | 显式、env、绑定、本账户凭据、全局默认、回退；consent 写入账户特殊规则 |
| V02.4 状态隔离 | 两个账户同名会话、历史、token、pending 均独立 |
| V02.5 ADC 并发 | 两个不同 ADC 文件并发请求，server 验证各自实际 token 来源，不只比较文件路径 |
| V02.6 锁与崩溃 | 两进程并发写、读改写丢失、写中断、截断 JSON、权限错误 |
| V02.7 迁移 | 默认旧文件/旧 keyring 复制；失败不删源；重复执行幂等 |
| V02.8 重命名 | 中途 keyring/目录/注册表失败，旧引用仍可恢复；所有 Drive 绑定同步 |
| V02.9 删除 | 有 session 时保护；force 仅按已公开语义删除资料，不隐式停止其他 VM |
| V02.10 路径 | `..`、分隔符、绝对路径、符号链接、规范化后冲突不能逃逸配置目录 |

锁兼容必须用 Python 与 Rust 两个真实进程竞争同一存储验证；各自测试通过不能证明互通。若无法保证共同写入，初版明确禁止并发混用，支持离线迁移。

## V03 OAuth 与凭据

| 用例 | 断言 |
| --- | --- |
| V03.1 主 OAuth | 正确 scopes、remote redirect 与 client 配对、拒绝与取消 |
| V03.2 state | callback 缺失/错误/重放 state 均拒绝；一次性使用 pending |
| V03.3 PKCE | 授权 URL 存在 S256 challenge，交换请求发送匹配 verifier；不凭字段名认定生效 |
| V03.4 两步流程 | url-only 持久化 pending，code 结束正确账户；超期、取消、错误 code 不污染现有 token |
| V03.5 部分授权 | 保存实际 granted scopes；刷新不扩大 scopes；缺少能力明确提示 |
| V03.6 keyring | 正常互操作、锁定、D-Bus 无响应、超时、文件降级与写入失败 |
| V03.7 token 刷新 | 提前刷新、并发刷新合并、撤销、refresh token 轮转与持久化失败 |
| V03.8 ADC | 用户、service account、metadata、impersonation、external account 各自列出 token 获取与 Colab 使用结果 |

ADC 测试矩阵应分别记录“格式可解析”“可获 token”“具有所需 scopes”“Colab 接受身份”四列。没有实测身份的格子保持 unknown，不能把 token 成功当作全绿。

Drive custom URL-only 的后续固定合并版探针实际验证了两端授权 URL 的 S256 challenge 与各自持久化 verifier 匹配，见 [58](58-drive-consent-profile-implementation.md)。这补齐该路径的动态 PKCE 证据，不代表其他 flavor、真实 code 交换或 Google 接受性均已通过。

## V04 控制面与 Contents

| 用例 | 断言 |
| --- | --- |
| V04.1 GET/POST | XSSI 去除、authuser、client agent、GET XSRF 后 POST、空响应 |
| V04.2 UUID 编码 | 按源码的 UUID 替换和补点，避免误用标准 base64 |
| V04.3 重试 | POST 已生效但响应丢失，下一次 GET 找回同一 assignment，不创建第二份 |
| V04.4 错误 | 412、401/403、502/503/504、ReadTimeout、损坏 JSON、未知枚举 |
| V04.5 token | proxy token 提前刷新、旧 state 缺过期字段、后端会话消失 |
| V04.6 文件 | 空文件、大文件、二进制、中文/空格/`#?%` 路径，目录拒绝，notebook JSON |
| V04.7 edit | 不变不上传；404 可新建；认证/网络失败不启动空白覆盖 |
| V04.8 释放 | unassign 失败保留待清理 endpoint；重新运行可恢复，无虚假成功 |

大文件初版可以保持 base64 Contents API 的整文件语义，但必须设定可接受大小与错误提示。分块或流式上传属于额外协议研究，不能擅自假定 `chunk=1` 等于现成流式支持。

## V05 Jupyter 与执行

| 用例 | 断言 |
| --- | --- |
| V05.1 codec | JSON 与二进制 DEFAULT 帧，offset 越界、非法数量、空 buffer 与非 UTF-8 buffer |
| V05.2 分派 | shell/iopub/stdin；不同 parent ID 的输出不串入当前执行 |
| V05.3 完成 | idle/reply 两种先后顺序、丢失 reply、跨请求 idle、迟到输出 |
| V05.4 超时 | 持续输出不超时，只有别的请求输出仍超时，静默超时，0 无限制 |
| V05.5 断线 | 握手失败、发送前断开、发送后断开、dead/restarting，结果未知不重放 |
| V05.6 输出 | stdout/stderr、display_data、execute_result、error、clear_output、display_id 更新与图片；富文本投影和原始数据保留见 [46](46-rich-text-output-implementation.md)，终端 PNG/默认图片文件见 [47](47-terminal-image-output-implementation.md)，JPEG 显示转换见 [51](51-jpeg-terminal-projection-implementation.md) |
| V05.7 持久性 | 第一个 CLI 设置变量，第二个读取同值；本地断开后 kernel 仍存活 |
| V05.8 notebook | 混合 cells、缺 ID、错误 cell、输出写回，其他 metadata 不丢失 |
| V05.9 run | argv、环境、shebang、__main__、SystemExit(None/0/整数/字符串)、异常清理与 keep |
| V05.10 REPL | 多行、快捷键、中文、EOF、Ctrl-C；不要把本地输入取消误认为远端执行已停止 |

现有 [streaming 测试](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/tests/test_streaming.py) 可转为状态机 fixtures。使用伪时钟避免靠长时间 sleep 验证超时。

## V06 SSH

当前 Unix 实现与 21 项离线 SSH 测试见 [19](19-ssh-implementation.md)。除本机 OpenSSH 到 loopback WS 的 ProxyCommand 外，后续已完成单次 macOS ARM64 到真实 Colab CPU 的五项 SSH 检查并确认释放，见 [56](56-live-ssh-validation-and-guarded-cleanup.md)。尚缺真实交互/信号故障、其他平台到 Colab 及 Windows 原生验收，V06 不能整体勾选完成。

真实 loopback server 验证 path、token query、公钥 header、client agent header 与两个同名 query 值；覆盖 400/401/403/404/429/502 错误。测试二进制、半关闭、远端关闭、本地 broken pipe、OpenSSH 子进程退出和信号。

分别覆盖 interactive/proxy/cmd 的创建、复用、`--rm`、工作目录、PTY 条件与退出码。通过“路径含空格”的真实 CLI 子进程验证 ProxyCommand 转义；Windows 若纳入，使用原生 Windows OpenSSH 验证，不以 POSIX mock 替代。

参考 [上游真实 wire 测试](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/tests/test_ssh_wire_contract.py)。Ctrl-C、SIGHUP、SIGTERM 与 SIGKILL 的恢复保证分别记录，不能合并描述为“中断总会释放”。

## V07 Drive

| 用例 | 断言 |
| --- | --- |
| V07.1 授权模式 | dfs loopback、sdk remote、custom 匹配 redirect；import 只接受已有授权 |
| V07.2 client 配对 | rt 拒绝不匹配 token；sdk/custom 走 shim，验证 full Drive scope |
| V07.3 metadata | token/身份/scopes 端点、header、URL 编码与缓存，提前刷新 |
| V07.4 真实挂载 | My Drive 可列出且实际 mount 成功，检查正确的用户身份 |
| V07.5 跨账户 | A 的 VM 挂载 B 的 Drive，token 来源、结果和日志明确，权限不串用 |
| V07.6 持续刷新 | 超过初始 access token 有效期仍可读测试文件，刷新撤销能诊断 |
| V07.7 只读 | 写入被拒绝，原文件不变 |
| V07.8 清理保护 | 卸载失败后不得 rm 挂载目录；两个挂载只停止所属进程 |
| V07.9 参数与敏感数据 | 特殊字符 mountpoint、extra flags、本地 dry-run/错误/日志不泄露 token |
| V07.10 断线重试 | 挂载已成功但回复丢失，重连先检查结果，不直接重复杀进程/删目录 |

真实 Drive 测试只使用新建的随机测试目录与小型文件；不对现有用户目录运行 force/purge。只读探针不满足写入与只读拒绝测试，必须记录实际覆盖。

## 真实测试资源账本

测试开始前记录 profile、已有 assignment 集合、测试 operation ID 与可用前提。每次成功分配立即记录 endpoint 和 notebook identity，发生超时则保留结果未知记录并调和。

finally 请求释放所有本测试创建的 VM，然后重新获取 assignments，确认新增集合为空。发现不属于本次测试的 VM 仅报告，不自动停止。若释放失败，输出具体账户与 endpoint，并保留恢复日志；不以本地 sessions 已清空作为成功依据。

真实账号授权只在被授权的测试范围内执行；本轮文档研究没有启动这些测试。

## V08 外部调度历史矩阵（退出当前验收）

本节随 supervisor 草案归档，不再作为 CLI 发布门槛。原版 WebSocket、心跳、断线状态、token 刷新和资源清理仍按 V03–V06 验收。历史完整矩阵见 [保活监控与自动恢复](08-supervision-and-recovery.md)。要求覆盖网络故障不重建、token 刷新、重复 controller、分配结果未知、stop 竞争、预算跨重启不清零、半写检查点、主机睡眠及旧 VM 重新出现。

真实验收分别记录“连接可恢复”“VM 受控删除后可重建”“任务从完整检查点继续”。这些结果均不能单独证明防止 Colab 闲置回收；保活效果需要独立服务端证据。本轮新增项未实现或执行测试。

## 风险与有意差异登记

| ID | 静态发现 | Rust 目标 | 验收 |
| --- | --- | --- | --- |
| F01 | ADC 探测通过临时进程 env，锁外判断可串账户 | 显式每账户认证上下文 | V02.5 |
| F02 | loopback handler 未显式比较 state | 强制 state、一次性 pending 与过期 | V03.2/V03.4 |
| F03 | PKCE 未显式开启，不能仅凭 code_verifier 字段认定 | 显式 S256 并检查请求 | V03.3 |
| F04 | 卸载失败后仍可能删除 mountpoint | mount 确认消失前拒绝删除 | V07.8 |
| F05 | remount 清理所有匹配进程 | 按账户与挂载实例记录 PID/启动身份 | V07.8 |
| F06 | 连接错误重试整个执行代码 | 发送后结果未知先检查，不重放 | V05.5/V07.10 |
| F07 | teardown 可吞错误、删本地记录并提示成功 | 保留 endpoint 与 CleanupPending | V04.8 |
| F08 | exec 与 run 错误退出码不同 | 最终对齐文本 exec 返回 0；JSON 扩展错误返回 1，run 按脚本结果 | V01/V05.9 |
| F09 | edit 将任意下载失败视为新文件 | 仅 404 可新建 | V04.7 |
| F10 | rename 多存储步骤无完整回滚 | 可恢复迁移事务 | V02.8 |
| F11 | gpu 与 tpu 同时给定时 TPU 优先 | 最终按兼容要求保留 TPU 优先，不再以早期冲突建议作为验收目标 | V01.1/V01.2 |
| F12 | Python update/.pth 修复耦合安装环境 | Rust 发行更新与内建诊断 | V01.6 |
| F13 | SSH proxy 公钥解析/握手发生在 finally 之前 | VM 创建后立即安装覆盖握手的清理 guard | V06 |
| F14 | 基线 session/extra_params 构造参数未被依赖消费 | 显式设置 WS 与消息 session 身份 | V05.7/U05 |

F 项是基于代码的风险或差异决策，不宣称已在 Google 环境复现。具体证据见 [账户文档](02-accounts-auth-storage.md)、[运行时文档](03-runtime-and-wire-protocols.md) 和 [Drive 文档](04-drivefs.md)。

## 待验证问题与关闭条件

| ID | 问题 | 关闭条件 |
| --- | --- | --- |
| U01 | 三个仓库组合后的实际 CLI 解析 | 已固定合并版 83 路径的命令/参数/help，见 [33](33-merged-help-and-aliases-implementation.md)；另有 [41](41-account-inventory-implementation.md) 的账户 list/current 离线业务对照及 [42](42-account-mutations-implementation.md) 的变更命令及 [43](43-account-selection-implementation.md) 的选择优先级及 [44](44-merged-inventory-implementation.md) 的会话/用量 loopback 对照，Drive show/clear、缺凭据 token 及七组实际刷新/whoami 协议对照见 [57](57-merged-drive-credentials-implementation.md)，九组 consent 起始路由/URL 见 [58](58-drive-consent-profile-implementation.md)；十二组实际合并版文件请求、下载字节与错误对照见 [75](75-merged-files-and-json-bytes.md)，数值精度与扩展十八组见 [76](76-json-number-download-compatibility.md)；其余合并版错误/JSON 与业务 fixtures 仍待补齐 |
| U02 | OAuth client/redirect/scopes 当前接受情况 | 每个目标 flavor 完成一次真实授权，保存脱敏参数与结果 |
| U03 | ADC 身份支持范围 | 对承诺支持的身份逐项完成获取 token 与实际 Colab 请求 |
| U04 | Python/Rust keyring 与锁互操作 | 两种客户端真实进程读写/竞争相同条目通过 |
| U05 | session_id 在锁定依赖中的传递效果 | 固定构造探针已确认传入值未被消费；Rust 实际 loopback 核对持久化/query/消息头一致，历史真实 CPU 跨命令变量通过，见 [传递证据](03-runtime-and-wire-protocols.md)。固定基线传递问题已澄清；没有推定服务端要求特定身份，也不代表全部重连/断线恢复通过 |
| U06 | DEFAULT JSON/二进制消息实际覆盖 | 已完成真实 CPU 的 DEFAULT TEXT/BINARY 采集、原始 Rust/上游 decoder 对照及脱敏样本重放，含非 UTF-8 与空 buffer，见 [67](67-live-codec-capture-and-replay.md)；其他时序/协议问题仍按各自条目核对 |
| U07 | 当前 DriveFS 私有 flags 和 shim | 记录 VM 版本、二进制版本/哈希，完成挂载和持续刷新 |
| U08 | rt 模式真实可用账户 | 配对 client 的 refresh token 完成真实挂载与续期 |
| U09 | 账户/组织政策与共享 Drive 限制 | 承诺环境分别测试；未测试环境不标支持 |
| U10 | Linux/macOS 全链路 | 各平台执行凭据、文件、终端、SSH、锁、发行测试 |
| U11 | Windows 是否纳入 | 明确范围并完成原生矩阵；仅 SSH 通过不能关闭 |
| U12 | 源码复用授权 | 扩展许可得到可核验说明，决定独立行为实现或授权复用范围 |
| U13 | 自更新与分发 | 已选择 GitHub Releases，并实现清单校验、权限限制、一代回滚与候选构建；公开仓库已创建，发布和四平台运行待验收，见 [32](32-native-update-and-release-implementation.md) |
| U14 | 兼容强度 | 对 JSON/schema/退出码/文案分别选等级并冻结差异清单 |
| U15（归档） | 服务允许的保活/续约机制是否存在且有效 | 外部调度研究项，不作为 CLI 门槛；协议心跳仍需验证 |
| U16（归档） | supervisor 重建判定与用户停止意图 | 外部调度职责，不作为 CLI 门槛 |
| U17（归档） | 自动任务恢复的持久化与副作用 | 具体任务与外部调度职责，不作为 CLI 门槛 |

## 完成判据

某项能力只有在对应离线用例及必要的真实测试通过后，才可标记为完成。Mock、skip、缺凭据退出、作者 README 声明均不能替代该项真实验收。

整个 Rust 重写的完成条件是：功能矩阵无遗漏；有意差异已登记；目标平台通过；承诺的 ADC/OAuth/Drive 路径有真实证据；所有测试创建的 VM 已确认释放或明确登记待清理；安装无需本地 Python。原版 WebSocket、协议心跳、断线状态与资源清理按 V03–V06 验收；已归档的 V08 不作为完成门槛。文档完成只代表研究规格形成，不代表这些条件已经满足。

核心 DriveFS mount/umount 已接入持久计划、跨账户租约与凭据回收，离线恢复验证见 [24](24-drivefs-lifecycle-implementation.md)。真实 Drive 授权/FUSE、RT 内部轮换与多 helper 同时轮换仍是验收缺口。

文件/URL external_account 已完成合成 STS/可选 IAM 与 CLI 接线验证，见 [36](36-adc-external-account-implementation.md)。可执行与 AWS 来源的后续离线实现见 39/40；X.509 workload 证书来源和 STS/IAM mTLS 的本地 TLS 验收见 [71](71-x509-adc-and-mtls.md)。metadata mTLS 的本地传输和模式接线见 [72](72-metadata-mtls.md)；证书绑定 token、其余 ADC 与真实 Google 接受性仍有缺口，V03.8 尚未全部完成。

Workforce 用户 ADC 的 STS Basic 刷新、token-only 和内存轮换已通过离线验证，见 [37](37-adc-workforce-user-implementation.md)；未取得真实 Workforce/Google 接受性证据。

metadata ADC 默认文件缺失后的发现与缓存、显式文件保护和离线诊断已完成 loopback 验证，见 [38](38-adc-metadata-implementation.md)；真实 GCE/GKE 与 metadata TLS/绑定 token 仍未验收，V03.8 仍未整体关闭。

Unix 非交互可执行 ADC 的显式 opt-in、缓存、分词和进程组回收已离线验证，见 [39](39-adc-executable-implementation.md)。没有运行真实 IdP 程序或确认真实 Google 接受性；V03.8 仍未全部完成。

AWS aws1 环境/EC2 IMDS 与 SigV4 已完成合成协议、独立签名与本机优化二进制验证，见 [40](40-adc-aws-implementation.md)。真实 AWS/GCP federation 与 Colab 接受性仍未确认，V03.8 尚未整体完成。

Jupyter/SSH 的 HTTP CONNECT、SOCKS5/SOCKS5h 环境代理已离线接入，HTTP 与 WS 共用 SOCKS 来源的实际 CLI 测试见 [49](49-websocket-socks-proxy-implementation.md)。成功代理 TLS/证书、真实企业代理、HTTPS 代理和系统代理发现仍未验收；ClientHello 起始字节不能替代 TLS 成功证据。

HTTPS 代理及成功的 loopback 双层 TLS 后续见 [50](50-websocket-https-proxy-implementation.md)：代理与端点独立验证证书链、主机名及过期证书，证据使用内存临时 CA，不关闭验证。此前代理类型实现缺口已补齐，但真实企业/公开 CA 代理、跨平台和系统代理发现仍待验证，不能用该本地 TLS 结果关闭平台或线上验收项。

ADC 配额项目的来源、环境覆盖、IAM 与 runtime 头隔离已完成纯库/loopback 验证，见 [52](52-adc-quota-project-implementation.md)。这不证明真实 IAM 配额角色、Colab 对项目头的接受性或计费行为；V03.8 与 U03 仍未整体关闭。

控制面同一客户端的 OAuth/ADC 续期和只读查询 401 最多一次重试已离线接入，见 [53](53-control-credential-refresh-implementation.md)。合成并发、锁复用和 CLI 清理测试不证明真实 Google 的权限、scope 或令牌拒绝语义，也不关闭整个认证矩阵。

U10 后续已取得 Linux ARM64 的隔离非 root 容器证据：首次为 16 项原生发行检查；当前实现另行复验，Rust 1.85.1 两种特性编译、稳定版 466/464 项 Rust 测试、74 项 Python 工具及 17 项发行检查通过，见 [54](54-linux-container-validation.md)。默认 ignored 的五项专用 Secret Service 测试不计本次通过。尚无宿主桌面凭据库、x86_64 运行和真实 Colab SSH 证据，U10 仍未整体关闭。

U04/V03.6 后续已取得 Linux 私有 GNOME Secret Service 的真实客户端证据：Python keyring/Rust 原生后端互通、锁定、无响应和恢复均通过，见 [55](55-native-secret-service-validation.md)。仅五项显式隔离测试实际执行；默认 ignored 不算验证。完整原版、宿主桌面和跨实现/跨平台互通仍待验收，Python/Rust 配置并发写入仍禁止。

U10 另取得 macOS x86_64 的 Rosetta 证据，见 [59](59-macos-x86-rosetta-validation.md)：两种特性 466/464 项测试、Clippy 及实际打包后运行通过。宿主是 Apple Silicon，已确认进程翻译；此结果不替代 Intel 原生硬件、最低 macOS 或桌面钥匙串验收，U10 仍未整体关闭。

V06 的文件后端历史预检查失败后，用户明确选择系统钥匙串；后续真实创建一台 CPU，五项 SSH 检查通过，条件清理后再次查询后端确认释放，见 [56](56-live-ssh-validation-and-guarded-cleanup.md)。报告分别保留两种认证来源，不将钥匙串成功外推为文件登录或完整 V06 通过。
