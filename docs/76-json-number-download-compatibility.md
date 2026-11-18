# JSON 下载的数值精度与表示

## 问题与最终行为

在 [75](75-merged-files-and-json-bytes.md) 的 Unicode 下载字节对齐后，增加浮点与大整数样本。首次十五组中十三组一致，浮点指数写法和超 64 位整数两组不一致，见 [首次结果](../research/json-numbers-first-comparison.json)。原先将下载文档解析为通用 Value，会把部分大整数转为浮点数；原版 Python 保留任意精度整数，下载后的数据可能因此发生实质改变。

现在 [Contents JSON 文档模块](../src/contents/json_document.rs) 提取响应顶层 content 的原始片段，用 serde 的 IgnoredAny 检查 JSON 语法，再构建仅用于下载排版的文档结构。整数保留十进制原值，不经过浮点转换；浮点按 binary64 解析后输出最短表示，指数显式符号、至少两位，保留负零。对象键保持插入顺序；重复键采用首次位置与最后值，与 Python dict 一致。字符串、空数组/对象、两空格缩进和 ASCII 转义继续与既有下载行为衔接。

没有为全局 serde_json 开启额外数值特性；控制面、凭据、状态等 JSON 继续使用其既有解析语义。文档中名为 `$serde_json::private::Number` 或 `$serde_json::private::RawValue` 的普通键也保持对象成员。

## 边界与失败

继续执行原有 48 MiB 响应读取和 32 MiB 下载输出限制，文档结构限制 128 层。格式、语法、重复响应字段、额外顶层输入、深度或数值失败都先返回错误，再由原有原子下载流程保证不覆盖本地文件。文档内部的重复对象键与响应元数据的重复字段分别处理，后者继续拒绝。

本实现限定有限 binary64 浮点数。`1e999` 等溢出值报错；Python 可能导出非标准 JSON 的 Infinity，这仍是已知差异。孤立 surrogate、超出原版 Python 整数位数配置的文档、全部浮点位模式和所有畸形响应没有逐一建立等价证据，不宣称任意 JSON 逐字节等价。

## 验证范围

最终 [实际合并版对照](../research/merged-json-numbers-comparison.json) 十八组全部一致。新增范围包括浮点边界、1024 个固定种子生成的有限 binary64 值、4001-bit 整数、字符串转义、私有字段名称、嵌套重复键及非规范数值写法。仍通过真实命令、缓存合成 OAuth、一次 assignments 与一次 Contents 请求，核对完整输出、退出码、路径编码、上传 payload、下载字节及失败保留目标文件；没有真实凭据、Google 请求或云资源。

五项 Rust 测试覆盖浮点/整数、空结构与普通私有键、嵌套重复键、原始内容提取及千位整数、无效/重复响应字段；超限深度与非有限浮点均有拒绝断言。全量结果与二进制/输入摘要见 [回归记录](../research/json-numbers-regression-validation.json)。这关闭本轮发现的下载精度问题，不关闭整体 U01/U03、真实平台或发布前提。

默认／无默认 Rust 502／500 项全部通过，两种全目标 Clippy 与 fmt 通过，Python 工具 185 通过、1 跳过。依赖许可材料 --check 通过，178 份实现/测试输入收尾未变，默认 debug 已重建；release 未在本轮重建。
