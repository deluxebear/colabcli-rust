# Jupyter 连接与执行核心实现

更新日期：2026-11-16。本章记录 P3 的第一部分：Jupyter REST、DEFAULT WebSocket、非交互 Python/Notebook 执行与恢复边界。协议依据见 [03](03-runtime-and-wire-protocols.md)，P3 的 run、history 与导出已见 [13](13-run-and-history-implementation.md)，不能将整个设计标记完成。

## 已交付命令

```sh
printf 'x = 7\nprint(x)\n' | colab exec -s cpu-demo
colab exec -s cpu-demo -f task.py --env MODE=train --timeout 30
colab exec -s cpu-demo -f experiment.ipynb --output-image plot.png
colab exec -s cpu-demo -f task.py --json
colab kernels -s cpu-demo --json
colab exec -s cpu-demo -f task.py --kernel-id EXISTING_KERNEL_ID
colab restart-kernel -s cpu-demo
```

exec 接受 stdin、UTF-8 `.py` 或 nbformat 4 `.ipynb`，-s 及账户路由沿用既有上下文。无文件且 stdin 为 TTY 时报错；空白输入成功退出，不认证或创建 kernel。所有命令仅访问已有 VM，不隐式申请 VM。环境参数为可重复 KEY=VALUE；名称限 ASCII Python 标识符格式，禁止 NUL 值；参数验证、文件读取、Notebook 结构及单消息编码限额检查在认证前完成。

执行前单独发送设置工作目录 `/content` 和环境变量的请求，再原样发送用户代码；环境映射通过 JSON 字符串交给远端 json.loads，不拼接参数为 Python 源码。独立设置请求避免破坏 `from __future__` 和末尾表达式的显示语义。环境设置对本次文档执行一次，后续 cell 可显式修改环境；不会在每个 cell 前强制恢复，这是与上游逐 block 环境前缀的差异。

文本模式流式透传 stdout/stderr，MIME 的 text/plain 和错误 traceback 单独渲染；纯图片可用 --output-image 保存，最后一个 PNG/JPEG bundle 覆盖该文件。暂不实现终端图片或完整富文本渲染。--json 输出请求结果数组，包含 msg_id、status、outputs、reply、error，不混入流式文本；成功的内部环境设置请求不列入数组，设置失败时单独返回该结果。

普通 Python/kernel error 退出 1；结果未知同样非零，并给出消息 ID。该错误退出行为是 [01](01-features-and-cli.md) 登记的 Rust 有意差异，不宣称与上游所有退出码完全一致。

## REST、WebSocket 与凭据

[jupyter/runtime.rs](../src/jupyter/runtime.rs) 提供 kernels 列表、创建、GET、restart、interrupt 与 DELETE 的库接口。CLI 当前暴露 kernels、restart-kernel；interrupt 与单独 kernel DELETE 仅为库接口。新 kernel POST 为 name=python3、path=content；已有 kernel ID 先 GET 校验，不会遇到 404 就自动创建替代实例。

HTTP 与 WS 的 Authorization 均使用 runtime proxy token，同时发送 X-Colab-Runtime-Proxy-Token 与 X-Colab-Client-Agent；不发送 Google 控制面 OAuth。保留 runtime base path，WS 路径为 `/api/kernels/<encoded-id>/channels`，追加真实持久化 session_id 与 token query。选择 DEFAULT，不请求 V1 subprotocol；禁止跟随 HTTP/WS 重定向。错误不包含 URL、proxy token 或原始响应正文。

REST 连接超时 5 秒，总超时沿用请求配置，响应上限 4 MiB。WS 使用 tungstenite 0.28、rustls 与 WebPKI 根证书；TCP 尝试每地址最多 5 秒，TLS/upgrade 受请求期限限制。系统 DNS 解析尚无独立可取消超时，WS 尚未接入 HTTP 代理配置。执行期间每次读取最多阻塞约 100 ms，以便检查执行期限；PING 自动回复 PONG，不进入用户输出。

当前采用单拥有者的同步读取循环，&mut runtime 串行执行并对输出处理施加背压，没有建立后台多请求 reader/有界 channel 分派器。该实现覆盖当前非交互执行路径；后续 stdin/Colab 专用请求、并发控制、REPL 和 SSH 仍需扩展架构。

## 编解码、输出与完成条件

[jupyter/codec.rs](../src/jupyter/codec.rs) 实现 DEFAULT TEXT/BINARY：无 buffers 发 TEXT，有 buffers 发 big-endian u32 数量与 offsets 的 BINARY。严格验证 N 为 2–65、首 offset、单调性、长度与 UTF-8 JSON；单条 WS 消息/帧上限 8 MiB。原始 buffer 保留为 Vec<u8>，不会强制解码为字符串。header、metadata、content 与 parent 结构校验，未知通道报受控错误，未知消息类型仍可由 KernelMessage 保留原始 envelope。

[jupyter/output.rs](../src/jupyter/output.rs) 聚合 stdout/stderr、execute_result、display_data、update_display_data 与 error；支持同 display_id 的多处更新，clear_output(wait=true) 延后到下一次新增输出。相较上游忽略 wait，这是有意修正。MIME、metadata、transient 与原始 buffers 在运行时保留，累计输出上限 32 MiB、10000 条。文本终端不能撤回已打印内容，但最终 JSON/Notebook 反映更新及清空后的输出状态。

一次执行必须同时取得本次 parent msg_id 的 IOPub idle 与 shell execute_reply；reply 先到也会保留并继续等 idle。不把其他请求的输出计入本次结果，不用第一个 stream 或 busy 作为完成信号。reply=error 而没有 error output 时补齐错误输出；aborted 视为失败。

--timeout 默认为相关消息的空闲期限 30 秒，<=0 无限制；持续相关 IOPub 输出可以超过总 timeout。无关请求、PING/PONG 和 idle 后的 IOPub 不会延长等待 shell reply 的期限。库另有独立 Deadline 类型表示整次请求期限，未混用于 CLI 的默认空闲语义。收到无 parent 的 dead/restarting 广播立即终止等待。

发送 execute_request 后的超时、断线、损坏帧或不支持的 stdin 请求会产生 Unknown，保留 msg_id、部分输出与错误，且不自动重放代码；Unknown 不代表远端已经取消。当前 allow_stdin=false；Colab 专用请求尚无处理器，收到时也结束为 Unknown，避免默默等待到无限期。

## 持久化、并发与恢复

[execution.rs](../src/execution.rs) 在 POST kernel 前先持久化 kernel_start_pending；取得 ID 后立即保存，再建立 WS。POST 或本地 ID 提交失败时保留恢复线索，不盲目重复创建。使用 kernels 查询当前 VM，再通过 exec --kernel-id 显式选择；仅采用该 VM GET 校验通过的 ID。WS 建立失败则保留已取得的 kernel ID，下一次只重新 GET/连接。

实际 Jupyter session ID 在连接前持久化，WS query 与消息 header 使用同一 ID。每个请求发送前 checkpoint running=msg_id；已确认完成后清除，Unknown 时保留，并拒绝后续执行，要求先核查或显式 restart-kernel。last_execution 保存输入来源、cell ID 与时间，不把 --env 值复制进会话元信息。

长执行持有按账户和 endpoint 派生的独立文件租约，账户生命周期锁只用于短暂读取和 checkpoint。其他终端仍能 status、stop 或 restart；同一 endpoint 的并发 exec 不会重复建 kernel 或并行提交。checkpoint 核对账户存在、名称、endpoint 和 kernel_generation，只合并执行字段，保留并发刷新的 proxy token/url/硬件。已停止或替换的会话不能被旧 writer 重新创建。

restart-kernel 在请求前记录 kernel_restart_pending，并生成新 kernel_generation。只有 REST 确认成功后才清除 running 和 pending；失败保留待核查状态，显式重试 restart-kernel。旧 generation 的执行即使稍后回包，也不能覆盖重启后的状态。普通断开/Drop 仅关闭 socket，不删除 kernel 或 VM。stop 当前仍通过控制面直接释放整个 VM；run 的单独 Jupyter teardown 与清理见 [13](13-run-and-history-implementation.md)。

## Notebook 与产物

输入 Notebook 保留未知根字段、metadata 和 markdown/raw cell；缺失或空的 cell ID 生成 UUID。只执行非空 code cell，错误或 Unknown 后不再提交后续 cell。结果写入同目录 `<stem>_output.ipynb`，不覆盖输入文件；后续连接/请求失败时也尝试保存已有 cell 输出。

正常输出保留 MIME bundle，运行时 transient display ID 不写入 nbformat output；二进制 buffers 按输出索引以 base64 保存到 cell.metadata.colab_cli_output_buffers。Unknown 在 cell.metadata.colab_cli_execution 中保存 msg_id 和原因；成功重跑时移除旧的 Unknown 标记。保存采用现有原子写入，目录/符号链接目标拒绝。

## 验证与完整目标缺口

[jupyter 测试](../tests/jupyter.rs) 新增 27 项：固定 Python codec fixture 对照、非 UTF-8/空 buffers、畸形 offsets、输出更新/清空与错误合成、两种 reply/idle 顺序、相关/无关流量、独立总期限、idle 后 reply 超时、PING/PONG、断线部分输出、无 parent 重启、REST 契约、kernel/session 持久化与复用、创建/WS 失败恢复、显式接管、Notebook MIME/buffers/部分输出、环境参数与 CLI 流式输出、空输入、并发 stop、刷新合并与 generation 防旧写回、消息与输出数量限额。

完整默认测试 139 项、no-default-features 138 项通过；fmt、两种特性的 clippy、build 与研究文档静态检查通过。测试仅使用临时目录、替身凭据和 loopback HTTP/WS，没有执行真实 Google 请求或验证远端 Python 结果。

run 的 SystemExit/argv/自动清理、history/log 和导出见 [13](13-run-and-history-implementation.md)；P4–P7 的 stdin/交互、SSH、DriveFS、完整发行与 supervisor 继续保留在完整目标中。线上 Jupyter、CPU/GPU/TPU、长任务和真实变量保留仍需独立验收，离线通过不能替代这些门槛。
