# 42. 合并版账户变更与文件凭据生命周期

## 输入、状态变化和输出

`account/acct use/switch` 支持资料名和邮箱选择器，输出 `[account] Active account -> NAME (EMAIL)`；没有邮箱时不追加括号。`set-drive` 输出计算账户绑定或全局共享 Drive 的结果，清除时分别使用 `(own credentials)` 和 `(cleared)`。与扩展一致，显式 `--clear` 忽略同时传入的存储选择器；没有选择器也没有 --clear 则退出 1，不修改配置。

`rename` 和 `remove/rm` 输出变更对象及操作后的 active。不存在/歧义选择器、目标名称无效、目标冲突及普通本地会话保护使用合并版的 `[account]` 错误文案与退出码。凭据库、损坏数据、路径和受管资源错误仍使用 Rust 的明确错误，不伪装成普通参数问题。

账户集合改用保序的 IndexMap；加载、更新和写回保持配置文件顺序，重命名在原位置替换键，删除保留其余顺序。账户并发汇总也沿用文件顺序，不再隐式按名称重排。删除 active 时选择首个剩余账户；最后一个账户删除后仍建立默认资料。JSON 注册表 schema 仍为 1，账户字段不改变，旧文件可直接加载。重命名更新 active、默认 Drive 和所有绑定（包括自绑定），并清除迁移标记、更新时间；计算账户 set-drive 更新其时间。不会自动重排过去已被排序的文件。

## file 模式与恢复

此前生命周期为了跨后端清理，在显式 file 模式下也读取/删除 keyring，可能导致密码提示或后端不可用报错。现在新建 file 重命名/删除只处理文件，与扩展一致：不读取、写入、迁移或删除系统 keyring 槽；原有槽保持原状。这也意味着 file 操作不是完整的系统凭据撤销。

事务日志新增 credentials=`file` 或 `both`。file 事务在 repair 时继续仅使用文件，即使当前命令改用 auto/keyring；both 事务维持已有跨后端迁移和清理规则。没有此字段的旧日志默认为 both，因此不能通过改用 file 绕过旧事务的凭据保护。没有原生后端的构建仍会对旧的 both 事务明确报错。不得手删日志绕过保护。

重命名仍在目标凭据、目录和注册表提交后清理源目录；失败保留源状态和日志。受管 Drive 计划/租约、待确认控制面操作依然阻止 rename/remove，--force 只放宽普通本地会话记录的限制。不会停止 VM、撤销云 token 或创建网络资源；本阶段没有远端清理责任。

## 对照与验证边界

[固定合并版变更探针](../tools/compare_merged_account_mutations.py) 在隔离 HOME/config 上实际运行 27 组用例，包含多步变更、邮箱选择、错误/冲突/清除、重命名迁移、普通会话保护、强制删除及最后账户回退。每一步对比 stdout、stderr、退出码、保序账户业务字段、绑定和本地文件的规范 JSON SHA256；失败操作另外检查负载字节没有变化。时间字段及可选字段默认值不做字节等价承诺；凭据内容不进入报告。

探针复用固定三仓库源码/提交验证及禁止网络、keyring、浏览器和子进程的 Python 加载器。Rust 变更端强制要求无 native-keyring 的构建，先只读检查能力，原生或未知能力的二进制直接拒绝，避免回归时接触本机凭据库。结果见 [merged-account-mutations.json](../research/merged-account-mutations.json)。它的 Rust 哈希对应专用无原生后端二进制，不等于默认 debug 哈希。

```sh
cargo build --locked --no-default-features
mkdir -p target/no-keyring
cp target/debug/colab target/no-keyring/colab
python3 tools/compare_merged_account_mutations.py \
  --cli /path/to/google-colab-cli --ext /path/to/colab-ext \
  --kernel /path/to/jupyter-kernel-client --python /path/to/official-python
# 之后按需重建默认特性。
cargo build --locked
```

[四项 Rust 回归测试](../tests/account_mutations.rs) 使用访问即 panic 的凭据替身，验证文件生命周期不访问后端、失败后改模式恢复、旧日志双后端清理和保序回退；并继续运行既有跨账户/并发/迁移测试。新增两个工具安全测试验证拒绝原生/未知构建及能力探测失败不被吞掉。真实系统钥匙串迁移/删除和跨平台运行仍未验收。

全量回归首次发现账户状态测试仍断言旧的名称排序；固定扩展 [probe_all_accounts](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L746) 明确保留注册表顺序，因此修正测试为 default 在先、随后添加的 ADC 在后，并保留两种身份、请求次数和文件不变的断言。最终验收以修订后重跑为准。

最终验证：默认特性 387 项、无默认特性 385 项 Rust 测试通过，两种特性 Clippy、fmt 通过。工具测试 57 项中 56 通过、1 项 Linux 专属跳过。27 组变更对照及此前 16 项清单对照全部一致；完整官方 CLI 差分仍为 365 项（92 一致、54 保留差异、219 Rust 扩展），原有命令输入和分类不变；83 个合并帮助路径无缺失。三份默认 debug 报告哈希匹配，变更报告单独匹配无钥匙串构建，本机 release 的 14 项检查通过。未推送代码或发布 Release。
