# CLI 自动对比报告

场景数：2；分类：matched=2。

官方源码与 KERNEL 依赖使用 research/source-inventory.json 固定提交并逐文件验哈希。两版使用独立临时配置、合成凭据及同规格 loopback 服务；未访问真实钥匙串、Google、Drive 或 VM。

`matched` 仅表示当前合成场景的退出码、输出、请求摘要及会话状态相同；`different` 表示存在已记录差异；`failed` 表示模拟契约断言或测试环境未通过；`rust_extension` 表示官方没有对应命令或参数，不能作为兼容通过。

| 场景 | 分类 | 差异 / 未通过检查 |
| --- | --- | --- |
| repl-image | matched | — |
| exec-image | matched | — |

业务场景分类：matched=2。

自动名称先断言格式，再归一化随机值；Notebook cell ID 与 Jupyter session ID 只在比较时归一化，JSON 中保留原始观察值。

完整 stdout、stderr、HTTP 请求摘要、断言、依赖版本与基线信息见同名 JSON 报告。

限制：帮助文本语言与版本号未强行归一化；没有测试真实 OAuth 提供方、OS ACL、Google 配额、真实 kernel 执行、交互 stdin、绘图与长期会话。account rename/remove 的成功和失败恢复路径由 Rust 内存凭据测试覆盖，本脚本只跑无真实钥匙串访问的缺失账户路径。run-cleanup 额外验证已知 endpoint 的所属 run 成功释放与非法 ID 拒绝路径。
