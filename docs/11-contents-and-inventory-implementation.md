# Contents 文件接口与账户汇总实现

更新日期：2026-11-16。本章记录 P2 文件操作与跨账户查询的已实现行为；协议依据为 [07](07-control-plane-and-contents.md)。离线回归使用 loopback HTTP；后续真实 CPU 验收见 [15](15-live-cpu-validation.md)。

后续实际合并版十二组文件请求与下载字节对照见 [75](75-merged-files-and-json-bytes.md)：JSON 下载新增原版 ASCII 转义，目录下载错误文本与输出流对齐；失败仍不覆盖本地文件。

JSON 大整数精度、浮点指数、重复文档键及扩展十八组对照见 [76](76-json-number-download-compatibility.md)；控制面和凭据的 JSON 解析特性保持原配置。

## 命令与输入验证

| 命令 | 当前行为 |
| --- | --- |
| `ls [PATH] [-s NAME] [--json]` | PATH 默认 content；目录优先、按名称排序；文本目录名加 `/`；JSON 仅输出 name/path/type 元数据 |
| `upload LOCAL REMOTE [-s NAME]` | 本地输入预读、普通文件及大小检查先于认证；base64 Contents PUT |
| `download REMOTE LOCAL [-s NAME]` | 拒绝目录、无效编码与无效 content；完整校验后原子替换本地文件 |
| `rm PATH [-s NAME]` | DELETE；成功时不解析空响应 |
| `edit PATH [-s NAME]` | 下载、运行编辑器、比较字节，仅变化后上传；仅 404 作为新空文件 |
| `status/sessions --all [--json]` | 后端会话汇总，每账户独立同步与错误 |
| `usage --all [--json]` | CCU 用量汇总，每账户独立错误 |

文件命令通过已有账户上下文解析会话所有者，显式账户优先于环境和会话路由，不改变 active。未指定 -s 时，当前账户恰有一个本地会话才自动选择；没有会话或存在多个会话均报错，不自动创建 VM。有未完成控制面操作时拒绝文件操作，先恢复 new/stop。

远端路径去除首尾 `/`，禁止空根路径的文件修改/下载、`.`/`..` 分段、控制字符和反斜线。目录查询允许根路径。内部 `/` 保留；空格、中文、`#`、`?`、`%` 及已经看似编码的字面量均通过 URL 分段编码一次，不执行 shell。相比上游，拒绝路径穿越与未知文件 format 是有意增加的校验。

## Runtime 协议与状态刷新

[contents.rs](../src/contents.rs) 使用独立 HTTP client，不接收控制面 Bearer，也不附加 X-Goog-Colab-Token。保留 runtime base URL 的既有 path，再追加 `/api/contents/<encoded-path>`；query 为 authuser=0 与 colab-runtime-proxy-token，下载另带 content=1。运行时 URL 只接受 HTTPS 或 loopback HTTP，拒绝 URL 凭据、query、fragment；禁止跟随重定向，错误不输出 URL、响应正文或 token。

使用 [SessionService::with_session](../src/sessions.rs) 访问已有会话；token 缺失、无到期时间或距到期不超过五分钟时，先查询控制面 assignments，同步 proxy token/url/expiry。查询失败不裁剪，后端确认消失时裁剪并报会话不存在。整个单次文件请求持有对应账户生命周期锁，避免改名或删除后的旧上下文写回状态。

HTTP 连接超时 5 秒，总超时沿用控制面的请求 timeout（默认 20 秒）。响应先检查状态；404 单独映射 RemoteMissing，其他非 2xx 报状态错误。PUT、DELETE 和文件请求均不自动重试；网络中断时远端写操作结果可能未知，用户应先查询或下载核验。

上传使用 type=file、format=base64、chunk=1 和原始字节的标准 base64。chunk=1 不代表支持流式或任意分块。单个本地文件与解码后下载上限均为 32 MiB；Contents JSON 响应上限 48 MiB。超限直接失败，不宣称支持大文件。

下载支持 base64 字节、text 字符串与 json 对象；notebook 的 JSON content 以两空格缩进写出。空 base64 文件正常写入零字节；目录、无效 base64、非字符串 text、未知 format 或损坏 JSON 均报错。目标父目录必须已存在，目标不能为目录或符号链接。先将完整字节写入同目录私有临时文件并 fsync，再原子替换目标；Unix 新文件权限为 0600，替换后同步父目录。

## 编辑器与失败恢复

编辑器选择顺序为 VISUAL、EDITOR、交互时默认 vi。环境变量可包含本地 shell 命令及参数，如 `code --wait`；临时文件路径通过独立位置参数传递，不拼接远端路径或文件内容。非交互且没有显式编辑器时报错。

下载阶段的 401/403、网络错误、目录或损坏响应均在编辑器启动前失败。仅 404 创建空编辑文件；临时文件保留远端扩展名，权限受限。编辑器非零退出不上传；字节相同不上传；新增的空文件若没有变化同样不上传。

等待编辑器时不持有账户锁。上传前重新获取所选账户凭据，再重新访问同一名称会话，刷新临近过期的 proxy token，并核对 endpoint 与编辑开始时一致。若用户在编辑期间更换 VM，不把旧文件上传到新 endpoint。上传失败或 endpoint 不匹配时返回 EditPending，保留私有临时目录及编辑结果，错误显示本地路径，用户可手动恢复或上传；成功和无变化时自动删除临时文件。

## 跨账户汇总

[inventory.rs](../src/inventory.rs) 每批最多四个账户并发查询，按注册表文件顺序输出。每个 worker 独立显式解析账户，不受合法 COLAB_ACCOUNT 的单账户选择影响，不触发交互式登录。合并版 account status、sessions/usage 会先校验全局资料；sessions/usage --all 不以 -a 过滤范围。Rust 专属 status --all 仍与 -s/-a 冲突并保持原有上下文规则。

sessions/usage JSON 已迁移为合并版账户行和逐账户 error，文本同样对齐；结构与调用方迁移见 [44](44-merged-inventory-implementation.md)。只有 Rust 专属 status --all 继续使用 account 与 result/error 数组。部分或全部账户查询失败仍返回 0，应检查每行 error；命令级配置、解析和恢复错误仍非零。

## 验证与剩余目标

新增 [Contents 测试](../tests/contents.rs) 16 项、[汇总测试](../tests/inventory.rs) 3 项。验证包含完整请求 target/header/body、无 Bearer 串用、特殊字符路径、二进制/空文件/文本/notebook、失败下载保留目标、目录/格式/404/认证错误、禁止重定向、大小/路径前置校验、符号链接、编辑不变/取消/失败保留、编辑期间 VM 替换、runtime token 刷新、跨账户 CLI 文件流程、有界并发和混合成功失败汇总。

完整默认测试 180 项、关闭原生凭据库后 178 项通过；fmt、两种特性的 clippy、默认 build 与研究文档静态检查通过。

P2 的线上 CPU 分配、文件往返、释放复查见 [15](15-live-cpu-validation.md)；文件操作与生命周期历史已见 [13](13-run-and-history-implementation.md)。P3 的 Jupyter REST/WS、执行消息状态机与 exec 已见 [12](12-jupyter-and-execution-implementation.md)，run、history 与导出已见 [13](13-run-and-history-implementation.md)；P4–P6 的交互、SSH、DriveFS 和完整分发仍属于完整目标；持续保活策略、自动重建和任务恢复已移至外部调度层，不能因本章完成而标记全部设计已交付。
