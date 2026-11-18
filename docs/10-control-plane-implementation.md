# 控制面实现状态

更新日期：2026-11-16。本文记录 P2 的控制面子阶段实际实现。线缆依据为 [07](07-control-plane-and-contents.md) 的固定源码契约，阶段初期使用离线替身；后续真实 CPU 验收见 [15](15-live-cpu-validation.md)；后续 Contents 与跨账户汇总已见 [11](11-contents-and-inventory-implementation.md)，线上验收仍未完成。

## 输入、状态变化与清理责任

| 操作 | 输入与账户选择 | 状态变化 | 失败与清理责任 |
| --- | --- | --- | --- |
| `new [-s NAME] [--gpu MODEL / --tpu MODEL] [--high-mem] [--json]` | 显式账户、环境账户、active；不会按会话名找其他账户 | 先写分配意图，成功后写 sessions | 结果未知保留同一 nbh；用户用同一账户与参数重试 |
| `status [-s NAME]` / `sessions`，均可加 `--json` | 指定名称时支持既有会话与操作日志归属路由 | 成功查询并校验后刷新 token/url/expiry/硬件、裁剪失效记录 | 网络、HTTP 或 schema 失败不裁剪；不自动分配 VM |
| `stop [-s NAME]` | 同 status 的账户解析优先级 | 先记释放意图，查询后端并执行 GET/POST unassign，成功后删本地记录与日志 | 失败保留清理意图；再次 stop 先查询，资源已消失时直接完成清理 |
| `usage [--json]` | 显式账户、环境账户、active | 读取 CCU 余额、小时消耗及 assignment 数 | 错误透传，不创建资源 |

GPU 为 T4/L4/G4/A100/H100；TPU 为 V5E1/V6E1；型号大小写不敏感。指定 --gpu/--tpu 时必须提供型号，同时存在时优先 TPU，与官方一致；均省略则为 CPU。high-mem 仅在支持形状选择时发送 `shape=hm`；未发送时本地 machine_shape 暂记 STANDARD，不代表服务端物理内存。

new JSON 返回一个会话对象；status JSON 始终返回数组（包括指定名称时）。字段为 account、name、endpoint、accelerator、variant、machine_shape、shape_source、token_expires_at；P3 另增加 kernel_id、session_id、running 和 kernel 操作 pending 标志，详见 [12](12-jupyter-and-execution-implementation.md)。不输出 token 或 runtime URL。`sessions` 列出全部后端 assignment，孤立 assignment 内部 name 为 null，终端及合并 JSON 显示 `?`；`status` 仅列出本地命名会话，并显示 IDLE/BUSY 与上次执行信息。两者不是别名。空列表分别输出 `[colab] No active sessions found on server.` 和 `[colab] No active sessions.`；终端硬件使用 CPU、Standard/High-RAM 标签。指定名称不存在时，文本 status 提示未找到并成功退出，JSON 模式仍返回错误。会话 JSON 另包含 last_execution。未知整数 variant/shape 显示 `Unknown(原值)`，未知 accelerator 字符串保留原值。创建的形状来源为 request；状态同步后为 backend。库层用量模型使用 currentBalance、consumptionRateHourly、assignmentsCount；CLI sessions/usage JSON 已迁移为合并版账户行，见 [44](44-merged-inventory-implementation.md)。

## HTTP 与认证边界

[colab_api.rs](../src/colab_api.rs) 负责协议，[sessions.rs](../src/sessions.rs) 负责账户与持久化；CLI 使用已有 OAuth/authorized_user ADC 服务取得所选账户 token，不改变进程环境或 active。每次命令开始获取一次 Bearer token；命令内部不对 401/403 自动重登，失败时保留资源恢复线索，重试命令会重新检查认证有效期。

所有请求带 Accept、X-Colab-Client-Agent 和 authuser=0；assign/unassign 的 POST 使用准备响应的 X-Goog-Colab-Token，无业务 JSON body，并显式发送 `Content-Length: 0`，避免严格 HTTP 前端返回 411。assign 准备响应的 acc/nbh/variant 是服务端元数据，不要求与请求值相等；POST 沿用原请求 query，仅从准备响应取 token，保持固定上游的行为。先检查 HTTP 状态，再去除精确 XSSI 前缀并解析模型；释放的成功响应允许空 body。禁止跟随重定向；错误不输出响应正文、URL 或凭据。响应体上限 4 MiB，连接超时 5 秒，单次请求总超时默认 20 秒（包括读取 body）。

分配默认初次请求后允许 3 次重试，退避 10/20/40 秒。只对读取超时和 502/503/504 重试，每次先用同一 nbh GET 调和，再决定是否 POST。412、其他 4xx、连接失败和无效响应不自动重试。分配准备 GET 的可重试错误也应用相同次数限制。文本模式对 412 提示容量或使用限制等可能原因，对 GPU/TPU 请求的 400 提示配额或权限及切换 CPU 的选项。两种提示均不输出服务端正文。

| 配置 | 默认值与限制 |
| --- | --- |
| `COLAB_CLI_REQUEST_TIMEOUT` | 20 秒；有限正数，允许小数；对应隐藏全局参数 `--request-timeout` |
| `COLAB_CLI_ASSIGN_RETRIES` | 3；整数 0–10；对应 `--assign-retries` |
| `COLAB_CLI_RETRY_BACKOFF` | 10 秒；有限数值 0–60；对应 `--retry-backoff` |
| `COLAB_CLI_CONTROL_PLANE_URL` | `https://colab.research.google.com`；对应 `--control-plane-url`；仅 HTTPS 或 loopback HTTP，拒绝 URL 凭据、fragment、query |

隐藏全局参数用于离线协议测试，放在子命令前。次数与退避上限是本实现增加的约束；负值报错，不沿用上游的截断到零行为。替换控制面地址会将所选账户 Bearer 发往该地址，仅应指定受信任的服务。

## 操作日志与并发

逐账户 `accounts/<profile>/control_operation.json` 由已有原子写入与受限权限存储保存。分配日志以随机 UUID v4 派生的 44 字符 nbh 作为稳定操作标识，记录名称、硬件参数和可选已取得的完整会话（含 proxy token，因此不得分享日志原文）。释放日志保存名称和 endpoint，不保存 OAuth 或 XSRF token。

分配在任何资源请求前先写日志。同一账户有待恢复的分配时，`new` 只接受匹配的恢复请求。缺省名称由随机标识派生，缺省名称重试会复用待恢复日志。未暂存 endpoint、无 run 所有者的普通分配允许以相同硬件参数换用本地名称恢复：先检查重名，再原子更新日志名称，保持原 nbh，不新建分配标识；失败后继续重试也沿用该标识。已暂存会话、run 所有者或不同硬件参数仍不能借改名绕过恢复保护。获得 endpoint 后先暂存完整会话，再提交 sessions；会话提交失败时再次 new 直接提交暂存结果，不重新分配，也不延长已记录的 token 到期时间。若暂存本身失败，原始 nbh 仍可用于后端调和。

首次 POST 前，原子记录 `post_attempted=true`。没有更早结果未知的 POST 时，控制面明确返回 HTTP 400 或 412 会结束本次分配并清理日志，允许用户换配置再次 `new`。如果更早的 POST 结果未知，后续 GET 或 POST 被拒绝时仍保留原 nbh。缺少 `post_attempted` 的旧日志按已尝试处理。`run` 共用此规则，确定拒绝且未分配时清理本次 run 计划。

释放前持久化意图。在资源请求前原子记录 `attempted=true`。`run` 转换成释放意图时先记录 `attempted=false`，并在真正请求前置 `true`。首次释放走 GET/POST unassign。已有 kernel ID 时，先查询 assignments 确认同一 endpoint 并取得最新代理凭据，再最多用五秒尝试 `DELETE /api/kernels/{id}`。kernel 消失、删除失败、首次查询失败或目标暂时未出现在列表时，仍向原 endpoint 发起 unassign。固定上游的 `stop` 调用了 `shutdown_kernel=True`，其新建的 runtime 未初始化 kernel 客户端，因此该调用不会发出 DELETE。这里按功能设计显式关闭已记录的 kernel。

unassign POST 失败保留会话和日志。重试已尝试的意图必须先查询后端，只有 endpoint 仍在时才再次尝试关闭已记录的 kernel 和释放。旧日志缺少 `attempted` 时按 `true` 处理。即使后续 `status` 已裁剪失效会话，操作日志仍能支持账户路由和 `stop` 恢复。POST 成功后本地提交失败，同样通过再次查询后端完成清理。成功释放会移除同一账户下指向该 endpoint 的全部本地记录。

有待恢复分配时，`stop -s NAME` 仍可释放同一账户下另一台已有会话。释放意图写到独立的 `control_parallel_release.json`，原分配日志和 nbh 继续保留。待分配名称及其已暂存 endpoint 受保护，其他名称指向同一 endpoint 时也拒绝释放。独立释放如遇到未知结果，先用原名称重试 `stop`，完成后再用原参数恢复 `new`。`ext doctor` 只报告日志是否存在。资源操作继续由同一账户生命周期锁串行执行。

整个会话操作持有逐账户生命周期锁，sessions 提交再持有既有读改写锁；并发同名创建不会重复分配，其他账户拥有独立锁。旧上下文不能重新创建已删除账户。待恢复操作阻止账户 rename/remove（包括 force），避免日志失去归属。待恢复日志由同一账户使用原参数重试 `new` 或 `stop`。省略名称时，`stop` 用一次 assignments 查询同步并选择唯一会话，再复用该查询的代理凭据尝试关闭 kernel。选择、endpoint 校验和远端释放持有同一把账户锁。若同名释放日志指向旧 endpoint，需显式指定名称恢复。无会话或多会话则提示并退出 1，指定未知名称则提示并返回 0。本地无会话时，省略名称的 `stop` 直接报告空列表。

## 验证与尚未实现

[control_plane.rs](../tests/control_plane.rs) 的 loopback 和临时目录测试覆盖请求格式、分配调和、释放失败恢复、账户隔离和 CLI 流程。kernel 关闭新增代理刷新和 DELETE 204、404、503 三种响应的覆盖。[Jupyter 测试](../tests/jupyter.rs) 同时验证执行中 `stop` 和自有 `run` 会话清理。

默认完整测试 180 项、no-default-features 178 项通过；fmt、两种特性的 clippy 与默认 build 通过。研究文档的静态检查结果见 [documentation-checks.json](../research/documentation-checks.json)。

孤立 assignment 的接管和直接按 endpoint 释放尚未实现。创建后端资源后，调用方仍需独立核对 Jupyter 是否就绪。单次线上分配、释放与 CPU 文件往返见 [15](15-live-cpu-validation.md)。本次新增的关闭顺序在 loopback 验证，真实服务仍待复验。supervisor 的职责划分见 [08](08-supervision-and-recovery.md)。
