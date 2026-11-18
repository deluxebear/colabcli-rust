# 合并版 Drive 凭据输出对齐

## 输入与最终行为

以固定合并版 drive/dfs show、clear 和缺凭据 token 的实际执行结果为依据，对齐本地凭据命令。show/token 按既有计算账户和 Drive 路由解析，clear 保留授权操作的自身账户/显式环境路由；不查询控制面、不分配 VM。

show 有凭据时将已知脱敏元数据放在 JSON 顶层，与 account、drive_account、store 并列；此前 Rust 独有的 credentials 包装层移除。旧脚本应将 result.credentials.user_id 等读取改为 result.user_id。没有凭据时恢复原版单行 ``[ACCOUNT] PATH: empty (run `consent`)``，不再返回 credentials=null JSON。clear 的删除和重复清除提示也采用固定合并版文本，并提示用户自行撤销 Google grant。

继续使用已知字段白名单，不回显 access/refresh token、client secret 或任意未知字段。已有安全摘要保留 access_token 的 `<set>`、expiry、scope_source 信息；这些扩展字段无值时省略，避免给旧格式增加无意义 null。并不复现上游遍历未知字段可能泄露额外凭据的行为。缺凭据 token 两端都退出 1；原版将字符串 SystemExit 转整数时触发 ValueError，stdout/stderr 均为空，Rust 保留明确的中文缺凭据诊断，不复现该包装异常。真实刷新和 whoami 不属于本次双端离线样本。

## 状态、失败与清理

没有更改凭据解析、验证、锁序、刷新、暂存恢复或账户隔离。show 仍可能完成合法的本地刷新暂存提交；不是严格零写入命令。clear 仍同时清理本账户的主凭据、授权 pending 和刷新暂存，防止凭据复活；不删除计算 OAuth、不停止 VM、不撤销远端授权。不兼容的或损坏的凭据仍明确报错，不为复刻原版宽松展示而跳过验证。

## 对照方式

[合并版对照工具](../tools/compare_merged_drive_credentials.py) 使用固定三个源码仓库、全新临时 HOME、合成凭据和 file 存储。Rust 必须是无原生凭据库构建，工具在运行前复用 capability 安全门，未知或原生后端拒绝。Python 使用 -S 禁用 .pth，并限制网络、钥匙串、浏览器和外部进程；记录实际导入路径对应的固定源码哈希。

12 组覆盖自身/绑定/显式 Drive/两种环境变量/全局回退路由、隐藏 dfs 别名、空凭据、清除自身/环境目标、重复空清除及缺凭据 token。每组逐文件核对 payload 哈希：show/token 不改变已有文件，clear 只能删除明确选中账户的 dfs_credentials.json；其他账户与计算 OAuth 保持原字节。报告只含合成账户与合成元数据，不接触真实授权。

结果见 [merged-drive-credentials.json](../research/merged-drive-credentials.json)。现有 [Drive 凭据 CLI 测试](../tests/drive_credentials.rs) 同步覆盖顶层元数据、敏感字段不输出、空展示和重复清除；其余刷新、租约和失败恢复测试继续保持。本次没有执行真实 consent、Drive token 请求、挂载或长期刷新，不关闭 V07 或完整 U01。

最终 12 组对照为 11 组完全一致、1 组明确的缺凭据诊断差异，无未解释差异。默认/无默认特性全量 Rust 测试分别为 461/459 项通过，两种特性的 Clippy、fmt 通过；Python 工具 74 项中 73 项通过、1 项 Linux 专属跳过。既有完整 CLI 365 场景的名称、命令和分类不变（92 一致、54 保留差异、219 Rust 扩展）。

本机 release 的 [17 项打包运行检查](../research/native-release-validation.json) 通过，新增实际打包二进制的顶层脱敏、空展示、重复清除及注册表不变检查；两组既有非 TTY 图片对照通过。release 为 9,140,704 字节，默认测试 debug 为 44,668,280 字节，四份报告分别匹配对应默认/无钥匙串/release 二进制哈希。其他合并专项与 Linux/Secret Service 报告保留对应阶段快照，未重新执行。当前默认二进制的真实 SSH 只读预检查仍缺认证，创建 VM 为 0；未推送或发布。

## 后续：实际刷新与身份查询对照

[刷新对照工具](../tools/compare_merged_drive_tokens.py) 与 [结果](../research/merged-drive-tokens.json) 增加七组独立 loopback 场景：绑定账户的普通 token、显式另一 Drive 账户的 JSON、whoami、短有效期、Asia/Shanghai 时区、invalid_grant，以及刷新成功后 userinfo 被拒绝。执行实际合并版 CLI、google-auth 与 Rust HTTP 客户端；仅把原版固定 userinfo URL 转到同一 loopback，保留其请求头及解析实现。原版禁止访问其他网络、钥匙串、浏览器或子进程，Rust 使用显式 file 后端及合成 token/userinfo 地址。每个账户有不同的 client/refresh token，服务端验证实际来源，不仅比较所选配置路径。

七组契约检查全部通过，无未解释差异，不将其登记成七组完全等价。记录包含固定源码哈希、实际 Python/依赖版本、脱敏请求轨迹及凭据状态投影。逐文件检查其他账户与计算 OAuth 保持原字节；invalid_grant 时全部 payload 不变。有效期允许请求执行造成的少量时间消耗，普通响应限定在 3595–3600 秒，不通过任意舍弃 expires_in 来隐藏差异。

| 行为 | 实际原版 | Rust 与验收边界 |
| --- | --- | --- |
| 刷新请求 scope | 发送已存储 scope | 不发送 scope；不据此认定原版扩大授权，真实授予仍须 Google 验证 |
| 轮换 refresh token | current_access_token 仅返回 access token/期限，新 refresh token 没有写回原 data | 成功保存轮换；userinfo 失败先保留刷新暂存 |
| userinfo 失败恢复 | 退出 1，轮换结果未保存 | 退出 1，随后 show 恢复暂存；刷新和 userinfo 都不重放 |
| whoami 注册表 | 将身份写入所选 Drive profile 的 dfs_email，并在缺失时填 email/user_id | 只更新独立 Drive 凭据，不用 Drive 身份补写计算身份元数据，保留既有设计差异 |
| 5 秒 token | expires_in 被抬至 60 | 实际记录 4 秒，不人为延长 |
| 3600 秒 token、亚洲时区 | naive UTC expiry 经本地 timestamp 换算后，被最小值规则变成 60 秒 | 使用带时区时间，记录 3599 秒；UTC 对照两端均为 3599 秒 |
| 刷新/身份查询失败 | 原版异常退出，部分路径没有错误文本 | 返回脱敏错误，响应正文不输出 |

依赖版本为 Python 3.14.0、google-auth 2.62.2、requests 2.34.2、typer 0.29.1；时区问题只按此固定组合陈述。短有效期或时区误差会影响消费输出的自动化，但本次没有把已有正确 UTC 计算改成原版错误。未修改产品 Rust、未重编译或重跑全量 Rust/发行测试，沿用上一节的构建证据；本次没有真实凭据、Google 请求或 VM。
