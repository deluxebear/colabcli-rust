# 单次真实 CPU 验收

日期：2026-11-16；环境：macOS、Rust 1.99.1、本机 `target/debug/colab`。用户明确授权创建一个真实 CPU 会话并测试相关命令。逐项结果见 [脱敏记录](../research/live-cpu-validation.json)。官方双端对比仍采用隔离的 loopback 服务，见 [14](14-cli-comparison.md)；本章为 Rust CLI 的真实 Google 服务测试。

## 输入、状态与清理

使用已有授权账户，开始前确认 assignments 为空且无待恢复控制面操作。仅分配一个 `NONE / DEFAULT / STANDARD` CPU VM，保存本次名称与 endpoint 供所有权核对。测试文件使用独立名称，执行内容仅包含合成变量、预期异常和小型绘图；没有挂载或访问用户 Drive。

先执行 `new -s <本次独立名称> --json` 成功创建，再执行下表 33 项检查。所有调用顺序执行；二进制往返与编辑结果做字节比较，执行结果检查标记与状态，内核重启后检查变量已清除，PNG 检查文件签名，导出检查产物存在且非空。预期错误的退出码作为通过条件。

| 范围 | 真实检查 |
| --- | --- |
| 控制面 | `sessions --json`、`status --json`、`usage --json`，确认 CPU 会话与用量字段 |
| 文件 | `ls` 目录/单文件、二进制 `upload`/`download`、`edit` 编辑器子进程、编辑后下载校验、`rm`、删除后 404 |
| 执行 | stdin、`-f` 脚本、`--env`、跨命令变量保留、文本异常退出 0 / JSON 异常退出 1、两单元 Notebook 与输出文件 |
| 图片 | 远端 Matplotlib/IPython 生成图形，`--output-image` 保存 PNG |
| 内核 | `kernels --json`、`restart-kernel`、重启后变量消失、再次查询 |
| 历史 | `log` 会话列表、事件、按类型/数量过滤，JSONL/TXT/Markdown/Notebook 四种导出 |
| 本地运行状态 | `runs` 只读列表、末次 `status` 确认无运行中执行 |

33 项最终全部通过。随后核对待释放会话的名称与 endpoint 均属于本次创建资源，执行 `stop -s <本次独立名称>`，退出 0；再次执行 `sessions --json` 成功，本次会话已不存在。没有清理既有历史或其他资源。真实 endpoint、账户配置、token、服务 URL 与完整原始输出不纳入仓库；提交的记录只保留测试标签、退出码、耗时与清理结论。

## 线上发现及修复

初次二进制下载失败：`Contents 协议或参数无效: 无效的 base64 content`。直接核对本次合成文件的 Contents 响应，确认服务端返回带 ASCII 换行的 base64，忽略换行后解码字节与上传内容一致。Rust 原先严格解码整个字符串，拒绝合法 Jupyter 格式。

修复下载后，真实图片保存再次暴露同类问题：`Jupyter 协议或参数无效: 图片 base64 无效`。现在文件下载、图片目录保存与最后一张图片保存均先去除 ASCII 空白，再执行严格 base64 解码；非法字符仍拒绝，原有大小限制与原子落盘保持。

新增 Contents 回归覆盖 256 个二进制字节、76 列折行、LF/CRLF 和末尾换行；现有 Jupyter 图像回归加入换行，并覆盖两种图片保存入口。这两个回归均先确认修复前失败，再确认修复后通过。双端差分替身也返回带换行的文件和图片 base64，避免仅用无换行样本掩盖线上差异。

## 验收边界

这是单账户、单 CPU、短时执行验收。没有重做 OAuth 登录、检查所有钥匙串 ACL、比较官方真实 VM 行为，也没有测试 GPU/TPU、Drive、交互 stdin、由 `run` 创建的 VM、长任务、supervisor 或无人值守恢复。账户隔离、并发、中断恢复与 `run` 生命周期继续由离线测试覆盖，不能以本次成功声称所有线上场景已兼容。

## 2026-11-18 控制面复验

使用 macOS ARM64 的当前 debug 构建和已有系统钥匙串，在后端会话为空时分配一台随机命名的 `NONE / DEFAULT / STANDARD` CPU。`sessions --json` 和具名 `status --json` 都返回本次 endpoint。绑定该 endpoint 的 `exec --json` 返回成功及预期标记，随后 `status` 与本地会话记录给出同一非空 kernel ID。

使用本次名称和预期 endpoint 执行 `stop`，退出码为 0。单独的 `sessions --json` 查询确认本次 endpoint 已消失，其他新增会话数为零。本地会话、控制面日志及独立释放日志均已清理。二进制哈希及六项检查见 [脱敏结果](../research/live-cpu-control-current.json)。

本次真实验收覆盖具名停止的成功路径。kernel DELETE 的具体响应、无 `-s` 时的请求次数，以及分配或释放的故障注入仍由本地测试覆盖。测试未访问 Drive。

## 2026-11-18 文件命令复验

在 macOS ARM64 当前 debug 构建上，用已有系统钥匙串创建一台 CPU。所有文件命令绑定本次 endpoint，并确认错误的 endpoint 条件会被拒绝。在运行时 `content` 下上传一份 65,794 字节的二进制文件，文件名含中文、空格及 `#?%`。目录和单文件 `ls --json` 返回预期路径与类型，下载内容与原始字节完全一致。

`edit` 对缺失的文本文件启动编辑器并创建文件，再次编辑无更改时明确提示。下载后核对 UTF-8 内容。`rm` 删除试验文件后，两次下载返回退出 1 和未找到提示，本地未生成目标文件，末次目录列表中无测试条目。首次探针误把根级文件子命令当成 `files` 命令组，修正后通过。两次测试的 VM 都经独立后端查询确认释放，未访问 MyDrive。详见 [本轮脱敏记录](../research/live-cpu-files-current.json)。

同一二进制与固定官方版进行 13 个 loopback 场景对照，12 个一致，另有 `ls --json` 扩展通过本地契约断言。以同一源码另构建无默认特性版本，固定合并版的 18 个文件场景全部一致。双端模拟和真实 Rust 验收的构建与范围分别记录。另有 17 项 Contents 和 7 项 endpoint/释放保护测试通过。详见 [本轮文件对照](../research/current-file-compatibility.json)。
