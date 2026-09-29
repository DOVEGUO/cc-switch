# Kiro API Key 接入

只支持 Kiro API Key 接入，不提供 Kiro 账号登录（Builder ID / OAuth）。

## 配置

1. 添加 Claude 供应商，选择 **Kiro** 预设。
2. 填入在 Kiro 门户创建的 `ksk_...` 密钥。
3. 设置密钥对应的 **Kiro Region**，例如 `us-east-1` 或 `eu-central-1`。
4. 点击获取模型列表，在模型映射中选择服务端返回的原始 `modelId`。
5. 开启 CC Switch 的 Claude 本地代理接管。CC Switch 发往 Kiro 的请求自动跟随 Windows 系统代理（若设置了 CC Switch 全局代理，也会跟随该设置），不需要在 Claude Code 的 settings.json 里另外配置代理。

区域保存在供应商的 `ANTHROPIC_BASE_URL`，不引入第二份区域配置：

- 推理：`https://runtime.<region>.kiro.dev/`
- 模型目录：`https://management.<region>.kiro.dev/`，`ListAvailableModels`，支持分页。

API Key 请求使用 `Authorization: Bearer ...` 和 `tokentype: API_KEY`，请求 origin 为 `AI_EDITOR`。

## 协议转换

Claude 的 `/v1/messages` 请求在 CC Switch 内部转换为 Kiro `conversationState`。系统提示、多轮对话、图片、工具定义、工具调用与工具结果通过现有代理转发。

Kiro AWS EventStream 经 CRC 校验和分片重组后，复用现有 OpenAI → Anthropic 转换器：流式请求返回 Anthropic SSE，非流式请求聚合为 Anthropic JSON。工具调用参数保留完整 JSON，输出上限对应 `max_tokens` 停止原因，异常帧不会被当作正常完成。

模型目录不限制厂商；服务端返回的 Claude、GPT、DeepSeek、MiniMax、GLM、Qwen 及后续新增 ID 均可选择或手动填写。只转换已知的 Claude 客户端别名；`[1M]` 后缀只在发送前移除，不生成服务端目录中不存在的 `-1m` ID。

附加生成参数只发送给明确支持它们的模型。当前支持附加参数的 Claude 模型，其 `max_tokens` 按服务端要求限制在 1024–64000 或 1024–128000；其他模型采用 Kiro 默认生成设置。GPT 的思考参数映射为 `reasoning.effort`，不接收 Claude 的 `output_config`。

Token 用量复用现有采集器。服务端只返回 credits 的事件不伪造 Token 数，也不会清零此前已经收到的 Token 用量。

## 用量

API Key 模式下，Kiro 只回传 credits 与 `contextUsagePercentage`（上下文占用比例），不回传逐请求的 Token 计数。因此 Token 用量按估算展示：`contextUsagePercentage` × 所选模型的输入窗口，加上输出字符数 / 4 估算的输出 Token；Prompt Cache 命中数在该模式下 Kiro 不回传，不作展示。

剩余 credits 通过 配置用量 → Token Plan → Kiro 查看：月度额度、已用/总额度、下次重置时间。查询会自动把供应商的 `ANTHROPIC_BASE_URL`（`https://runtime.<region>.kiro.dev`）换算成对应的 `https://management.<region>.kiro.dev/` 用量端点，无需另外填写凭据。

## 验证

### Claude Code WebSearch

Kiro API Key 模式下，Claude Code 内置 WebSearch 的独立 `web_search_20250305` 请求会直接调用所选区域的 `https://q.<region>.amazonaws.com/mcp`，复用 CC Switch 网络代理。无需另外配置搜索服务密钥。

支持 JSON 和 Anthropic SSE 响应、域名结果过滤、30 秒超时及标准搜索失败结果。搜索结果来自 MCP，不估算或记入模型 Token 费用。当前仅支持独立搜索子请求；混合多工具搜索、动态过滤版搜索和位置参数会明确拒绝，不会静默忽略。

编译、类型检查、前端单元测试、协议测试及 MSI 打包由 GitHub 的 `Custom Windows MSI` 工作流执行。本地不需要 Rust。

工作流还上传 `Kiro-API-route-tests-windows-x64` 测试程序。该程序使用内存数据库和独立测试目录，不读取或改写实际供应商配置。通过进程环境提供以下变量后，运行下载的测试可执行文件：

```powershell
# KIRO_API_KEY 由调用环境安全提供，勿写进仓库。
# KIRO_TEST_PROXY 可选：不设置时走系统代理（http_client::init(None)）。
$env:KIRO_TEST_PROXY = 'http://127.0.0.1:7897'
$env:KIRO_REGION = 'us-east-1'
$env:CC_SWITCH_TEST_HOME = Join-Path $env:TEMP 'cc-switch-kiro-live'
& '<下载的 cc_switch_lib-*.exe>' kiro_api_live_route --ignored --nocapture --test-threads=1
```

默认逐个测试目录内全部模型，交替使用流式与非流式请求，随后验证工具调用和工具结果回传。可通过 `KIRO_TEST_MODELS` 指定逗号分隔的模型 ID 做定向复测。测试会消耗少量 Kiro 额度，不向 GitHub 上传密钥。
