# Jupyter 连接与执行核心实现

更新日期：2026-11-16。本章记录 P3 的第一部分：Jupyter REST、DEFAULT WebSocket、Python/Notebook 执行、显式终端输入与恢复边界。协议依据见 [03](03-runtime-and-wire-protocols.md)，P3 的 run、history 与导出已见 [13](13-run-and-history-implementation.md)，不能将整个设计标记完成。

## 已交付命令

```sh
printf 'x = 7\nprint(x)\n' | colab exec -s cpu-demo
colab exec -s cpu-demo -f task.py --env MODE=train --timeout 30
colab exec -s cpu-demo -f experiment.ipynb --output-image plot.png
colab exec -s cpu-demo -f task.py --json
colab exec -s cpu-demo -f interactive.py --allow-stdin --timeout 120
colab kernels -s cpu-demo --json
colab exec -s cpu-demo -f task.py --kernel-id EXISTING_KERNEL_ID
colab restart-kernel -s cpu-demo
```

exec 接受 stdin、UTF-8 `.py` 或 nbformat 4 `.ipynb`，-s 及账户路由沿用既有上下文。无文件且 stdin 为 TTY 时报错；空白输入成功退出，不认证或创建 kernel。所有命令仅访问已有 VM，不隐式申请 VM。环境参数为可重复 KEY=VALUE；名称限 ASCII Python 标识符格式，禁止 NUL 值；参数验证、文件读取、Notebook 结构及单消息编码限额检查在认证前完成。

执行前单独发送设置工作目录 `/content` 和环境变量的请求，再原样发送用户代码；环境映射通过 JSON 字符串交给远端 json.loads，不拼接参数为 Python 源码。独立设置请求避免破坏 `from __future__` 和末尾表达式的显示语义。环境设置对本次文档执行一次，后续 cell 可显式修改环境；不会在每个 cell 前强制恢复，这是与上游逐 block 环境前缀的差异。

文本模式流式透传 stdout/stderr，MIME 按 Markdown、HTML、纯文本优先级进行文本显示，错误 traceback 单独渲染；纯图片可用 --output-image 保存，最后一个 PNG/JPEG bundle 覆盖该文件。HTML/Markdown 文本转换及原始数据保留见 [46](46-rich-text-output-implementation.md)；终端 PNG 与默认图片文件见 [47](47-terminal-image-output-implementation.md)；尚无 JPEG 终端转码或完整富文本终端排版。--json 输出请求结果数组，包含 msg_id、status、outputs、reply、error，不混入流式文本；成功的内部环境设置请求不列入数组，设置失败时单独返回该结果。

普通 Python/kernel error 在文本 exec 中与官方一致退出 0，异常内容仍写 stderr；JSON 扩展模式退出 1。结果未知仍返回非零并给出消息 ID。run 的脚本退出码不受文本 exec 的兼容策略影响。

## REST、WebSocket 与凭据

[jupyter/runtime.rs](../src/jupyter/runtime.rs) 提供 kernels 列表、创建、GET、restart、interrupt 与 DELETE 的库接口。CLI 暴露 kernels、restart-kernel 和仅清理待恢复自动化的 automation-cleanup；interrupt 与任意 kernel DELETE 仍只为库接口。新 kernel POST 为 name=python3、path=content；已有 kernel ID 先 GET 校验，不会遇到 404 就自动创建替代实例。

auth/drivemount 使用独立 kernel，普通 exec 的 kernel/session 字段保持不变；自动化恢复与显式清理见 [16](16-colab-automation-implementation.md)。

HTTP 与 WS 的 Authorization 均使用 runtime proxy token，同时发送 X-Colab-Runtime-Proxy-Token 与 X-Colab-Client-Agent；不发送 Google 控制面 OAuth。保留 runtime base path，WS 路径为 `/api/kernels/<encoded-id>/channels`，追加真实持久化 session_id 与 token query。选择 DEFAULT，不请求 V1 subprotocol；禁止跟随 HTTP/WS 重定向。错误不包含 URL、proxy token 或原始响应正文。

REST 连接超时 5 秒，总超时沿用请求配置，响应上限 4 MiB。WS 使用 tungstenite 0.28、rustls 与 WebPKI 根证书；TCP 尝试每地址最多 5 秒，TLS/upgrade 受请求期限限制。系统 DNS 解析尚无独立可取消超时，WS 的 HTTP CONNECT 环境代理已接入，选择、认证隔离与限制见 [48](48-websocket-http-proxy-implementation.md)。执行期间每次读取最多阻塞约 100 ms，以便检查执行期限；PING 自动回复 PONG，不进入用户输出；执行及 REPL 空闲循环每 60 秒主动 PING，库的空闲调用者需轮询 poll_idle。

当前采用单拥有者的同步读取循环，&mut runtime 串行执行并对输出处理施加背压，普通 exec 没有建立后台多请求 reader；REPL 以单网络线程和有界通道连接编辑线程。该实现覆盖当前执行和可轮询终端输入路径；dfs_ephemeral 专用请求通过可轮询处理器接入，见 [16](16-colab-automation-implementation.md)；REPL 详见 [18](18-repl-and-websocket-implementation.md)，独立的 Unix SSH 桥接见 [19](19-ssh-implementation.md)。

## 编解码、输出与完成条件

[jupyter/codec.rs](../src/jupyter/codec.rs) 实现 DEFAULT TEXT/BINARY：无 buffers 发 TEXT，有 buffers 发 big-endian u32 数量与 offsets 的 BINARY。严格验证 N 为 2–65、首 offset、单调性、长度与 UTF-8 JSON；单条 WS 消息/帧上限 8 MiB。原始 buffer 保留为 Vec<u8>，不会强制解码为字符串。header、metadata、content 与 parent 结构校验，未知通道报受控错误，未知消息类型仍可由 KernelMessage 保留原始 envelope。

[jupyter/output.rs](../src/jupyter/output.rs) 聚合 stdout/stderr、execute_result、display_data、update_display_data 与 error；支持同 display_id 的多处更新，clear_output(wait=true) 延后到下一次新增输出。相较上游忽略 wait，这是有意修正。MIME、metadata、transient 与原始 buffers 在运行时保留，累计输出上限 32 MiB、10000 条。文本终端不能撤回已打印内容，但最终 JSON/Notebook 反映更新及清空后的输出状态。

一次执行必须同时取得本次 parent msg_id 的 IOPub idle 与 shell execute_reply；reply 先到也会保留并继续等 idle。不把其他请求的输出计入本次结果，不用第一个 stream 或 busy 作为完成信号。reply=error 而没有 error output 时补齐错误输出；aborted 视为失败。

--timeout 默认为相关消息的空闲期限 30 秒，<=0 无限制；持续相关 IOPub 输出可以超过总 timeout。无关请求、PING/PONG 和 idle 后的 IOPub 不会延长等待 shell reply 的期限。显式 --allow-stdin 使用 Deadline，表示每个 execute_request 的总期限（包含输入等待）；默认非交互模式保持 Idle 语义。收到无 parent 的 dead/restarting 广播立即终止等待。

发送 execute_request 后的超时、断线、损坏帧或不支持的 stdin 请求会产生 Unknown，保留 msg_id、部分输出与错误，且不自动重放代码；Unknown 不代表远端已经取消。默认 allow_stdin=false，可显式启用终端输入；只有 auth/drivemount 配置 dfs_ephemeral 处理器，其他路径或未知类型收到时结束为 Unknown，避免默默等待到无限期。

### 真实 CPU 空闲期限验收

2026-11-18 在 macOS ARM64 上使用 Colab CPU DEFAULT 复验。设置 5 秒空闲期限时，远端每秒打印一次、运行约 9 秒，CLI 收到九条输出并退出 0。`--timeout 0` 等待静默 7 秒的代码正常完成。设置 3 秒空闲期限时，预设静默 15 秒的代码返回 `unknown`，CLI 退出 1。CLI 端到端耗时包含认证与连接。首次验证器读取了错误的 JSON 字段，修正后重新执行了三个场景，两次测试 VM 和本地状态均已确认清理。loopback CLI 回归进一步验证超时保留同一 msg_id，正常完成清除执行标记。详见 [脱敏报告](../research/live-cpu-exec-idle-timeout.json)。

固定官方 CLI 的 loopback 对照中，持续输出和 `--timeout 0` 的退出码与输出一致。静默超时两版均退出 1，Rust 在 stderr 报告结果未知及消息 ID，官方版打印 Python `TimeoutError` 堆栈。跨 60 秒心跳周期的静默执行另在真实 REPL 中通过。详见 [原版对照](../research/exec-idle-timeout-comparison.json)和 [REPL 复验](18-repl-and-websocket-implementation.md#真实-cpu-交互复验)。

后续补齐文本 `exec` 的超时诊断。stderr 会明确显示设置的秒数和空闲期限，`--allow-stdin` 显示总期限，并提示可用 `--timeout` 调整。消息 ID 与禁止自动重放的提示继续输出。JSON 模式沿用原有结果结构。重新对照固定原版后，两端的静默超时退出码、配置秒数和调整提示均通过断言，stderr 文案仍有差异。详见 [超时诊断对照](../research/exec-timeout-diagnostic-comparison.json)。

## 显式终端输入

`exec -f script.py --allow-stdin` 是 Rust 的显式入口扩展，也可用于 Notebook。它在认证前要求 stdin 为 TTY 并打开终端；禁止把代码管道或重定向内容误当远端回复。未设置此选项时不启用 stdin，原非交互行为保持。当前终端后端支持 Unix；Windows 返回明确不支持，尚未完成平台验收。

运行时只处理 parent ID 属于本次执行的 stdin/input_request，校验 prompt 字符串和 password 布尔值。每次请求调用非阻塞 InputHandler::poll；返回 None 时继续读取 socket、响应 PING、检查期限和取消信号。有值时发送 stdin/input_reply，保留请求 header 作为 parent_header，使用当前 session。重叠/畸形请求、输入失败、超时或发送结果未知均结束为 Unknown 并保留原有禁止重放保护，不伪装成已取消远端代码。

终端密码输入关闭 ECHO/ECHONL，正常回复、错误返回和受控 SIGINT/SIGTERM/SIGHUP 后恢复终端设置；SIGKILL 等无法执行清理的情况不作保证。提示写 stderr，JSON stdout 保持结构化。客户端不把 stdin 回复添加到历史、Notebook 或执行结果；用户代码若主动打印输入值，其输出仍按普通输出处理。处理器错误使用固定脱敏提示，不保存可能包含密码的原始错误。输入行限 1 MiB。

库保留原 execute/execute_with_checkpoint/execute_document API；新增 execute_with_input 与 execute_document_with_input 供交互和后续专用流程复用，setup 请求始终保持非交互。stdin 的普通字符串回复不等于 Colab credentials-propagation 专用回复，后者通过 auth/drivemount 专用处理器实现，见 [16](16-colab-automation-implementation.md)。

2026-11-18 在 macOS ARM64 的真实 PTY 上，用一台专用 Colab CPU 运行 `exec -f … --allow-stdin --json`。远端 Python 分别调用 `input()` 和 `getpass.getpass()`，通过摘要核对随机 Unicode 文本和合成密码。终端普通输入回显正常，密码提示期间关闭回显，退出后恢复。执行 JSON 及 `log --json` 都包含成功标记，输入原文均未写入。测试绑定本次 endpoint，完成后已独立查询确认 VM 释放。线上期间取消和输入超时仍待实测。详见 [脱敏结果](../research/live-cpu-stdin.json)。

## 持久化、并发与恢复

[execution.rs](../src/execution.rs) 在 POST kernel 前先持久化 kernel_start_pending；取得 ID 后立即保存，再建立 WS。POST 或本地 ID 提交失败时保留恢复线索，不盲目重复创建。使用 kernels 查询当前 VM，再通过 exec --kernel-id 显式选择；仅采用该 VM GET 校验通过的 ID。WS 建立失败则保留已取得的 kernel ID，下一次只重新 GET/连接。

实际 Jupyter session ID 按官方 jupyter_client 的 8+24 十六进制分组生成，在连接前持久化，WS query 与消息 header 使用同一 ID。每个请求发送前 checkpoint running=msg_id；已确认完成后清除，Unknown 时在默认文本模式转存 execution_pending 并清除 running；库接口与 JSON 模式可继续保留 running。两种形式都拒绝后续执行，要求先核查或显式 restart-kernel。last_execution 保存输入来源、cell ID 与时间，不把 --env 值复制进会话元信息。

长执行持有按账户和 endpoint 派生的独立文件租约，账户生命周期锁只用于短暂读取和 checkpoint。其他终端仍能 status、stop 或 restart；同一 endpoint 的并发 exec 不会重复建 kernel 或并行提交。checkpoint 核对账户存在、名称、endpoint 和 kernel_generation，只合并执行字段，保留并发刷新的 proxy token/url/硬件。已停止或替换的会话不能被旧 writer 重新创建。

restart-kernel 在请求前记录 kernel_restart_pending，并生成新 kernel_generation。CLI 先连接/复用 runtime 再发送 restart，与官方顺序一致；没有已知 kernel 时可显式初始化，但创建结果未知时仍拒绝重复创建。只有 REST 确认成功后才清除 running、execution_pending 和重启 pending；失败保留待核查状态，显式重试 restart-kernel。旧 generation 的执行即使稍后回包，也不能覆盖重启后的状态。Notebook 默认文本路径对齐官方的多行数组表示、执行进度和 execution_count；JSON/库路径保留增强的执行次数与未知结果元数据。普通断开/Drop 关闭 socket。显式 `stop` 先尝试关闭已记录的 kernel，再释放 VM，见 [控制面释放流程](10-control-plane-implementation.md#操作日志与并发)。

## Notebook 与产物

输入 Notebook 保留未知根字段、metadata 和 markdown/raw cell。输出至少为 nbformat 4.5，以容纳 cell ID，较新的次版本号会保留。合法 ID 保留首次出现，缺失、空值字符串、重复或格式不合法的字符串会获得新 UUID。非字符串 ID 仍报错。只执行非空 code cell，错误或 Unknown 后不再提交后续 cell。结果写入同目录 `<stem>_output.ipynb`，输入文件保持原样，后续连接或请求失败时也尝试保存已有 cell 输出。

执行历史记录输出 Notebook 中实际使用的 `cell_id`，以及从零开始的原 Notebook `cell_index`。混合 cell 的部分输出、重复 ID 修复和历史索引通过本地 WebSocket 验证。详见 [Notebook ID 验证](../research/notebook-cell-identity-validation.json)。

Notebook 的执行进度与会话最近执行记录都优先采用符合上游规则的 `@title`，无标题时使用 cell ID。历史仍保存 cell ID 以便定位。七组标题语法通过固定上游表达式对照，实际 CLI 也通过标题显示与会话状态的验证。详见 [标题对照](../research/notebook-title-compatibility.json)。

文本兼容模式写回 Notebook 时按原版 nbformat 处理换行，保留 CRLF、单独 CR 等分隔符。文本 MIME、SVG、JavaScript 和附件中的对应文本采用行数组。PNG 等二进制 MIME 与 JSON 对象保留原始表示。实际 CLI 测试使用本地 WebSocket 覆盖输出和附件写回。详见 [验证记录](../research/notebook-line-compatibility.json)。

正常输出保留 MIME bundle，运行时 transient display ID 不写入 nbformat output；二进制 buffers 按输出索引以 base64 保存到 cell.metadata.colab_cli_output_buffers。Unknown 在 cell.metadata.colab_cli_execution 中保存 msg_id 和原因；成功重跑时移除旧的 Unknown 标记。保存采用现有原子写入，目录/符号链接目标拒绝。

## 验证与完整目标缺口

[jupyter 测试](../tests/jupyter.rs) 新增 27 项：固定 Python codec fixture 对照、非 UTF-8/空 buffers、畸形 offsets、输出更新/清空与错误合成、两种 reply/idle 顺序、相关/无关流量、独立总期限、idle 后 reply 超时、PING/PONG、断线部分输出、无 parent 重启、REST 契约、kernel/session 持久化与复用、创建/WS 失败恢复、显式接管、Notebook MIME/buffers/部分输出、环境参数与 CLI 流式输出、空输入、并发 stop、刷新合并与 generation 防旧写回、消息与输出数量限额。

完整默认测试 225 项、no-default-features 223 项通过；fmt、两种特性的 clippy、build 与研究文档静态检查通过。测试仅使用临时目录、替身凭据和 loopback HTTP/WS，后续真实 CPU、远端 Python 与绘图验收见 [15](15-live-cpu-validation.md)。

run 的 SystemExit/argv/自动清理、history/log 和导出见 [13](13-run-and-history-implementation.md)；REPL 已见 [18](18-repl-and-websocket-implementation.md)，Colab 专用交互已见 [16](16-colab-automation-implementation.md)；Unix SSH 已见 [19](19-ssh-implementation.md)；DriveFS 和完整发行继续保留在完整目标中。原版 WebSocket 协议、心跳和断线状态处理仍在范围内；supervisor 与任务恢复已移至外部调度层，见 [08](08-supervision-and-recovery.md)。单次 CPU、Jupyter 与变量保留已单独验收，GPU/TPU 与长任务仍待验证，离线通过不能替代这些门槛。

SOCKS5/SOCKS5h 环境代理已随后接入，共享 HTTP/WS 的代理路径；DNS 模式、认证和验证范围见 [49](49-websocket-socks-proxy-implementation.md)。

HTTPS 代理连接与独立的双层 TLS 已接入，库调用者可显式添加代理 CA；默认验证、类型变化及 loopback 证据见 [50](50-websocket-https-proxy-implementation.md)。

JPEG 终端显示已补 PNG 投影转换，原始保存、JSON、Notebook 不替换字节；限制与 CLI/PTY 验证见 [51](51-jpeg-terminal-projection-implementation.md)。

后续单次真实执行中断线已验证未知状态持久化、保留部分输出、阻止后续 exec、原脚本继续一次和显式重启恢复，见 [61](61-live-execution-disconnect-validation.md)。其范围为 TLS 透明代理切断本次进程的 runtime 连接，不能代表 dead/restarting、所有消息时序或长期运行；这些仍需分别验收。
