# 真实 WebSocket 断线与执行恢复验收

2026-11-18（Asia/Shanghai），使用用户选择的现有系统钥匙串，当前默认 debug 的单次真实 CPU 验收。新增 [验证入口](../tools/validate_live_disconnect.py) 和 [本地安全回归](../tools/tests/test_live_disconnect_validation.py)。本节真实断线验收使用修复前的既有构建，原哈希匹配当时 CLI 对照；后续发现的广播状态修复和新构建回归独立记在末节。这不是完整 V05/P3 验收。

## 输入、状态与断线责任

默认只预检查，不分配。显式 execute 才创建一台随机唯一名称的 CPU；cleanup-only 只处理既有账本，不执行代码。账户、认证来源与配置路径冻结，拒绝已有控制面事务，分配后校验名称、非基线 endpoint 和 accelerator=NONE 并立即写私有账本。不会登录、挂载 Drive、使用已有用户 VM 或停止其他新增资源。

```sh
python3 tools/validate_live_disconnect.py --token-storage keyring --state-dir /tmp/new-private-disconnect --execute
python3 tools/validate_live_disconnect.py --state-dir /tmp/new-private-disconnect --cleanup-only
```

只有本次 exec 子进程的 HTTP/HTTPS 代理环境被改为本地随机端口 CONNECT relay；父环境和其他 CLI 命令保持原配置。relay 只允许所选 runtime URL 的 hostname:443，其他 authority 拒绝；TLS 字节透明转发，不装 CA、不解密、不解析 Jupyter/认证正文。因为 REST 与 WS 共用域名，故障切断该测试进程通过 relay 建立的全部活连接，而不是按加密后的 URL 区分。真实记录中累计建立两条 tunnel；触发时切断所有仍活跃的连接，没有单独记录每条 tunnel 的存活数量，不称作单一 WS frame 故障。

远端脚本仅写 `/content` 下随机测试文件：追加 once、打印随机标记、sleep 四秒、追加 done。只有本地实际收到标记行后才切断 relay 的连接，不能用本地持久化 running 或连接建立代替代码已执行的证据。之后 relay 仍允许并计数新 tunnel，使“未自动重连”可以被观测，不通过禁止重连掩盖实现行为。

执行进程、proxy listener 和线程均有受控回收；所有原始 stdout/stderr 与账户/endpoint 账本只保存在 0700 的私有 state-dir 内，结构化诊断/账本为 0600。任何异常进入 endpoint 条件 stop，随后独立查询后端，不能仅凭本地文件消失或 stop 返回 0 宣称释放。分配回复未知时最多沿同名称/参数调和一次，绝不重放脚本；没有可核实 endpoint 时保留未知状态，不能猜测资源归属。

## 实际结果

[真实结果](../research/live-disconnect-validation.json) 的五项主流程检查通过，另从实际历史及被阻止重试的消息 ID 关联完成一项后续核对：

1. 收到远端标记后切断本进程 relay 的活连接；此前累计建立两条 tunnel，exec 非零退出，未知状态持久化。
2. 本次进程没有新建 tunnel，即没有自动重连或重发代码。
3. 同一脚本再次 exec 被“上次执行尚未确认结束”拒绝，指向相同消息；没有以新的 kernel 或会话绕过保护。
4. 独立 Contents 下载得到精确 once/done 两行，证明原脚本在客户端断线后继续完成，且该观测期内没有执行第二次。它不证明任意用户任务具备 exactly-once 分布式语义。
5. 显式 restart-kernel 清除 pending，之后新 exec 成功并再次断言文件仍只有 once/done；恢复没有重放原脚本。
6. 历史的 status=unknown、error=“执行完成前连接丢失”，保留了故障前的随机 stream 输出；不是把 idle timeout 误报为网络断线。此项为同次真实数据的后续核查，报告分开记录执行版本和后续验证器哈希。

最终 guarded stop 后查询 assignments，确认本次 endpoint 消失，其他新增 assignment 为 0。没有访问 Drive 或修改用户既有资源。CPU 来源是分配响应 NONE，不将其称为独立硬件测量。真实账户、endpoint、消息 ID、随机标记和原始历史不提交。

## 本地回归与剩余范围

[工具回归](../research/live-disconnect-harness-validation.json) 当前包含十二项安全/流程检查，其中最初十项覆盖：允许 authority、65,539 字节透明往返、断线与重连计数、默认预检查、已有事务保护、失败后精确清理、分配未知同参数调和、基线拒绝，以及断线原因/部分输出验证。后续另加 kernel-death 模式的共用归属/清理与失败清理回归，整个 Python 工具测试 91 项中 90 项通过，1 项 Linux 专属在 macOS 跳过。合成本地结果不替代真实报告，也不代表其他平台通过。

本次仅覆盖单账户 macOS ARM64 的一种执行中断线；不覆盖服务端特定帧时序、实际 Google 故障、长连接、token 到期、释放失败恢复、run 断线或交互输入。本次核对时发现 runtime 将 dead/restarting 广播记为 Failed，既有测试也要求此行为；而 [V05.5](06-validation-and-open-questions.md) 的目标包含这些广播下的结果未知保护。该实现差异随后按下节修正；不能用本次 transport Unknown 成功代替广播分支或新构建的线上验收。

## 后续：dead/restarting 未知状态保护

输入是等待执行完成时收到 iopub status 的 dead/restarting 广播，广播可以没有 parent。它只说明 kernel 生命周期发生变化，没有确认本次请求已经完成，也不能说明此前副作用是否发生。因此 [runtime](../src/jupyter/runtime.rs) 现在返回 Unknown，保留部分输出和具体原因，不将它当作普通 Python 异常结果。非 iopub 的同名 status 不终止本次等待。

既有普通 error reply 仍为 Failed；文本 exec 的普通代码错误兼容退出 0，JSON 扩展返回非零。广播 Unknown 则按已有结果未知路径非零退出。持久层沿用 running 或兼容模式的 execution_pending，后续 exec 在发送代码之前被拒绝；只在确认显式 restart-kernel 成功后清除，失败不能抹去保护。不自动重建 VM、重发代码或推断任务检查点。

[定向回归](../tests/jupyter.rs) 同时覆盖 dead/restarting、无 parent 广播、普通 error reply、错误 channel 的 status 被忽略，以及两种输出模式下部分输出和持久化保护。四组完整执行用例检查后续调用确实返回同一个 ExecutionPending，而不是因另一个 HTTP/网络失败偶然没有执行。完整回归结果独立保存在 [广播修复验证](../research/kernel-broadcast-regression-validation.json)。此前真实断线报告保留原构建哈希，不改写成新构建的线上证明。

真实广播验收使用同一验证入口的独立模式，只针对脚本刚创建的专属 CPU kernel：

```sh
python3 tools/validate_live_disconnect.py --token-storage keyring --state-dir /tmp/new-private-kernel-death --execute --fault kernel-death
```

合成脚本只在随机 /content 文件追加 once、打印随机标记，再 os._exit(7)。该模式要求 JSON 明确返回生命周期广播对应的 Unknown 原因并保留标记，不接受普通 timeout/连接丢失冒充“已观测到广播”。随后验证消息一致的持久化保护、Contents 独立读取仅 once 一行，以及显式重启后再次执行成功且不重放崩溃脚本。任何失败仍进入相同 endpoint 条件清理；不会把现有用户 kernel 当作故障注入目标。

本次广播修复后的本机验证最终为：默认/无默认特性 467/465 项 Rust 测试、两种 Clippy、格式检查、365 个 CLI 场景、17 项发行检查及两组非 TTY 图片对照均通过。release 与 debug 分别核对对应报告哈希，其他平台和合并专项保留历史构建证据。真实 kernel-death 模式尚未执行故障注入：新构建 inventory 的钥匙串等待 30 秒超时，之后单独延长至 120 秒的只读检查仍超时，创建 VM 始终为 0，见 [预检查](../research/live-kernel-broadcast-preflight.json)。需要在系统访问窗口完成认证后再继续，不将这项预检查写成广播实测通过。

## 后续真实 kernel 生命周期验收

此前预检查超时保留为历史结果。后续使用继承控制终端的认证方式完成两次尝试：首次缺少部分输出而失败，第二次在预热和一秒发送窗口下通过生命周期 Unknown、输出保留、持久化禁止重放、一次副作用及显式重启恢复；两台 VM 均确认释放。详情及边界见 [68](68-live-kernel-death-validation.md)，不据此关闭其他时序/平台或钥匙串根因。
