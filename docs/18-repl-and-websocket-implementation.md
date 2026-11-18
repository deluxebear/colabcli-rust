# REPL 与 WebSocket 心跳实现

更新日期：2026-11-17。本文记录当前实现；Unix SSH 已见 [19](19-ssh-implementation.md)；扩展 DriveFS、真实长期会话和跨平台验收仍未完成。

## 输入、状态与退出

```sh
colab repl -s cpu-demo
printf 'x = 7\nprint(x)\n' | colab repl -s cpu-demo
colab repl -s cpu-demo --output-image plot.png
```

只连接已有 VM，复用普通 exec 的 kernel_id 和 session_id。先确认或创建 kernel，并持久化 ID，再打开 WebSocket；创建、重启、执行或自动化的结果未知时沿用现有恢复保护。初始化工作目录 `/content` 后进入 REPL。管道输入一次发送全部代码，空白管道仍完成初始化，与官方一致。输入为 UTF-8，管道保留既有 32 MiB 文件上限和单消息编码限额；本地路径与编码检查在认证前执行。

自动化可以用 `colab --colab-runtime-endpoint ENDPOINT repl -s NAME` 限定现有 VM。交互和管道模式共用同一检查。显式名称及其 endpoint 在连接前核对，凭据刷新后再次核对，防止连接到被替换的同名会话。有效代理凭据可直接用于运行时连接。

交互模式使用内存行编辑历史（最多 100 条）、Unicode 输入和简单 Python 词法高亮。Enter 提交，Ctrl-J 或 Alt-Enter 插入换行；不根据语法自动提交或补齐代码。空白忽略，`/quit`、`quit()`、`exit()` 忽略大小写退出。Python 异常展示后继续接受下一条代码；EOF 干净退出。`NO_COLOR` 或 `TERM=dumb` 禁用高亮。配色、续行布局与原版 Pygments 不逐像素一致。

每个交互 REPL 持有一个 endpoint 执行租约、一条 WebSocket 和同一个 kernel；编辑线程通过容量 1 的命令通道与容量 4 的输出通道通信，网络线程串行读写，有界通道提供背压。等待本地输入时继续读取 WebSocket、响应 PING、检查关闭与 dead/restarting；迟到输出不会作为下一条请求的结果。普通 Python 错误不销毁连接。退出关闭 socket，不 DELETE kernel、不释放 VM；变量在 kernel 中保留。状态查询不持有整个 REPL 的账户锁；旧 generation 的写回仍会被拒绝。

每条执行发送前写入 running/msg_id 和 last_execution，取得本次 execute_reply 与 idle 后清除 running，记录执行输出和历史来源。交互 last_execution 来源为 REPL，管道为 stdin，管道历史标记 source=piped。结果未知时保留 running 或 execution_pending，返回非零；后续 exec/REPL 不自动重放，需要显式核查或重启 kernel。交互等待输入时不把 running 写成长期的 `repl` 字符串，避免进程退出后把空闲 kernel 错判为未完成执行；与官方状态展示的差异在此保留。

REPL 与官方一样默认不设置执行期限；exec 原有默认空闲期限仍为 30 秒。文本 Python 异常退出契约保持 0，传输、恢复保护和结果未知错误为非零。图片可通过 --output-image 保存，沿用有界解码与原子写入；HTML/Markdown 文本转换见 [46](46-rich-text-output-implementation.md)，终端 PNG 与默认图片文件见 [47](47-terminal-image-output-implementation.md)；完整富文本终端排版及 JPEG 转码仍未实现。REPL 未开放远端 input/getpass；需要远端输入时使用 exec -f --allow-stdin。

## Ctrl-C 与清理责任

输入阶段 Ctrl-C 清除当前编辑内容，不发送远端代码。执行阶段第一次 Ctrl-C 发送一次 REST interrupt；HTTP 成功本身不算执行结束，仍须在 REST 返回后的 5 秒内取得本次 execute_reply 与 idle。确认后可继续 REPL。REST 失败、缺失确认、再次中断或 TERM/HUP 都结束等待；未确认的执行保持 Unknown，不能自动重放。REST 自身受请求期限约束，不能把 5 秒确认期限理解为所有网络阶段的总期限。

Unix 上空闲断线和终止信号会唤醒行编辑器并恢复终端；网络线程先排入结束结果，再唤醒输入线程，防止把恢复后的输入发送出去。行编辑器只在读取本地输入时存在，结束后恢复执行阶段的信号处理器。使用 rustyline 17.0.2，关闭磁盘历史和 home 探测；不启用该版本存在批量输入阻塞问题的 ExternalPrinter 路径，也不为使用新版编辑依赖提高 Rust 1.85 基线。

Windows 尚未做原生终端验收；编辑中发生后台错误会提示用户按 Enter/EOF 返回，不能声称与 Unix 一样立即唤醒。进程强杀、操作系统崩溃、DNS 和部分 TLS/HTTP 阶段的即时取消仍不作保证。清理仅关闭连接和归还客户端租约，不自动重建 VM 或恢复用户任务。

## 协议心跳

KernelRuntime 默认每 60 秒发送小型 WebSocket PING，执行循环和 REPL 空闲循环均维护；对服务端 PING 自动回复 PONG。与固定上游的默认间隔一致；协议心跳不重置代码执行的空闲期限，不成为用户输出，不代表续租 VM 或防止 Colab 回收。没有另行设置 PONG 逾期重连策略；依靠底层传输错误、关闭、执行期限及 kernel 状态判断连接失败。不会在断线后自动补发 execute_request。

库提供非零心跳间隔配置，用于受控调用和快速 loopback 测试。若库调用者自行保持空闲连接，须持续调用 poll_idle；单纯持有对象不会启动后台心跳线程。CLI REPL 的网络线程负责调用，不要求用户脚本额外轮询。持续保活策略、自动申请替代 VM 与任务检查点由外部脚本或调度器决定，见 [08](08-supervision-and-recovery.md)。

## 验证与边界

[jupyter 集成测试](../tests/jupyter.rs) 新增七项，覆盖空闲 PING/PONG、主动心跳不延长执行期限、中断 REST 失败/缺失确认/确认后复用、单 kernel 单 socket 的连续执行与 Python 错误、未知结果阻止后续 exec、空白/错误/中文管道，以及真实 PTY 的 Ctrl-J、Alt-Enter、中文、历史、两阶段 Ctrl-C、EOF、退出命令、空闲断线、SIGTERM 和终端回显恢复。另有词法高亮原文保留测试。

[差分工具](../tools/compare_cli.py) 新增六个非 TTY REPL 行为场景和 help。交互按键由 Rust PTY 测试验证，尚未对两版逐事件驱动比对。该阶段默认 188 项、无默认特性 186 项测试通过，152 个官方差分场景无断言失败；fmt、两种特性的 Clippy、默认构建与文档检查通过。测试均使用合成账户和 loopback 服务，没有创建新 Colab VM；此前的单次真实 CPU 记录不能视为本次 REPL、主动心跳或中断机制的真实验收。Linux/Windows、实际远端中断、长期代理 token 过期和长时间心跳仍需后续验证。

2026-11-18 加入 endpoint 条件后，以 `--only '^repl-'` 重跑固定官方 CLI 对照，六个管道场景均一致。默认及无默认特性各通过 87 项相关测试。真实 PTY 复验在使用系统钥匙串查询基线时返回 `inventory_credentials_or_account_error`。随后保持同一二进制哈希，仅重试只读查询，确认等待系统钥匙串 30 秒超时。分配步骤未开始，创建 VM 数为零。交互按键、实际远端 Ctrl-C 和 EOF 的线上复验仍待执行。

### 真实 CPU 交互复验

后续在 macOS ARM64 上，现有系统钥匙串通过控制面预检查。用真实 CPU 的 REPL 确认了输入阶段 Ctrl-C 丢弃、中文多行定义、Python 异常后继续执行、执行阶段 Ctrl-C、同一 kernel 继续使用、EOF 退出和终端回显恢复。退出后 `exec` 仍能读取 REPL 中定义的变量。一次静默执行在 63.335 秒完成，超过配置的 60 秒心跳间隔。该测试确认执行与连接在这段时间内正常，PING/PONG 帧由 loopback 测试观察。首次线上探针误把提示符重绘当成远端异常结束，等待实际异常输出后复验通过。两次测试 VM 均独立确认释放。其他终端和平台、长时间空闲连接及代理凭据轮换仍待验收。详见 [脱敏报告](../research/live-cpu-repl.json)。
