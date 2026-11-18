# 41. 账户离线清单与合并版业务契约

## 输入与输出

`account/acct list/ls --json` 现在输出 `{active, default_drive_account, accounts: [...]}`，不再直接序列化内部注册表。每行包含 account、active、email、user_id、auth、drive 和 sessions；会话名排序、账户行按注册表文件顺序展示。文本输出包含同样的摘要。后续 [42](42-account-mutations-implementation.md) 也让内部写回保序；过去已经排序的文件不会自动恢复创建时间顺序。

`account current --json` 补齐账户元数据、auth_provider、Drive 账户和来源、sessions/history/Drive/OAuth 路径及 auth 摘要。账户来源使用 flag/env/active；显式参数优先于环境。Drive 来源复用解析器，使用 flag/env/self/bound:计算账户/default，避免摘要另写一套选择规则。普通 `current` 仍只输出账户名。

## 状态、失败与清理

查询不刷新令牌、不打开浏览器、不访问系统凭据库或 Google。OAuth 文件存在非空 token/refresh_token 时，只声明本地文件就绪，不证明令牌有效。损坏或缺失文件在 file 模式下为 ready=false；auto/keyring 模式下 ready=null、storage=`keyring (not inspected)`。后者是保留差异：扩展会尝试限时读钥匙串，可能触发系统解锁；本实现避免离线查询弹窗。

ADC 摘要按扩展规则仅检查配置文件、环境文件或默认 gcloud 文件存在性，不解析、执行插件、访问 metadata 或进行交换。ready=true 不能作为授权成功证据。摘要中的路径可能含用户配置的文件名；不会输出文件内容。

账户顺序和模型验证来自同一次注册表读取，仍拒绝待恢复账户事务。会话文件、账户和 Drive 路径使用既有存储校验；读取失败按已有错误退出，不丢弃注册表保护。入口仍使用既有 bootstrap，不承诺不存在配置时零写入。查询没有新增远端资源和清理责任，不修改已有凭据/注册表负载。

## 证据

固定合并版实际命令对照见 [merged-account-comparison.json](../research/merged-account-comparison.json)。[探针](../tools/compare_merged_accounts.py) 验证三仓库固定提交和源码哈希，在临时 HOME/config、file 存储中执行 16 项真实命令，覆盖 JSON/文本、隐藏别名、账户 flag/env 优先级、Drive 环境别名和 ADC 文件存在状态。合成会话覆盖排序和账户隔离。Python 子进程禁止网络/DNS、钥匙串、浏览器和外部程序，禁用后台更新；逐项检查已有文件字节未变化、敏感合成值未输出。除 HOME 路径规范化和 JSON 对象键序外，不抹平输出差异。

```sh
python3 tools/compare_merged_accounts.py \
  --cli /path/to/google-colab-cli --ext /path/to/colab-ext \
  --kernel /path/to/jupyter-kernel-client --python /path/to/official-python
```

[五项 Rust 集成测试](../tests/account_inventory.rs) 补充 native/auto 未检查状态、损坏文件、只按存在检查 ADC、会话隔离、路径、顺序和文件不变验证。合并版业务验收目前仅覆盖此处列出的命令与状态，不等价于整个合并版 CLI 已兼容；真实钥匙串和 Google 平台不在本轮结论内。

最终验证：默认特性 383 项、无默认特性 381 项 Rust 测试通过；两种特性 Clippy、fmt 通过。16 项合并账户对照一致且无禁止操作/已有负载变化。完整官方 CLI 差分仍为 365 项：92 一致、54 保留差异、219 Rust 扩展，原有命令输入和分类不变；83 个合并帮助路径无缺失。三份 debug 报告哈希与当前二进制一致，本机 release 的 14 项打包/运行检查通过，包含本轮新增的两个账户检查。未发布或推送代码。
