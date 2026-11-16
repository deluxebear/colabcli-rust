# run 生命周期、历史与导出实现

更新日期：2026-11-16。本章记录 P3 的 run、历史事件与导出。依赖 [12](12-jupyter-and-execution-implementation.md) 的执行核心，采用 [01](01-features-and-cli.md) 和 [03](03-runtime-and-wire-protocols.md) 的生命周期语义；真实 Colab 执行和释放复查仍未验收。

## 命令与脚本语义

```sh
colab run --gpu T4 --env MODE=train -s training train.py --epochs 5
colab run --keep -s retained job.py --env THIS_IS_A_SCRIPT_ARGUMENT
colab run --json job.py
colab runs
colab run-cleanup RUN_UUID
colab log
colab log -s training --type execution --lines 10 --json
colab log -s training -o training.ipynb
colab log -s training -o training.md
colab log -s training -o training.txt
colab log -s training -o training.jsonl
```

run 的所有客户端选项必须在脚本路径前；从脚本路径开始，后续参数原样作为 sys.argv，不再提取同名 --env/--gpu 等选项。这是对上游参数歧义的明确修正。脚本可以使用 shebang `#!/usr/bin/env -S colab run --gpu T4`；上传执行前移除首行 shebang。没有脚本时 clap 返回参数错误，不触发认证或申请资源。

[PreparedRun](../src/runs.rs) 在分配前读取 UTF-8 普通文件，校验环境变量、会话名和执行消息大小。脚本、basename、argv 和一次性结果 nonce 通过 JSON 字符串传输给远端，不把参数插入 Python 语法。setup 独立设置 /content 与请求的环境变量；包装器用 compile/exec 在独立 globals 中执行原脚本，设置 __name__='__main__'、__file__=basename 和 sys.argv，结束后恢复原 argv。该路径保留脚本 future import 和原文件行号；不使用 IPython 的逐行 shell/magic 语法解释脚本。

包装器在 finally 发出带 nonce 的专用 MIME 结果，用于区分正常退出与 SystemExit。SystemExit(None/0) 为 0，整数按 POSIX 进程低八位处理（如 -1 为 255），非整数为 1 并写 stderr；其他异常保留 traceback，返回 1。仅在结果已确认且唯一匹配 nonce 时采信脚本退出码；超时/断线、缺少或重复结果均不能认定成功。内部结果不打印，也不作为普通 execution output 写历史；run_completed 另存脚本退出码与清理结果。

run --json 返回 RunOutcome：run_id、account、session、endpoint、exit_code、execution、execution_error、cleanup、cleanup_error、kernel_cleanup_error、history_error。普通脚本输出保持 stdout/stderr，生命周期消息发 stderr。已确定的非零脚本退出码不被清理失败覆盖；脚本为 0 而 VM 清理失败时命令返回 1，JSON 中 exit_code 仍为脚本的 0，cleanup 明确为 pending。

## 所有权、清理与中断

run 总是分配新 VM，不按同名旧会话路由，也不覆盖既有会话。分配前保存逐账户 `runs/<UUID>.json`，包含账户、会话名、硬件、keep、创建时间与可选 endpoint；不保存脚本正文、argv、环境变量或 token。控制面 allocation 日志另带 owner_id，取得 endpoint 后 sessions 保存 run_owner，用于清理归属核对。运行期间持有逐 run 文件租约，run-cleanup 无法与活跃 run 并发释放；未处理的计划阻止账户 rename/remove（包括 force）。

分配成功后，任何 kernel 创建、WS 建立、执行或本地提交失败都会进入同一清理路径。非 keep 模式先重新取得账户凭据与会话 proxy，尽力 DELETE 已记录的 kernel，再调用控制面释放本次 endpoint；kernel DELETE 失败不会跳过 VM 释放。仅删除本次 endpoint 对应本地记录；其他同名会话或未归属本次 run 的资源不作为回收目标。普通 exec 的 kernel/VM 保留语义不受影响。

cleanup 为 not_allocated、confirmed_released、kept 或 pending。释放失败保留控制面清理意图及 run 计划，不能只删本地记录或输出“已停止”。成功释放或明确 keep 后删除 run 计划；计划本地删除失败单独报告，不改写脚本退出码。--keep 在执行失败/中断后也保留已分配 VM，用户应通过 status/stop 核查并处置。

run 使用 signal-hook 将 SIGINT/SIGTERM/SIGHUP 写入取消标志，WS 等待循环观察后停止提交后续代码，进入清理，未确认结果按 128+signal 返回（如 SIGTERM=143）。HTTP 请求、TLS 建立、DNS 和分配退避并非即时可取消，需等本次操作返回后进入清理；SIGKILL、断电不保证当前进程清理，依靠持久计划恢复。

`runs` 是所选账户的离线计划列表。`run-cleanup UUID` 只调和和清理，永远不执行脚本或申请 VM；可在本地会话已裁剪后依据计划中的 endpoint 再查后端。若分配响应丢失且尚无 endpoint，保留 nbh/owner 的控制面日志，先使用同一账户、名称及硬件运行 new 调和，再运行 run-cleanup；该恢复不自动重放原脚本。计划可从 runs 查看，不能手删日志绕过保护。

## 历史格式与可靠性

[history.rs](../src/history.rs) 新增日志在 `accounts/<profile>/history/rust-v1/<SHA256(session-name)>.jsonl`。会话名不参与目录拼接，因此含斜线等名称也不会越界。兼容读取旧 `history/<safe-name>.jsonl`，不修改旧正文；新旧日志按旧在前、新在后合并查询。账户重命名会复制既有历史，读取时以当前账户归属为准。

事件包含 timestamp、event_type、event_id、account、session、可选 endpoint 和业务字段。已接入 session_created、session_terminated（释放/后端消失）、execution、file_operation（ls/upload/download/rm/edit）及 run_completed。execution 保存用户源代码、聚合输出、msg_id、状态、来源、cell 索引与执行次数；run 保存原脚本，不写 JSON 参数包装器或客户端 --env 值。后续 stdin/自动化功能的更多事件需随其功能继续接入。

追加顺序是：持有账户生命周期与历史文件锁，写原子 `.append.json`（原长度和完整行），再追加/fsync，最后移除追加意图。中断后校验原尾部是目标行前缀，恢复完整行；已完整写入但意图尚在时不重复追加。未登记的截断行、损坏 JSON、长度或尾部不匹配直接报错，不静默丢弃数据。Unix 目录/文件权限沿用 0700/0600，路径拒绝符号链接。事件 ID 令控制面恢复与执行事件重试可去重。

生命周期历史失败保留 allocation/release 日志，重试提交历史不会再次申请 VM；普通文件操作已成功但历史失败，返回 HistoryAfterOperation，明确告知操作已完成。execution 的历史失败保存在 ExecutionResult.history_error，不将已完成代码伪装成未执行，也不自动重放。run 的历史错误单独记录。这里不提供事务性“远端请求与本地历史恰好一次”的承诺。

单事件上限 48 MiB，查询累计上限 64 MiB，可用 --lines 限制。逐行读取有长度上限；过滤 event_type 后截取末尾 N 条，N=0 返回空数组。log 不指定会话时列出有历史的名称；指定会话且无显式/环境账户时沿历史归属解析，active 有同名历史优先，多账户歧义报错。所有 log/runs 查询不访问凭据库或 Google。

## 脱敏与导出

写入、读取和导出递归屏蔽 token、refresh/access/proxy/XSRF token、client_secret、Authorization、password、secret、api_key、client_config 等结构化字段，移除完整 URL 的 userinfo，并屏蔽敏感 query；input_reply 的 value 始终脱敏，旧历史只在返回时屏蔽，原文件保留。应用从不将控制面 token、runtime URL 凭据或授权 pending 正文传给历史接口。

用户代码、普通文本输出、图片与 traceback 按用户内容保存；无法据此保证识别用户自行写入代码或打印的任意秘密，导出前仍应自行审阅。运行计划不含 --env；用户脚本本身打印出的环境值属于输出内容。

导出扩展名决定格式，统一先校验再原子写入，不覆盖目录或符号链接：

| 格式 | 行为 |
| --- | --- |
| jsonl | 完整脱敏事件，每行一个 JSON |
| ipynb | execution 变成 code cell，其他事件为 markdown；保留 stderr、错误、MIME、执行次数，移除 transient，额外 buffers 留在 metadata |
| md | 代码与文本输出使用可容纳内容中反引号的围栏；PNG/JPEG 验证 base64 后嵌入 data URI |
| txt | 时间、事件、代码和文本/traceback；二进制 MIME 请使用 ipynb/jsonl |

输出 Notebook 使用 nbformat 4.5、独立 UUID cell ID，保留执行状态与消息身份元信息，不把历史自动执行一遍。

## 验证与完整目标缺口

新增 [history 测试](../tests/history.rs) 10 项、[run 单元与恢复测试](../tests/runs.rs) 7 项；[Jupyter 测试](../tests/jupyter.rs) 增加 4 项 run 端到端，包含 argv 字符串/参数隔离、keep、握手失败清理、退出码 7 与释放失败、cleanup-only 恢复，以及真实子进程 SIGTERM=143。控制面另增历史失败调和测试。并修正旧控制面 loopback 测试在 macOS 继承 nonblocking socket 导致的偶发 WouldBlock。

[probe_run_wrapper.py](../tools/probe_run_wrapper.py) 直接提取 Rust 包装模板，用本地 Python 与假的 IPython.display 执行八个合成用例；[结果](../research/run-wrapper-probe.json) 记录源码哈希、__main__/argv/file/future、SystemExit 和普通/编译错误验证。Python 只用于研究工具，Rust CLI 与 Cargo 测试不要求本地 Python。

完整默认测试 139 项、no-default-features 138 项通过，fmt、两种特性 clippy、默认 build 和研究文档静态检查通过。没有读取开发者真实 Google 凭据或创建 VM；本地 Python 包装测试也不能证明 Colab/IPython 实际行为。

整个目标仍包含 P4–P7 的 REPL/交互 stdin、Colab 专用授权、SSH、DriveFS、完整诊断/分发和 supervisor/任务恢复，以及所有真实集成门槛。当前不能宣称“整个设计文档已实现”或无限保活可用；下一步继续按这些缺口推进。
