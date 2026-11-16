# Colab 控制面与文件接口

本章补齐会话分配、token 更新、用量与文件操作的线缆契约。所有字段来自固定 CLI 提交；示例中的标识和凭据均为占位符。服务端返回的额外字段、枚举以及错误格式仍应通过脱敏真实样本验证。

## 接口来源

| 标记 | 固定源码 |
| --- | --- |
| H01 | [环境、枚举、UUID 与硬件形状](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/client.py#L48-L121) |
| H02 | [响应模型](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/client.py#L124-L169) |
| H03 | [公共 HTTP 请求与解码](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/client.py#L190-L253) |
| H04 | [会话分配、释放与重试](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/client.py#L255-L360) |
| H05 | [状态同步、proxy token 更新](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/common.py#L36-L197) |
| H06 | [Contents API 全部实现](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/contents.py#L25-L96) |
| H07 | [CCU 模型和格式化](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/consumption.py#L20-L56) |
| H08 | [创建后落盘与状态展示](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/session.py#L108-L250) |

## 域名与凭据

源码 Prod 配置包含 `https://colab.research.google.com` 和 `https://colab.pa.googleapis.com`；本章实际追到的 assignments、CCU 与 assign/unassign 都访问前者的 `/tun/m`。第二个域名存在于配置不代表这些功能会调用它。[H01、H04]

控制面 HTTP 通过 OAuth/ADC 的授权 session 发送 Bearer token。runtime URL 来自后端分配响应，Jupyter、Contents、SSH 使用 runtime proxy token。两类 token 不可互换。

公共请求添加 `Accept: application/json` 和 `X-Colab-Client-Agent: colab-cli`；session backend 请求带 `authuser=0`。扩展的多账户身份由 Bearer 凭据隔离，不能通过改变 authuser 数字切换 OAuth 身份。[H03]

公共解码去掉精确 XSSI 前缀 `)]}'\n`；空 body 返回空值；未传 schema 时不解码业务响应；其他情况下对 JSON 校验 schema。Rust 应先检查 HTTP 状态，再解析响应，不将 Google HTML 登录页当作普通 JSON 错误。[H03]

`COLAB_CLI_REQUEST_TIMEOUT` 是可选的请求 timeout 字符串，源码转 float；未设置时沿用授权 HTTP 库默认。Rust 应明确连接/读取 timeout 类型及默认，不能把该字段与执行空闲 timeout 混用。[H03]

## 请求表

| 方法 | 路径 | 参数与请求体 | 期望响应 |
| --- | --- | --- | --- |
| GET | `/tun/m/assignments` | authuser | `assignments` 数组 |
| GET | `/tun/m/assign` | nbh、variant、accelerator、可选 shape、authuser | 已有 Assignment 或分配准备信息 |
| POST | `/tun/m/assign` | 同一 query；X-Goog-Colab-Token header；无业务 JSON body | 分配响应 |
| GET | `/tun/m/unassign/{endpoint}` | authuser | `{token: ...}` XSRF 材料 |
| POST | `/tun/m/unassign/{endpoint}` | authuser；X-Goog-Colab-Token header | 不依赖业务响应的释放操作 |
| GET | `/tun/m/ccu-info` | authuser | currentBalance、consumptionRateHourly、assignmentsCount |

这些 GET 中 assign 准备阶段是否绝对无服务端副作用不能从 HTTP 方法推定；研究阶段不应仅因为 GET 就随意请求资源相关端点。[H04]

## 分配 identity 与参数

函数 `uuid_to_web_safe_base64` 实际不做 base64 编码：将 UUID 标准文本中的 `-` 替换为 `_`，尾部补 `.` 到 44 字符。[H01]

```text
输入：00000000-0000-0000-0000-000000000000
nbh：00000000_0000_0000_0000_000000000000........
```

同一次分配重试必须使用同一 nbh；换 UUID 再尝试会丢失调和已有分配的依据。

variant query 是字符串 DEFAULT/GPU/TPU；响应中的 AssignmentVariant 却是整数 0/1/2。Shape 响应为 0/1，而高内存请求 query 为 `shape=hm`。不能对所有这些字段使用同一个 serde enum 表示。[H01、H02]

accelerator 包含 NONE、G4、T4、L4、A100、H100、V5E1、V6E1。L4/V5E1/V6E1 的 high-mem 选项不发送 shape；其他未请求 high-mem 时同样省略，而不是显式传 STANDARD。

## 响应 schema

```json
{
  "assignments": [
    {
      "accelerator": "NONE",
      "endpoint": "<endpoint>",
      "variant": 0,
      "machineShape": 0,
      "runtimeProxyInfo": {
        "token": "<redacted>",
        "tokenExpiresInSeconds": 3600,
        "url": "https://<runtime-host>"
      }
    }
  ]
}
```

示例的 3600 只是便于理解的样例，不代表服务器固定有效期。[H02]

| 模型 | 必需字段 |
| --- | --- |
| RuntimeProxyInfo | token 字符串、tokenExpiresInSeconds 整数、url 字符串 |
| ListedAssignment | accelerator、endpoint、variant 整数、machineShape 整数、runtimeProxyInfo |
| PostAssignmentResponse | accelerator、endpoint、variant 整数、runtimeProxyInfo |
| GetAssignmentResponse | acc、nbh、token、variant 字符串 |
| Assignment | endpoint、runtimeProxyInfo |
| GetUnassignRequest | token |

GET assign 的 response 是 union：若已带 runtimeProxyInfo 则直接复用，否则取得 XSRF token 再 POST。Rust 应用字段结构区分，不仅检查 HTTP 200。[H02、H04]

本地 machine_shape 在创建时按请求推导，不一定代表服务端真实分配形状；后续 assignments 同步才有后端 machineShape。显示来源需要明确。[H08]

Rust 设计：未知 accelerator/variant 保留原值并显示 Unknown，避免一个新硬件型号让整个多账户汇总失败；是否接受缺失字段则按模型分别处理，不静默把无效凭据当作有效会话。

## 分配重试与调和

源码默认允许初次请求后再重试 3 次；`COLAB_CLI_ASSIGN_RETRIES` 下限 0，`COLAB_CLI_RETRY_BACKOFF` 默认 10 秒、下限 0。第 k 次重试前等待 `backoff * 2^k`，默认 10/20/40 秒。[H04]

可重试：ReadTimeout 或 HTTP 502/503/504。412 立即转 TooManyAssignmentsError；不是对所有 4xx 重试。其他异常继续抛出。

```text
GET assign(nbh)
  已有 Assignment -> 保存并返回
  准备信息 -> POST assign(同一 nbh, XSRF token)
    成功 -> 保存并返回
    可重试失败 -> 退避 -> GET assign(同一 nbh)
      已有 Assignment -> 恢复成功，不再次 POST
      准备信息 -> 更新 XSRF token，再 POST
```

Rust 设计：分配前持久化 operation ID 与 nbh，成功取得 endpoint 后立即登记；本地保存失败仍需保留可恢复资源信息。跨进程崩溃恢复是高于现有单次函数重试的新增保证。

不要把“412”解释为永远只有并发数耗尽；源码提示包括临时容量/使用限制。400 在指定 accelerator 时提示可能没有相应权益；真实错误原因仍以响应为准。[H08]

## Proxy token 与本地状态同步

源码按收到响应的 UTC 时刻加 tokenExpiresInSeconds 计算绝对过期时间。get_session 在过期时间缺失或距离过期不超过 5 分钟时查询 assignments，更新 token/url/expiry 并落盘；后端找不到 endpoint 时裁剪本地记录并记录 session_terminated。[H05]

sync_sessions 将本地状态与后端 endpoint 集合对齐：删除失效的本地会话，刷新有效会话 token。后端存在但本地没有的会话仍需显示为孤立 assignment，不能因为缺本地名称就隐形。

Rust 应按 `(profile, endpoint)` 识别后端资源；多账户同名 session 必须允许存在。长任务结束后的清理需要重新取得新 token，不能只复用启动时的旧值。

## Contents API 请求与响应

base URL 为分配返回的 runtime URL，移除尾斜线后追加 `/api/contents/<path>`。path 去首尾斜线，URL 编码保留内部 `/`；空格、`#`、`?`、`%` 和 Unicode 必须编码。[H06]

所有请求 query 包含 `authuser=0`、`colab-runtime-proxy-token=<proxy-token>`。当前 ContentsClient 不通过控制面 OAuth AuthorizedSession，不应给它附加其他账户的 Bearer token。

| 操作 | 方法 | 额外参数 / body | 返回处理 |
| --- | --- | --- | --- |
| list | GET | 无 | 目录模型 content 数组或文件模型 |
| upload | PUT | base64 JSON；见下方 | 解析 JSON |
| download | GET | `content=1` | 按 format 解码 |
| remove | DELETE | 无 | 不解析 JSON |

```json
{
  "name": "sample.bin",
  "path": "content/sample.bin",
  "type": "file",
  "format": "base64",
  "content": "<base64>",
  "chunk": 1
}
```

源码一次性读完整本地文件、base64 后上传；`chunk=1` 不能据此推定支持任意分块。Rust 第一版可保持该语义，并定义内存/文件大小上限；分块需要额外验证。

下载对 type=directory 报 IsADirectoryError；format=base64 解码字节，format=json 生成缩进 JSON，其他值按文本写入。404 统一映射 FileNotFoundError；其他失败调用 raise_for_status。空文件与 notebook content 对象必须分别测试。

Rust 设计：本地下载先写临时文件、校验成功再替换，避免网络失败破坏已有文件。远端 delete/overwrite 的参数验证在请求前完成。路径操作使用 URL 类型与精确编码规则，不通过 shell。

## CCU 与账户汇总

ccu-info 返回 currentBalance 浮点数、consumptionRateHourly 浮点数、assignmentsCount 整数。内部显示模型将 currentBalance 映射为 paidComputeUnitsBalance，名称变化不代表服务端区分免费/付费余额。[H07]

单账户文本为 Current balance、Usage rate、Active assignments 三行；余额与速率固定两位小数。扩展 --all 的 JSON 结构还需实际 fixture，不能直接把内部 pydantic 模型当作公共 JSON 输出契约。

Rust 设计：保留账户错误与成功结果，限制并发数和每账户超时；不要遇到某账户缺少凭据就请求交互式登录。浮点精度用于展示与兼容，不用于自行推导账单或保证计费金额。

## 验收入口

控制面以本地 HTTP server 验证完整 request target、headers、XSSI、union、重试和调和；文件 API 做真实字节往返；分别覆盖未知枚举、损坏响应、空 body、404、412、超时和 save 失败。真实 CPU VM 测试在记录 endpoint 后执行，结束以服务端 assignments 复查释放。具体编号见 [V04](06-validation-and-open-questions.md)。
