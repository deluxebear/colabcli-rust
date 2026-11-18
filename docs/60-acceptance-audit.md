# 全范围验收核对与未完成项

本章按目标“实现整个设计文档中的功能”核对当前证据，不将后续目标改写成已完成的子集。依据为 [功能契约](01-features-and-cli.md)、[架构阶段](05-rust-architecture.md)、[验收矩阵](06-validation-and-open-questions.md)、当前代码、测试和脱敏报告。范围仍含账户/认证、控制面、文件、执行/WebSocket、SSH、Drive、兼容与发行；外部 supervisor、VM 自动重建和任务检查点已由用户决定退出 CLI 范围。

下表的“已有证据”仅说明可定位到相应验证；列出测试文件不表示文件中的每条断言覆盖了该要求全部输入。离线测试不能替代明确要求的线上行为，历史真实 CPU 报告也不自动证明当前所有构建。全部 V 条目有对应行，整体完成仍不成立。

## V01–V07 逐项核对

| 要求 | 已有证据 | 尚不能据此证明的部分或后续验收 |
| --- | --- | --- |
| V01.1 | [83 路径合并帮助](../research/merged-help.json)、[CLI 对照](../research/cli-comparison.json) | 帮助/采样参数不是所有业务输入等价；合并专项使用不同阶段构建 |
| V01.2 | [argv/环境](../tests/cli_compat.rs)、[run/SSH](../tests/ssh.rs) | 未采样的参数组合仍不能概括为 Click 完全等价 |
| V01.3 | [URL 对照](../research/merged-url-comparison.json)、[SSH 字节流](../tests/ssh.rs)、[真实 SSH](../research/live-ssh-validation.json)、[汇总](../tests/merged_inventory.rs) | 单次 macOS ARM64 真实 SSH 二进制 stdio 通过；其他平台、故障和所有输入未覆盖 |
| V01.4 | [PTY/图片](../tests/jupyter.rs)、[SSH TTY](../tests/ssh.rs) | 实际图形终端像素显示、其他系统终端未覆盖 |
| V01.5 | [非交互登录](../tests/cli.rs)、[部分账户失败](../tests/inventory.rs) | 原生桌面凭据提示仍需宿主验证 |
| V01.6 | [更新与回滚](../tests/update.rs)、[每日检查](../tests/update_daily.rs)、[GitHub 验收入口与预检查](32-native-update-and-release-implementation.md#github-release-发布后演练) | 公开清单预检查返回 HTTP 404，尚无真实 GitHub Release 下载与安装证据 |
| V02.1 | [选择与同名优先](../tests/accounts.rs)、[合并选择](../research/merged-account-selection.json)、[历史路由](../research/merged-history-routing.json) | 实际账户/组织政策不能由本地选择测试证明 |
| V02.2 | [分配路由](../tests/control_plane.rs)、[run 碰撞](../tests/runs.rs) | 多真实账户 new/run 的服务器结果未验收 |
| V02.3 | [Drive 优先级](../tests/accounts.rs)、[consent 路由](../research/merged-drive-consent.json) | 跨真实账户的 Drive 授权与挂载仍缺证据 |
| V02.4 | [账户](../tests/accounts.rs)、[历史](../tests/history.rs)、[Drive](../tests/drive_credentials.rs) | 合成隔离不代表所有真实账户认证来源均通过 |
| V02.5 | [ADC 双来源并发](../tests/auth.rs)、[quota/请求身份](../tests/adc_quota.rs) | 两个真实 ADC 身份的 Colab 接受性未知 |
| V02.6 | [存储与失败保护](../tests/storage.rs)、[CLI 多进程](../tests/cli.rs) | 任意断电时刻/文件系统故障未穷举；Python/Rust 并发混用禁止 |
| V02.7 | [旧文件迁移](../tests/storage.rs)、[原生条目迁移](../research/secret-service-validation.json) | macOS 桌面钥匙串 ACL/完整原版迁移仍待验收 |
| V02.8 | [重命名恢复](../tests/accounts.rs)、[后端策略](../tests/account_mutations.rs) | 真实桌面后端的各失败阶段不由内存替身覆盖 |
| V02.9 | [删除保护](../tests/accounts.rs)、[合并变更](../research/merged-account-mutations.json) | 已按仅删资料语义验证，不承诺停止远端 VM |
| V02.10 | [路径/符号链接/权限](../tests/storage.rs)、[大小写冲突](../tests/accounts.rs) | 未取得所有文件系统和 Windows 路径原生证据 |
| V03.1 | [OAuth 流程](../tests/auth.rs)、[CLI 两步](../tests/account_login.rs) | 各目标 OAuth client/redirect/scopes 的新真实授权尚未完成 |
| V03.2 | [state/callback](../tests/auth.rs)、[Drive state](../tests/drive_credentials.rs) | Google 实际 callback 路径仍需真实授权验证 |
| V03.3 | [URL 与交换 verifier](../tests/auth.rs)、[两端 custom URL S256](../research/merged-drive-consent.json) | 不能外推所有 flavor、Google 授权码交换接受性 |
| V03.4 | [持久 pending/期限/错误保护](../tests/auth.rs)、[Drive 完成边界](../tests/drive_credentials.rs) | 尚缺新真实 OAuth 两步流程证据 |
| V03.5 | [实际 scope 保存](../tests/auth.rs)、[Drive scope 来源](../tests/drive_credentials.rs) | 服务器实际授予及缺权限挂载诊断仍待线上验证 |
| V03.6 | [真实私有 Secret Service](../research/secret-service-validation.json)、[超时/文件策略](../tests/credentials.rs)、[macOS 钥匙串复查](69-macos-keychain-code-identity.md#减少-run-生命周期内的重复访问) | macOS 已用原条目执行真实 inventory 和 run；首次读取历史偶发超时尚未复现，原生互通和 ACL 未闭环 |
| V03.7 | [轮换/并发/提交失败](../tests/auth.rs)、[控制面续期](../tests/control_refresh.rs) | 实际 Google 撤销、长时间刷新仍缺证据 |
| V03.8 | [ADC 分类型实现](02-accounts-auth-storage.md)、[各 ADC 测试](../tests/adc_external.rs)、[STS 客户端认证回归](../research/external-client-auth-regression-validation.json)、[X.509 本地 TLS 与回归](../research/x509-adc-regression-validation.json) | X.509 workload 的主体/STS/IAM mTLS 已有本地 TLS 与原版合成对照；真实 token/scopes/Colab 四列未完整建立，metadata mTLS 已接入本地双向 TLS/模式验收；绑定 token 等仍有缺口 |
| V04.1 | [loopback 请求](../tests/control_plane.rs)、[当前真实 CPU 控制面](../research/live-cpu-control-current.json) | 已有当前 macOS ARM64 单账户钥匙串 OAuth 的真实分配、查询和释放证据，其他认证组合仍待验收 |
| V04.2 | [identity/hardware 编码](../tests/control_plane.rs) | 客户端固定算法已有局部证据，不能外推未来服务端兼容 |
| V04.3 | [丢失回复同 identity 调和及确定拒绝](../tests/control_plane.rs) | 已验证明确拒绝后可重选硬件，较早 POST 结果未知时保留原标识。真实服务器丢失响应的故障注入未做 |
| V04.4 | [HTTP/损坏响应/读取超时](../tests/control_plane.rs)、[401 续期边界](../tests/control_refresh.rs) | 真实 Google 各状态码/组织拒绝尚未逐项触发 |
| V04.5 | [proxy token 刷新](../tests/contents.rs)、[裁剪](../tests/control_plane.rs) | 当前真实长时间 token 生命周期未验收 |
| V04.6 | [文件边界与编码](../tests/contents.rs)、[当前真实 CPU 往返](../research/live-cpu-files-current.json)、[十八组合并版及数值精度](76-json-number-download-compatibility.md) | 已在线验证带 Unicode 和保留字符的路径、二进制往返及删除。真实上限体积、其余路径和全部 JSON 数值组合仍待采样 |
| V04.7 | [仅 404 新建/编辑失败保护](../tests/contents.rs)、[当前真实 CPU 编辑](../research/live-cpu-files-current.json) | 已真实验证缺失新建、无变更编辑和结果字节。真实认证故障覆盖仍缺 |
| V04.8 | [释放失败及 kernel 关闭](../tests/control_plane.rs)、[endpoint 保护](../tests/guarded_stop.rs)、[当前真实释放](../research/live-cpu-control-current.json) | 具名 `stop` 后已独立确认真实 VM 释放。kernel DELETE 的真实响应、释放失败及待分配时的独立恢复仍缺真实证据 |
| V05.1 | [codec fixtures/坏偏移](../tests/jupyter.rs)、[固定探针](../research/kernel-codec-probe.json)、[真实采集与重放](../research/live-codec-validation.json) | 本条与 U06 已有对应证据：当前 CPU DEFAULT 文本与二进制、非 UTF-8/空 buffer，既有坏帧测试及两种特性重放通过；其他 V05 时序仍独立核对 |
| V05.2 | [channel/parent/stdin](../tests/jupyter.rs)、[真实 Unicode/密码输入](../research/live-cpu-stdin.json) | macOS ARM64 PTY 中的普通输入、密码输入、回显恢复和 JSON/历史隔离已真实验证。等待输入期间的超时、取消和边界时序仍待线上验收 |
| V05.3 | [idle/reply 时序与迟到输出](../tests/jupyter.rs)、[真实断线部分输出](../research/live-disconnect-validation.json) | 单种客户端受控断线已有证据；服务端各消息/迟到时序没有完整线上采样 |
| V05.4 | [相关输出/静默/期限](../tests/jupyter.rs)、[真实 CPU 超时场景](../research/live-cpu-exec-idle-timeout.json)、[原版对照](../research/exec-idle-timeout-comparison.json)、[跨 60 秒的真实 REPL 执行](../research/live-cpu-repl.json) | 真实持续输出、静默超时、`--timeout 0` 和一次跨心跳周期的执行已通过。无关消息与心跳对期限的隔离已有本地测试。长时间空闲连接及代理凭据轮换仍待验收 |
| V05.5 | [握手/断线测试](../tests/jupyter.rs)、[历史真实断线](../research/live-disconnect-validation.json)、[广播 Unknown 回归](../research/kernel-broadcast-regression-validation.json)、[真实 kernel 崩溃](../research/live-kernel-death-validation.json) | 真实生命周期 Unknown、部分输出、同 msg_id 禁止重放、单次副作用和显式重启恢复通过；首次缺输出失败保留，两台 VM 已确认释放；其他时序/故障与平台仍有缺口 |
| V05.6 | [富文本](../tests/display.rs)、[图片](../tests/images.rs)、[实际 CLI/WS](../tests/jupyter.rs) | 历史真实 PNG 文件成功；图形终端视觉、特殊 JPEG 未验收 |
| V05.7 | [持久化/query/header](../tests/jupyter.rs)、[构造探针](../research/kernel-session-probe.json)、[历史跨命令变量](../research/live-cpu-validation.json) | 固定依赖行为已澄清，未证明服务端要求特定 session 或所有重连情况 |
| V05.8 | [notebook metadata/部分输出](../tests/jupyter.rs)、[历史 Notebook](../research/live-cpu-validation.json)、[ID 与格式校验](../research/notebook-cell-identity-validation.json)、[标题对照](../research/notebook-title-compatibility.json) | 本地已验证 ID、标题、输出与历史对应。真实 CPU 校验对应历史构建，Notebook 格式和实际编辑器的完整兼容仍待扩大样本 |
| V05.9 | [脚本结果](../tests/runs.rs)、[run 生命周期](../tests/jupyter.rs)、[本机 kernel 包装器复验](../research/run-wrapper-runtime-validation.json)、[当前真实 CPU 脚本与释放](../research/live-run-keychain-reuse-validation.json)、[真实 keep/后续 exec](../research/live-run-keep-validation.json)、[真实 SIGTERM/143](../research/live-run-signal-validation.json) | 本机 kernel 验证脚本模块、行号与退出后恢复，真实 CPU 已复验脚本模块、dataclass/pickle、argv 和退出 23。其他信号、未知结果、释放失败及长任务仍待线上验收 |
| V05.10 | [PTY REPL/取消/EOF](../tests/jupyter.rs)、[REPL endpoint 条件](../tests/guarded_stop.rs)、[真实远端 REPL](../research/live-cpu-repl.json) | macOS ARM64 真实中文多行、输入取消、远端中断、EOF 与跨命令保留变量已通过。其他终端、平台及更多故障路径仍待验收 |
| V06 | [21 项 SSH](../tests/ssh.rs)、[真实五项 SSH 与释放](../research/live-ssh-validation.json)、[真实 PTY/命令 SIGTERM](../research/live-ssh-extended-validation.json)、[首次失败](../research/live-ssh-extended-first-attempt.json)、[第二次失败](../research/live-ssh-extended-second-attempt.json)、[真实验证脚本](../tools/validate_live_ssh.py) | 单次 macOS ARM64 登录 shell PTY 与命令 SIGTERM 通过；两次先前失败原因未定位，其他交互/信号、故障恢复、长连接及其他平台未覆盖，Windows 未实现原生桥接 |
| V07.1 | [Drive 授权](../tests/drive_credentials.rs)、[consent URL](../research/merged-drive-consent.json) | dfs/sdk/custom 各一次真实授权尚未完成 |
| V07.2 | [精确 scope/rt client](../tests/drive_credentials.rs) | 匹配 client 的真实 rt 挂载/轮换未知 |
| V07.3 | [metadata shim](../tools/tests/test_drivefs_shim.py)、[远端控制器](../tools/tests/test_drivefs_controller.py) | 真实 shim 挂载已接受 metadata（见 [65](65-live-drive-validation-and-recovery.md)）；超过初始 token 有效期刷新仍缺证据 |
| V07.4 | [本地编排](../src/drive_runtime_cli.rs)、[受限目录准备](../research/live-drive-validation-preparation.json)、[执行器及只读 API 回归](../research/live-drive-api-validation.json) | 真实 MyDrive 读写及 API 字节核对已完成，见 [65](65-live-drive-validation-and-recovery.md)；仅同 token 所有权验证，未建立独立 OAuth subject 映射 |
| V07.5 | [跨账户租约/隔离](../tests/drive_credentials.rs)、[生命周期](../tests/jupyter.rs) | 无 A 的真实 VM 挂 B 的真实 Drive，明确未完成 |
| V07.6 | [轮换保存与失败恢复](../research/merged-drive-tokens.json)、[shim](../tools/tests/test_drivefs_shim.py)、[刷新诊断](23-drivefs-probe-and-recovery.md) | 可控时钟和合成 OAuth 验证了旧 token 到期、`invalid_grant` 脱敏诊断与后续刷新恢复。真实 Drive 挂载超过初始 access token 有效期后的文件读取与真实撤销仍待验收 |
| V07.7 | [只读参数与远端控制器](../tools/tests/test_drivefs_controller.py)、[只读探针本地安全验证](../research/live-drive-preparation-validation.json) | 真实只读挂载写入被拒绝、原文件字节不变已通过，见 [65](65-live-drive-validation-and-recovery.md)；仍限本次运行环境 |
| V07.8 | [拥有关系/卸载失败保护](../tools/tests/test_drivefs_controller.py)、[编排](../tests/jupyter.rs) | 无真实 FUSE 卸载失败和双挂载隔离故障注入 |
| V07.9 | [凭据输入/参数/日志](../tests/drive_credentials.rs)、[私有远端输入](../tests/jupyter.rs) | 真实 DriveFS 私有 flags 接受性未验收 |
| V07.10 | [未知操作先核查/不重放](../tests/jupyter.rs)、[控制器](../tools/tests/test_drivefs_controller.py) | 无真实挂载成功但回复丢失后的恢复证据 |

## 其他交付与阶段门

| 范围 | 当前证据与未完成事项 |
| --- | --- |
| P0 契约 | 合并帮助、固定源码和多类协议 fixtures 已有；U01 仍缺部分合并业务，不能由静态清单替代 |
| P1 基础 | 本地隔离/恢复和私有 Linux Secret Service 有证据；宿主钥匙串、真实 OAuth/各 ADC 未闭环 |
| P2 控制面/文件 | 当前构建已真实复验 CPU 分配、查询、释放及文件往返、编辑和删除。其他 provider、真实上限尺寸及线上故障分支仍待验收 |
| P3 执行 | 真实 CPU 已覆盖静默超时、持续输出、`--timeout 0`、跨心跳周期的 REPL 执行，以及 `run` 非零退出、`--keep` 和 SIGTERM。受控断线及 kernel 崩溃已有 Unknown 与恢复证据。长期空闲、其他信号和更多断线时序仍待验收 |
| P4 交互/SSH | 真实 macOS PTY 已覆盖 REPL、远端中断及普通和隐藏输入。已有五项真实 Colab SSH 验收和一次真实登录 shell PTY/命令 SIGTERM 成功，两次先前失败原因未定位。其他 SSH 交互和信号、其他平台及 VM auth/官方 Drive 授权仍待验收 |
| P5 Drive | 真实读写、只读、API 字节及服务端清理已验证，进程/快照/上传恢复经过见 [65](65-live-drive-validation-and-recovery.md)；跨真实账户、持续刷新、rt 模式及其他环境仍缺证据 |
| P6 分发 | 原生更新与回滚已做本地演练。四目标本地候选已按验收报告核对打包字节与校验和。源码推送、四平台远程候选构建、Release 发布和实际下载仍待验收 |
| V08/P7 | 按已接受职责划分归档，不将其作为本 CLI 完成门槛；WebSocket/心跳/未知结果保护仍需按 V05/V06 验收 |
| 平台 | ARM64 macOS、Linux ARM64 隔离容器通过；x86 macOS 是 Rosetta 翻译执行。[Linux x86_64](64-linux-x86-container-validation.md) 已通过隔离模拟执行；Intel/AMD 原生硬件与 Windows 尚无同等运行证据 |
| 许可 U12 | 扩展固定仓库无根 LICENSE；未取得可核验复用授权结论，不能凭仓库可读推定可发布复制代码 |
| ADC 边界 U03 | authorized_user/service_account/metadata/impersonation/多种 external account 有实现和合成测试；[Workforce 用户来源的服务账号模拟](37-adc-workforce-user-implementation.md#workforce-用户作为服务账号模拟来源)已通过 loopback。X.509 workload 和 metadata mTLS 有本地 TLS 证据，绑定 token 及相应真实权限仍待验收 |
| 兼容 U14 | [当前固定原版全量对照](14-cli-comparison.md#2026-11-18-全量复验)包含 370 个场景，91 一致、60 有差异、219 Rust 扩展，契约断言通过。合并专项与线上验收各自保留证据 |

## 当前下一步与外部前提

核对时文件认证预检查为 inventory_credentials_or_account_error，created_sessions=0。后续用户明确选择现有系统钥匙串，该来源预检查通过，并已完成一台 CPU 的五项真实 SSH 验收、后端确认释放与临时密钥清理，见 [56](56-live-ssh-validation-and-guarded-cleanup.md)。因此真实 SSH 的缺认证阻塞已解除；文件后端仍没有登录，不改写历史结果。后续另有一次真实登录 shell PTY 与命令 SIGTERM 通过，但两次先前失败原因未定位；完整 V06 的其他交互/故障、长连接及其他平台仍待验收。

后续 [Drive 离线元数据检查](../research/live-drive-offline-preflight.json) 发现一份现有资料，具备 client/refresh/user 字段并声明完整 Drive scope，但没有刷新或访问云盘。后续用户已授权现有资料及单个随机目录内两文件的真实测试，实际读写、只读与云端清理结果见 [65](65-live-drive-validation-and-recovery.md)。授权范围前提已解除；rt client 配对、跨账户、长期刷新与独立 subject 映射仍不能由本轮证明。ADC 不同云身份、原生其他平台以及发布/许可也各有外部条件，不能通过更改“完成”的定义省略。

本章将当前证据映射和缺口冻结，便于后续逐项关闭；不是整体验收通过，也不是重新执行全部测试。原始证据及各自二进制/源码哈希保留，快照汇总见 [验收核对记录](../research/acceptance-audit.json)。

后续 [63](63-drive-live-validation-executor.md) 补齐受控执行器与 endpoint 锁内保护，默认/无默认 Rust 471/469 项和 20 项合成流程测试通过。本地准备核对没有实际云盘访问，V07 各项真实证据及独立身份仍未关闭。

当前读取中断修正的 macOS 474/472、Linux x86_64 模拟执行 474/472（默认另有 5 ignored）和 153 工具测试证据见 [64](64-linux-x86-container-validation.md)。它补充平台和传输本地证据，不关闭真实认证/Drive 或完整 U10。
