# run 生命周期、历史与导出实现

本章记录 P3 的 run、历史事件与导出。依赖 [12](12-jupyter-and-execution-implementation.md) 的执行核心，采用 [01](01-features-and-cli.md) 和 [03](03-runtime-and-wire-protocols.md) 的生命周期语义；单次真实 CPU 执行、历史导出与释放复查见 [15](15-live-cpu-validation.md)。后续已完成一次真实 run 创建/执行/自动释放，结果及范围见本章末节。

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

[PreparedRun](../src/runs.rs) 在分配前读取 UTF-8 普通文件，校验环境变量、会话名和执行消息大小。脚本、basename、argv 和一次性结果 nonce 通过 JSON 字符串传输给远端。setup 独立设置 /content 与请求的环境变量。包装器在临时 `__main__` 模块中使用 compile/exec，设置 `__file__` 和 `sys.argv`，退出时恢复 kernel 的模块和参数。脚本内的 `import __main__`、dataclass 和 pickle 能访问同一模块。读取时去除可选 UTF-8 BOM，清空 shebang 的内容并保留行尾，保持原文件行号。脚本通过 Python 编译，采用它自身声明的 future imports。

包装器在 finally 发出带 nonce 的专用 MIME 结果，用于区分正常退出与 SystemExit。SystemExit(None/0) 为 0，整数按 POSIX 进程低八位处理（如 -1 为 255），非整数为 1 并写 stderr；其他异常保留 traceback，返回 1。仅在结果已确认且唯一匹配 nonce 时采信脚本退出码；超时/断线、缺少或重复结果均不能认定成功。内部结果不打印，也不作为普通 execution output 写历史；run_completed 另存脚本退出码与清理结果。

run --json 返回 RunOutcome：run_id、account、session、endpoint、exit_code、execution、execution_error、cleanup、cleanup_error、kernel_cleanup_error、history_error。普通脚本输出保持 stdout/stderr，生命周期消息发 stderr。已确定的非零脚本退出码不被清理失败覆盖；脚本为 0 而 VM 清理失败时命令返回 1，JSON 中 exit_code 仍为脚本的 0，cleanup 明确为 pending。

## 所有权、清理与中断

run 总是分配新 VM，不按同名旧会话路由，也不覆盖既有会话。分配前保存逐账户 `runs/<UUID>.json`，包含账户、会话名、硬件、keep、创建时间与可选 endpoint；不保存脚本正文、argv、环境变量或 token。控制面 allocation 日志另带 owner_id，取得 endpoint 后 sessions 保存 run_owner，用于清理归属核对。运行期间持有逐 run 文件租约，run-cleanup 无法与活跃 run 并发释放；未处理的计划阻止账户 rename/remove（包括 force）。

分配成功后，任何 kernel 创建、WS 建立、执行或本地提交失败都会进入同一清理路径。CLI 在分配、执行和释放阶段共用绑定该账户的认证客户端。仍有效的访问凭据在进程内复用，临近过期时通过原认证来源刷新。非 keep 模式只释放本次 run 的 endpoint 并删除对应本地记录。普通 exec 保留 kernel 和 VM。

cleanup 为 not_allocated、confirmed_released、kept 或 pending。释放失败保留控制面清理意图及 run 计划，不能只删本地记录或输出“已停止”。成功释放或明确 keep 后删除 run 计划；计划本地删除失败单独报告，不改写脚本退出码。--keep 在执行失败/中断后也保留已分配 VM，用户应通过 status/stop 核查并处置。

自动清理与 `stop` 共用释放路径。已记录 kernel ID 的自有会话先尝试关闭 kernel，VM 释放结果仍由控制面确认。准备关闭 kernel 时若取得或刷新凭据失败，命令保留释放日志与 run 计划，并返回该失败供核查和恢复。[释放流程](10-control-plane-implementation.md)

run 使用 signal-hook 将 SIGINT/SIGTERM/SIGHUP 写入取消标志，WS 等待循环观察后停止提交后续代码，进入清理，未确认结果按 128+signal 返回（如 SIGTERM=143）。HTTP 请求、TLS 建立、DNS 和分配退避并非即时可取消，需等本次操作返回后进入清理；SIGKILL、断电不保证当前进程清理，依靠持久计划恢复。

`runs` 是所选账户的离线计划列表。`run-cleanup UUID` 只调和和清理，永远不执行脚本或申请 VM；可在本地会话已裁剪后依据计划中的 endpoint 再查后端。若分配响应丢失且尚无 endpoint，保留 nbh/owner 的控制面日志，先使用同一账户、名称及硬件运行 new 调和，再运行 run-cleanup；该恢复不自动重放原脚本。计划可从 runs 查看，不能手删日志绕过保护。

## 历史格式与可靠性

[history.rs](../src/history.rs) 新增日志在 `accounts/<profile>/history/rust-v1/<SHA256(session-name)>.jsonl`。会话名不参与目录拼接，因此含斜线等名称也不会越界。兼容读取旧 `history/<safe-name>.jsonl`，不修改旧正文；新旧日志按旧在前、新在后合并查询。账户重命名会复制既有历史，读取时以当前账户归属为准。

事件包含 timestamp、event_type、event_id、account、session、可选 endpoint 和业务字段。已接入 session_created、session_terminated（释放/后端消失）、execution、file_operation（ls/upload/download/rm/edit）及 run_completed。execution 保存用户源代码、聚合输出、msg_id、状态、来源、cell 索引与执行次数；run 保存原脚本，不写 JSON 参数包装器或客户端 --env 值。后续 stdin/自动化功能的更多事件需随其功能继续接入。

追加顺序是：持有账户生命周期与历史文件锁，写原子 `.append.json`（原长度和完整行），再追加/fsync，最后移除追加意图。中断后校验原尾部是目标行前缀，恢复完整行；已完整写入但意图尚在时不重复追加。未登记的截断行、损坏 JSON、长度或尾部不匹配直接报错，不静默丢弃数据。Unix 目录/文件权限沿用 0700/0600，路径拒绝符号链接。事件 ID 令控制面恢复与执行事件重试可去重。

生命周期历史失败保留 allocation/release 日志，重试提交历史不会再次申请 VM；普通文件操作已成功但历史失败，返回 HistoryAfterOperation，明确告知操作已完成。execution 的历史失败保存在 ExecutionResult.history_error，不将已完成代码伪装成未执行，也不自动重放。run 的历史错误单独记录。这里不提供事务性“远端请求与本地历史恰好一次”的承诺。

单事件上限 48 MiB，查询累计上限 64 MiB。命令先按事件类型过滤，再应用 `--lines`。正数取最后 N 条，负数跳过最早的 N 条，省略或传零返回全部。展示和导出使用同一批结果。底层 Rust 查询的 `HistoryQuery.lines=Some(0)` 仍返回空数组。log 不指定会话时列出有历史的名称。指定会话且无显式或环境账户时，先按本地会话归属解析，再按历史归属查找。所有 log/runs 查询均为离线操作。固定合并版的七组文本与一组 JSONL 导出对照见 [兼容结果](../research/log-lines-compatibility.json)。

## 脱敏与导出

写入、读取和导出递归屏蔽 token、refresh/access/proxy/XSRF token、client_secret、Authorization、password、secret、api_key、client_config 等结构化字段，移除完整 URL 的 userinfo，并屏蔽敏感 query；input_reply 的 value 始终脱敏，旧历史只在返回时屏蔽，原文件保留。应用从不将控制面 token、runtime URL 凭据或授权 pending 正文传给历史接口。

用户代码、普通文本输出、图片与 traceback 按用户内容保存；无法据此保证识别用户自行写入代码或打印的任意秘密，导出前仍应自行审阅。运行计划不含 --env；用户脚本本身打印出的环境值属于输出内容。

CLI 的 log 导出按官方格式，统一先校验再原子写入，不覆盖目录或符号链接；内部历史与 log --json 保留增强元信息：

| 格式 | 行为 |
| --- | --- |
| jsonl | 官方空白与 ASCII 转义格式，每行一个脱敏事件，省略内部 account/session/event_id |
| ipynb | 官方标题、事件映射与 kernelspec；执行次数为 null，多行 source/text 使用数组 |
| md | 官方 Execution/Session Created/File Operation 文本、代码和 Output 围栏 |
| txt | 官方时间戳、事件大写标签、代码摘要与事件文本 |

输出 Notebook 使用 nbformat 4.5、独立 UUID cell ID，不把历史自动执行一遍。库接口 history::export 仍提供原增强导出，保留执行状态、消息身份、stderr、MIME 与 buffers；CLI 兼容输出不会修改这些内部审计记录。RunOutcome 的 kernel_cleanup_error 字段保留兼容，目前不单独请求 kernel DELETE，通常为 null。

CLI Notebook 导出采用与原版相同的文本换行和 MIME 行数组。合成历史的三组对照比较除随机 cell ID 外的全部 Notebook JSON，覆盖混合换行、SVG、JavaScript、输入提示、已脱敏回复和自动化输出。详见 [Notebook 对照](../research/notebook-line-compatibility.json)。

## 历史账户路由

`log -s NAME` 先使用全局 `-a` 或 `COLAB_ACCOUNT`。无显式选择时，先查各账户的本地会话记录，active 中命中优先，否则使用唯一命中的账户。多个非 active 账户的会话同名时要求显式选择。没有本地会话记录时按历史查找，便于读取已停止会话。该查找只使用本地数据。

[固定合并版对照](../research/merged-history-routing.json) 的九个场景包含跨账户同名记录、显式选择、历史回退和歧义。八个场景的退出码和输出一致，包括同名歧义时的账户列表及选择提示。Rust 保留按非 active 历史查找已停止会话的行为。可用 [对照脚本](../tools/compare_merged_history.py) 复现。

## 验证与完整目标缺口

新增 [history 测试](../tests/history.rs) 10 项、[run 单元与恢复测试](../tests/runs.rs) 7 项；[Jupyter 测试](../tests/jupyter.rs) 增加 4 项 run 端到端，包含 argv 字符串/参数隔离、keep、握手失败清理、退出码 7 与释放失败、cleanup-only 恢复，以及真实子进程 SIGTERM=143。控制面另增历史失败调和测试。并修正旧控制面 loopback 测试在 macOS 继承 nonblocking socket 导致的偶发 WouldBlock。

[probe_run_wrapper.py](../tools/probe_run_wrapper.py) 直接提取 Rust 包装模板，用本地 Python 与假的 IPython.display 执行八个合成用例；[结果](../research/run-wrapper-probe.json) 记录源码哈希、__main__/argv/file/future、SystemExit 和普通/编译错误验证。Python 只用于研究工具，Rust CLI 与 Cargo 测试不要求本地 Python。

2026-11-18 的 [包装器复验](../research/run-wrapper-runtime-validation.json) 覆盖十组纯 Python 模板用例，并在本机启动真实 IPython kernel，通过 Jupyter 消息确认 dataclass/pickle、`SystemExit(23)` 和第 3 行异常各收到一次退出标记。每组都另发执行请求核对原 kernel 的 `__main__` 与 `sys.argv` 已恢复。CLI loopback 测试另验证带 BOM 和 CRLF 的 shebang 变为空行，并保持退出码及清理记录。Gateway 和 Google CPU 验收仍引用各自构建对应的记录。

可使用以下命令复现本机 kernel 验证。默认及无默认特性各通过 88 项 Rust 专项测试，原版 `run` 八场景对照的分类、差异字段及退出码均保持一致。

```sh
uv run --isolated --with ipykernel==7.4.0 --with ipython==9.18.1 \
  --with jupyter-client==8.10.0 -- python tools/probe_run_wrapper.py \
  --kernel --output /tmp/colab-run-wrapper.json
```

完整默认测试 180 项、no-default-features 178 项通过，fmt、两种特性 clippy、默认 build 和研究文档静态检查通过。上述回归使用合成凭据；另见 [真实 CPU 验收](15-live-cpu-validation.md)。本地 Python 包装测试仍不能证明 run 包装在 Colab/IPython 的实际行为。

整个目标仍包含 P4–P6 的 REPL/交互 stdin、原版 WebSocket 剩余协议、Colab 专用授权、SSH、DriveFS、完整诊断/分发，以及对应真实集成门槛。supervisor/任务恢复已移至外部调度层，见 [08](08-supervision-and-recovery.md)。当前不能宣称“整个设计文档已实现”或无限保活可用；下一步继续按这些缺口推进。

## 后续：真实 run 创建、非零退出与自动释放

2026-11-18（Asia/Shanghai），使用用户明确选择的系统钥匙串与当前默认 debug（哈希匹配完整 CLI 报告），先查询 assignments 并确认无控制面 pending，再以随机唯一会话名调用 run --json。不复用用户 VM；只请求默认 CPU，不挂载 Drive，不安装远端服务。

本地合成脚本含 shebang、future annotations、断言和随机输出标记，实际验证 __name__='__main__'、Unicode 文件名对应的 __file__/sys.argv、空格/引号/反斜线/中文参数、脚本路径之后 --env 仍属于 argv。客户端 --env 的值含等号/Unicode/空格，在远端被精确读取，cwd=/content。所有断言通过后脚本发出随机标记并 SystemExit(23)；进程与 RunOutcome 均返回 23，execution_error/cleanup_error/history_error 等为空，标记在实际执行输出中出现。

run 自身 cleanup=confirmed_released，随后独立查询后端确认本次 endpoint 不再存在，其他新增 assignment 为 0。本次 run 计划已删除，未调用额外 run-cleanup，也未重放脚本。私有账本和原始结果留在临时目录、不提交；若失败，测试清理仅选唯一随机名称/账户匹配且 endpoint 不属基线的 run 计划，使用其 run_id 和已有归属保护恢复。

脱敏结果见 [live-run-validation.json](../research/live-run-validation.json)。报告明确区分“请求 CPU”与实际硬件查询：本次自动释放前未另做硬件查询，不将默认请求当作独立硬件测量。此项报告本身未覆盖 --keep、信号中断、结果未知、清理失败或长期运行；后续 keep/SIGTERM 实测见下节，不能以单次成功关闭完整 V05.9/P3。没有改产品或重编译，不重报全量测试。

## 后续：真实 keep 与 SIGTERM 生命周期

2026-11-18（Asia/Shanghai），继续使用同一默认 debug 和用户选择的系统钥匙串，分别顺序创建两台随机唯一名称的默认 CPU 会话；每次先查询基线、记录本次归属，只清理本次 endpoint，不访问 Drive。没有改产品、重新编译或重报全量测试。

`run --keep --json --timeout 30` 的合成脚本在远端 `/tmp` 写随机测试文件。返回 exit_code=0、cleanup=kept，后端仍存在本次 endpoint；随后独立 `exec` 读取该文件并断言随机内容，返回 0，证明后续命令仍可访问该运行状态。成功 keep 已移除本次运行计划；测试最后用名称与预期 endpoint 保护的 stop 主动释放，独立后端查询确认消失，其他新增 assignment 为 0。见 [keep 结果](../research/live-run-keep-validation.json)。

另一项不带 keep 的 `run --timeout 0` 执行打印随机标记并 sleep 的脚本。测试在本地实际收到远端标记后，从匹配账户、随机名称和文件 ID 的持久运行计划记录归属，且拒绝基线 endpoint；随后仅向 CLI 主进程发送 SIGTERM。进程退出 143，历史 run_completed 的 exit_code=143、cleanup=confirmed_released、cleanup_error 为空；独立后端查询确认 endpoint 已消失，本次运行计划已移除，没有新增其他 assignment，未额外恢复或重放脚本。见 [SIGTERM 结果](../research/live-run-signal-validation.json)。

两份报告只提交脱敏标签、计数与构建哈希，真实账户、endpoint、随机标记和原始结果保留在本机私有临时目录。CPU 仅为请求类型，未单独查询硬件。SIGTERM 证据不代表 SIGINT/HUP/KILL，也不证明客户端收到信号时远端代码立即被中断；本次由后续 VM 释放结束远端工作。仍缺真实断线导致结果未知、释放失败恢复及长任务的证据，完整 V05.9/P3 不据此关闭。
