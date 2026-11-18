# macOS 钥匙串代码身份排查

2026-11-18（Asia/Shanghai）。签名检查只读文件；后续另用同一 CLI 和现有系统钥匙串执行两次真实会话查询。没有导出 token、修改 ACL、创建或释放 VM。

## 已验证事实

[只读工具](../tools/inspect_macos_keychain_identity.py) 使用 `/usr/bin/codesign --display --requirements -` 检查文件，不运行输入程序。公开报告只保留文件及指定要求的 SHA-256 和身份分类，不保存原始诊断、绝对路径、证书名或真实账户。错误非零退出，缺失要求不推定成功。

[实际检查](../research/macos-keychain-identity-validation.json) 显示当前 debug、release 以及已安装原版使用的 Python 解释器均使用只含 cdhash 的指定要求。debug 与 release 的身份不同。在外部临时目录同一路径先后编译两个固定最小 Rust 程序，指定要求随二进制变化；没有运行探针，目录已清理。生产 CLI 没有重新构建。

本地固定上游 `auth.py` 使用 Python keyring；本机安装版本的 macOS 后端通过 SecItemCopyMatching 读取通用密码。Rust keyring 3.6.3 则通过 security-framework 的 find_generic_password 读取 User keychain。两者的调用 API、搜索域与进程身份存在差异，不能仅凭同一服务名推定完整互通。

## 为什么原版可能较少重新提示

Apple 的 [代码签名说明](https://developer.apple.com/library/archive/technotes/tn2206/) 指出钥匙串可利用指定要求识别访问程序，ad hoc 身份不能自动提供跨版本的稳定证书身份。[ACL API 说明](https://developer.apple.com/documentation/security/secaclcopycontents%28_%3A_%3A_%3A_%3A%29?language=objc) 解释不受信任程序可能需要密码，即使钥匙串已经解锁。

因此，重新编译 Rust 可使旧授权无法匹配新程序；Python CLI 源码变化通常不改变已有解释器的代码身份。这是有实验支持的机制解释，不是对真实条目 ACL 的直接取证。更新 Python 解释器也可能重新触发授权，不能声称原版永不提示。

开发时应固定使用同一份已允许的构建；重新构建后的提示需按新身份判断。正式发行需在同一受信任签名身份下签署各版本，并验证升级前后的指定要求和实际钥匙串访问。仅固定路径、二进制文件名或添加 ad hoc identifier 都不等价于稳定受信任的发行身份。当前没有自动创建证书、改变钥匙串信任或开放所有程序访问。

## 未证明与回归

没有检查真实条目 ACL，不能断言此前每次提示的唯一根因已确认。继承控制终端的真实验收成功与此前脱离终端的超时并存；本轮固定同一构建/账户及 30 秒上限，先继承、再脱离控制终端查询：分别约 1.436 秒与 0.891 秒成功，没有复现超时。这排除了当前“脱离终端必然超时”的解释，不代表之前超时不存在；先后顺序、已有授权和缓存等影响未隔离。没有据此修改生产凭据代码。搜索域/API 差异是否影响用户条目也尚未证明。该调查不关闭 U04、V02.7 或 V03.6。

[测试](../tools/tests/test_macos_keychain_identity.py) 覆盖 hash-only 与证书型要求分类、身份变化、失败/缺失要求和路径脱敏，调用严格限定为 codesign 检查而非执行被检查程序。完整工具回归 176 项中 175 通过、1 项平台跳过；工具输入指纹记录在 [检查报告](../research/macos-keychain-identity-validation.json)。首次测试发现 macOS `/var` 与 `/private/var` 解析后的预期路径差异，已修正测试以比较真实路径；该次失败不记为成功。

## 减少 run 生命周期内的重复访问

2026-11-18 复查发现，`run` 原先在分配、执行和释放各建一个认证客户端。每个客户端的 OAuth 内存缓存从空开始，系统钥匙串可能在同一命令中反复读取。现在 `run` 在一次命令内复用该账户的认证客户端，仍有效的 bearer 继续使用，需刷新时走原认证来源。分配尚未开始时的认证超时直接返回，释放预查询若因获取凭据失败则保留已记账的释放意图，避免同次命令立即重复获取凭据。

用现有钥匙串和 30 秒上限，更新前的三次真实 inventory 和更新后的两次分配前 inventory 均成功，覆盖继承会话及启动新会话的子进程。随后更新版用 `run` 创建一台默认 CPU，执行 `__main__`、dataclass/pickle、Unicode argv 与退出码探针，约 10.75 秒返回 23。自动释放成功，独立 inventory 确认测试 endpoint 消失，专属 run 计划已移除。此次真实构建包含客户端复用，随后补充的认证失败路径由本地测试覆盖。[脱敏实测](../research/live-run-keychain-reuse-validation.json)

这次没有复现首次读取的历史偶发超时。首次读取仍由 macOS 钥匙串处理；旧授权、代码签名身份和钥匙串 ACL 均未变更。

最终构建在默认特性下通过 143 项相关测试，在无默认特性下通过 89 项，Clippy 通过。最终构建另做四次真实只读 inventory，每次均在 30 秒内成功，且均确认同一测试 endpoint 仍不存在。没有再次创建 VM。
