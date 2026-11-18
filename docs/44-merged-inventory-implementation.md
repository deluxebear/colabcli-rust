# 44. 合并版会话与用量汇总

## 查询和输出

共享账户探针现在服务 account/acct status、sessions 和 usage 的合并版 JSON/全账户文本输出。各 worker 显式解析自己的账户并使用其凭据，保持注册表顺序，最多并发四个账户（固定扩展为八个）；不通过修改进程环境切换身份。

| 命令 | 请求 | JSON 顶层 |
| --- | --- | --- |
| account status --json | 每账户先 CCU、后 assignments | 账户行数组 |
| sessions --json | 所选账户 assignments | 单个账户行 |
| sessions --all --json | 每账户 assignments | 账户行数组 |
| usage --json | 所选账户 CCU | 单个账户行 |
| usage --all --json | 每账户 CCU | 账户行数组 |

账户行字段为 account、active、email、auth_provider、drive_account、drive_ready、usage、assignments、error。usage 包含 balance、rate_hourly、assignments_count；不查用量时为 null。assignment 包含 name、endpoint、hardware、accelerator、shape、machine_shape、variant；后端孤立会话名称为 `?`。不会输出 proxy token、runtime URL 或任意内部字段。

sessions --all 文本按 `[账户/名称] endpoint | Hardware: ... | Shape: ... | Variant: ...` 展示；usage --all 文本保留 active 标记、邮箱、余额、速率和数量。会话错误输出到 stderr，用量错误输出到 stdout，与固定扩展一致。单账户无 JSON 的原版文本流程保持；Rust 扩展 status --json/--all 继续提供本地 kernel、执行和恢复详情。

## 状态与退出码

全局账户先校验，但合法 -a 不限制 --all 范围或改变 active；account status 同样先校验根级账户。sessions/usage --all 可与有效账户选择共存。每个 worker 仍显式选择自己的资料，不复用全局选中的 Bearer。

只查 CCU 不修改本地 sessions。完整 assignments 查询成功后同步已有会话 proxy 并裁剪服务端确认消失的本地项；请求失败不裁剪。联合查询中 CCU 成功、assignments 失败时保留已经取得的余额；CCU 失败不继续请求 assignments。凭据文件和账户注册表不因正常汇总写入；认证需要刷新时仍使用既有受控轮换流程。

与固定扩展一致，账户认证、HTTP、协议及会话同步错误放入当前行，其他账户继续。**即使单账户 --json 出错也返回 0，调用方必须检查 error**；不再把这些错误写到 stderr 并返回 1。参数解析、无效全局资料、注册表/事务和命令级配置错误仍非零。取消保留 128+signal 和插件回收责任，不输出成功 JSON。HTTP/缺凭据错误文案继续使用 Rust 脱敏类型，不复制响应原文、URL 或 SDK 异常信息。

不会创建或释放 VM，也不执行挂载。裁剪不清除受管恢复责任，原有锁、事务及凭据隔离规则保持；本轮没有远端资源清理责任。

## JSON 迁移

此前 sessions --json 是内部 SessionView 数组，--all 使用 `{account, result/error}`；usage --json 使用原始 currentBalance 字段。现在迁移为上述合并版账户行：

```sh
colab usage --json | jq '.usage.balance'
colab sessions --all --json | jq '.[] | select(.error == null) | .assignments[].endpoint'
```

脚本应先检查 error，不能只用进程退出码判断查询成功；本地 kernel/执行详情可改用 status --json。库层 SessionView 和 colab_api::Usage 的模型未修改。

## 验证与限制

[运行时探针](../tools/compare_merged_inventory.py) 先验证三仓库提交和源码哈希，再在临时 HOME/config、合成独立 OAuth 缓存上运行实际合并入口。没有替换探针业务函数、认证函数或 HTTP 客户端；仅将 SDK Prod 地址设为精确的 loopback origin、禁用代理继承与后台更新。缓存由真实 google-auth 加载，报告记录 Python/google-auth/requests/typer/pydantic 的实际版本，服务器核对逐账户 Bearer、GET target 和 authuser=0；禁止实际 token 刷新、资源 mutation、其他 host/port、DNS、keyring、浏览器与子进程。Rust 端强制无原生凭据库构建。

[22 组结果](../research/merged-inventory-comparison.json) 核对 JSON/文本、单账户和全部账户、根级选择、独立身份、命名/孤立会话、形状、空集、部分 HTTP 失败和缺凭据。记录每账户请求顺序，仅忽略跨账户线程交错；独立断言成功才裁剪并刷新 proxy、失败/只查用量不改会话、凭据和注册表字节不变。runtime 缓存仅比较名称及经过 token/url 校验的刷新状态，不宣称到期时刻、额外硬件缓存或历史日志字节完全等价。

正常结果逐项一致；七组错误文本差异单独保留，仅允许对应 JSON 行的 error 文本不同，其余输出、状态和请求必须相同。未将这些行计入 matched。原版依据为 [账户探针](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L669)、[sessions 包装](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L848) 和 [usage 包装](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L904)。

```sh
cargo build --locked --no-default-features
mkdir -p target/no-keyring
cp target/debug/colab target/no-keyring/colab
python3 tools/compare_merged_inventory.py \
  --cli /path/to/google-colab-cli --ext /path/to/colab-ext \
  --kernel /path/to/jupyter-kernel-client --python /path/to/official-python
cargo build --locked
```

新增 [三项 Rust 测试](../tests/merged_inventory.rs) 验证单行 JSON、身份/裁剪隔离、HTTP 错误和无效全局资料零请求；新增两项工具网络策略测试，拒绝其他 host/port、IPv6、Unix socket、userinfo 和未知 origin。旧 ADC CLI 测试仍核对身份与 wire，但九个单账户 JSON 错误场景改为返回 0 并新增结构化 error 断言；取消测试仍要求非零信号退出。

不代表真实 Google 接受性、原生钥匙串、ADC 合并版并发或完整历史/缓存字节等价；U01 和整体验收尚未完成。

首次全量回归发现服务账户 ADC 集成测试仍断言旧空数组；按新账户对象校验资料、provider、空 assignments、usage/error，并保留原有真实 JWT/请求次数和文件不变断言。最终结果以更新后的全量重跑为准。

最终验证：默认特性 395 项、无默认特性 393 项 Rust 测试通过，两种特性 Clippy、fmt 通过。工具测试 59 项中 58 通过、1 项 Linux 专属跳过。合并在线汇总 22 组为 15 一致、7 组仅保留错误文本差异，零未解释差异；既有 27 组账户变更、21 组选择和 16 项清单对照仍一致。官方单端基线的完整 CLI 365 场景保持命令输入和分类（92 一致、54 保留差异、219 Rust 扩展）；九项 JSON 错误的退出约定和对应断言按本次契约迁移。83 个合并帮助路径无缺失。三份默认 debug、三份无钥匙串报告分别匹配对应二进制哈希；本机 release 的 16 项打包/运行检查通过。没有推送或发布，也未进行真实 Google/ADC 合并版并发验收。
