# 控制面实现状态

更新日期：2026-11-16。本文记录 P2 的控制面子阶段实际实现。线缆依据为 [07](07-control-plane-and-contents.md) 的固定源码契约，尚未执行真实 Google 请求或创建 VM；后续 Contents 与跨账户汇总已见 [11](11-contents-and-inventory-implementation.md)，线上验收仍未完成。

## 输入、状态变化与清理责任

| 操作 | 输入与账户选择 | 状态变化 | 失败与清理责任 |
| --- | --- | --- | --- |
| `new [-s NAME] [--gpu [MODEL] / --tpu [MODEL]] [--high-mem] [--json]` | 显式账户、环境账户、active；不会按会话名找其他账户 | 先写分配意图，成功后写 sessions | 结果未知保留同一 nbh；用户用同一账户与参数重试 |
| `status [-s NAME]` / `sessions`，均可加 `--json` | 指定名称时支持既有会话与操作日志归属路由 | 成功查询并校验后刷新 token/url/expiry/硬件、裁剪失效记录 | 网络、HTTP 或 schema 失败不裁剪；不自动分配 VM |
| `stop -s NAME` | 同 status 的账户解析优先级 | 先记释放意图，查询后端并执行 GET/POST unassign，成功后删本地记录与日志 | 失败保留清理意图；再次 stop 先查询，资源已消失时直接完成清理 |
| `usage [--json]` | 显式账户、环境账户、active | 读取 CCU 余额、小时消耗及 assignment 数 | 错误透传，不创建资源 |

GPU 为 T4/L4/G4/A100/H100；TPU 为 V5E1/V6E1；型号大小写不敏感。省略 GPU/TPU 型号时分别默认 T4/V5E1，同时指定时报参数冲突。high-mem 仅在支持形状选择时发送 `shape=hm`；未发送时本地 machine_shape 暂记 STANDARD，不代表服务端物理内存。

new JSON 返回一个会话对象；status JSON 始终返回数组（包括指定名称时）。字段为 account、name、endpoint、accelerator、variant、machine_shape、shape_source、token_expires_at；P3 另增加 kernel_id、session_id、running 和 kernel 操作 pending 标志，详见 [12](12-jupyter-and-execution-implementation.md)。不输出 token 或 runtime URL。`sessions` 列出全部后端 assignment，孤立 assignment 的 name 为 null，终端显示 `?`；`status` 仅列出本地命名会话，并显示 IDLE/BUSY 与上次执行信息。两者不是别名。空列表分别输出 `[colab] No active sessions found on server.` 和 `[colab] No active sessions.`；终端硬件使用 CPU、Standard/High-RAM 标签。指定名称不存在时，文本 status 提示未找到并成功退出，JSON 模式仍返回错误。会话 JSON 另包含 last_execution。未知整数 variant/shape 显示 `Unknown(原值)`，未知 accelerator 字符串保留原值。创建的形状来源为 request；状态同步后为 backend。usage JSON 使用 currentBalance、consumptionRateHourly、assignmentsCount。

## HTTP 与认证边界

[colab_api.rs](../src/colab_api.rs) 负责协议，[sessions.rs](../src/sessions.rs) 负责账户与持久化；CLI 使用已有 OAuth/authorized_user ADC 服务取得所选账户 token，不改变进程环境或 active。每次命令开始获取一次 Bearer token；命令内部不对 401/403 自动重登，失败时保留资源恢复线索，重试命令会重新检查认证有效期。

所有请求带 Accept、X-Colab-Client-Agent 和 authuser=0；assign/unassign 的 POST 使用准备响应的 X-Goog-Colab-Token，无业务 JSON body，并显式发送 `Content-Length: 0`，避免严格 HTTP 前端返回 411。assign 准备响应的 acc/nbh/variant 是服务端元数据，不要求与请求值相等；POST 沿用原请求 query，仅从准备响应取 token，保持固定上游的行为。先检查 HTTP 状态，再去除精确 XSSI 前缀并解析模型；释放的成功响应允许空 body。禁止跟随重定向；错误不输出响应正文、URL 或凭据。响应体上限 4 MiB，连接超时 5 秒，单次请求总超时默认 20 秒（包括读取 body）。

分配默认初次请求后允许 3 次重试，退避 10/20/40 秒。只对读取超时和 502/503/504 重试，每次先用同一 nbh GET 调和，再决定是否 POST。412、其他 4xx、连接失败和无效响应不自动重试。相较基线，此实现也对 GET 分配准备阶段的可重试错误应用相同次数限制。412 只报告 HTTP 状态，不能据此判断唯一原因或承诺固定并发数。

| 配置 | 默认值与限制 |
| --- | --- |
| `COLAB_CLI_REQUEST_TIMEOUT` | 20 秒；有限正数，允许小数；对应隐藏全局参数 `--request-timeout` |
| `COLAB_CLI_ASSIGN_RETRIES` | 3；整数 0–10；对应 `--assign-retries` |
| `COLAB_CLI_RETRY_BACKOFF` | 10 秒；有限数值 0–60；对应 `--retry-backoff` |
| `COLAB_CLI_CONTROL_PLANE_URL` | `https://colab.research.google.com`；对应 `--control-plane-url`；仅 HTTPS 或 loopback HTTP，拒绝 URL 凭据、fragment、query |

隐藏全局参数用于离线协议测试，放在子命令前。次数与退避上限是本实现增加的约束；负值报错，不沿用上游的截断到零行为。替换控制面地址会将所选账户 Bearer 发往该地址，仅应指定受信任的服务。

## 操作日志与并发

逐账户 `accounts/<profile>/control_operation.json` 由已有原子写入与受限权限存储保存。分配日志以随机 UUID v4 派生的 44 字符 nbh 作为稳定操作标识，记录名称、硬件参数和可选已取得的完整会话（含 proxy token，因此不得分享日志原文）。释放日志保存名称和 endpoint，不保存 OAuth 或 XSRF token。

分配在任何资源请求前先写日志；同一账户有未完成操作时只接受匹配的恢复请求。缺省名称由随机标识派生，缺省名称重试会复用待恢复日志。未暂存 endpoint、无 run 所有者的普通分配允许以相同硬件参数换用本地名称恢复：先检查重名，再原子更新日志名称，保持原 nbh，不新建分配标识；失败后继续重试也沿用该标识。已暂存会话、run 所有者或不同硬件参数仍不能借改名绕过恢复保护。获得 endpoint 后先暂存完整会话，再提交 sessions；会话提交失败时再次 new 直接提交暂存结果，不重新分配，也不延长已记录的 token 到期时间。若暂存本身失败，原始 nbh 仍可用于后端调和。

释放前持久化意图；POST 失败不删除会话。即使后续 status 已裁剪失效会话，操作日志仍能支持账户路由和 stop 恢复。POST 成功后本地提交失败，同样通过再次查询后端完成清理。成功释放会移除同一账户下指向该 endpoint 的全部本地记录，不影响其他账户。

整个会话操作持有逐账户生命周期锁，sessions 提交再持有既有读改写锁；并发同名创建不会重复分配，其他账户拥有独立锁。旧上下文不能重新创建已删除账户。待恢复操作阻止账户 rename/remove（包括 force），避免日志失去归属。不要手删日志；使用同一账户和参数重试 new/stop。确定失败的分配也暂留日志，没有实现丢弃操作或通用 doctor 恢复命令。

## 验证与尚未实现

[control_plane.rs](../tests/control_plane.rs) 有 24 项 loopback/临时目录测试，覆盖请求 header/query/body、XSSI、GET union、POST 网关错误与超时、body 读取超时、412 不自动重试、跨调用 nbh 调和、落盘失败恢复、失败查询不裁剪、未知硬件与孤立资源、释放 endpoint 编码、释放失败恢复、账户隔离、同名并发、待清理路由及真实 CLI new/status/usage/stop 流程。未使用开发者凭据或真实系统 keyring。

默认完整测试 152 项、no-default-features 150 项通过；fmt、两种特性的 clippy 与默认 build 通过。研究文档的静态检查结果见 [documentation-checks.json](../research/documentation-checks.json)。

尚未实现孤立 assignment 的接管/按 endpoint 释放、Jupyter kernel teardown 或 supervisor；stop 当前直接释放整个 VM。创建后端资源并不是 Jupyter 就绪检查。未执行 Google 线上分配/释放、CPU 文件往返或原生凭据互通验证，因此不能将完整 P2 或线上兼容标记完成。
