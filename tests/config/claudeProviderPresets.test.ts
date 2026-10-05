import { describe, expect, it } from "vitest";
import { providerPresets } from "@/config/claudeProviderPresets";
import type { Provider } from "@/types";
import { providerNeedsRouting } from "@/utils/providerCapabilities";
import { detectCodingPlanProvider } from "@/config/codingPlanProviders";

describe("Kimi For Coding Provider Preset", () => {
  const kimiForCoding = providerPresets.find(
    (p) => p.name === "Kimi For Coding",
  );

  it("should include Kimi For Coding preset", () => {
    expect(kimiForCoding).toBeDefined();
  });

  // CLAUDE_CODE_MAX_CONTEXT_TOKENS is ignored for claude-* model ids, so the
  // preset must route the endpoint's own alias for the context envs to bite
  it("should route the kimi-for-coding model id on every tier", () => {
    const env = (kimiForCoding!.settingsConfig as any).env;
    expect(env).toMatchObject({
      ANTHROPIC_MODEL: "kimi-for-coding",
      ANTHROPIC_DEFAULT_HAIKU_MODEL: "kimi-for-coding",
      ANTHROPIC_DEFAULT_SONNET_MODEL: "kimi-for-coding",
      ANTHROPIC_DEFAULT_OPUS_MODEL: "kimi-for-coding",
    });
  });

  // 预设直接钉值，不再暴露表单输入框；要调整的用户直接改 JSON 编辑框
  it("should pin the 256K context envs without exposing form fields", () => {
    const env = (kimiForCoding!.settingsConfig as any).env;
    expect(env.CLAUDE_CODE_MAX_CONTEXT_TOKENS).toBe("262144");
    expect(env.CLAUDE_CODE_AUTO_COMPACT_WINDOW).toBe("262144");
    expect(kimiForCoding!.templateValues).toBeUndefined();
  });
});

describe("Codex Provider Preset", () => {
  const codex = providerPresets.find((p) => p.name === "Codex");

  it("should include the Codex preset", () => {
    expect(codex).toBeDefined();
  });

  // 预设直接钉 Codex 目录的 372K 窗口（openai/codex#31860），不暴露表单输入框
  it("should pin the Codex-catalog 372K window without exposing form fields", () => {
    const env = (codex!.settingsConfig as any).env;
    expect(env.CLAUDE_CODE_MAX_CONTEXT_TOKENS).toBe("372000");
    expect(env.CLAUDE_CODE_AUTO_COMPACT_WINDOW).toBe("372000");
    expect(codex!.templateValues).toBeUndefined();
  });
});

describe("OpenCode Go Provider Preset", () => {
  const openCodeGo = providerPresets.find((p) => p.name === "OpenCode Go");

  it("should use the Go Anthropic compatibility endpoint with x-api-key auth", () => {
    expect(openCodeGo).toBeDefined();

    const env = (openCodeGo!.settingsConfig as any).env;
    expect(env).toMatchObject({
      ANTHROPIC_BASE_URL: "https://opencode.ai/zen/go",
      ANTHROPIC_API_KEY: "",
      ANTHROPIC_MODEL: "deepseek-v4-flash",
      ANTHROPIC_DEFAULT_HAIKU_MODEL: "deepseek-v4-flash",
      ANTHROPIC_DEFAULT_SONNET_MODEL: "deepseek-v4-flash",
      ANTHROPIC_DEFAULT_OPUS_MODEL: "deepseek-v4-flash",
    });
    // /messages 只认 x-api-key，Bearer 被网关静默忽略——勿换回 AUTH_TOKEN
    expect(env).not.toHaveProperty("ANTHROPIC_AUTH_TOKEN");
    // 原生 anthropic 直连的预设不写 apiFormat（缺省即免路由）
    expect(openCodeGo!.apiFormat).toBeUndefined();
    expect(openCodeGo!.apiKeyField).toBe("ANTHROPIC_API_KEY");
  });

  it("should not require local routing in Claude Code", () => {
    const provider: Provider = {
      id: "opencode-go",
      name: openCodeGo!.name,
      category: openCodeGo!.category,
      settingsConfig: openCodeGo!.settingsConfig as Record<string, any>,
      meta: {
        apiFormat: openCodeGo!.apiFormat,
        apiKeyField: openCodeGo!.apiKeyField,
      },
    };

    expect(providerNeedsRouting("claude", provider)).toBe(false);
  });
});

describe("Command Code Provider Presets", () => {
  // 上游按 API 档建了一条 "Command Code"（/provider + openai_chat）；本发行版另加
  // 一条 "Command Code Go" —— Go 档没有 API 接入，只能由本地路由模拟官方 CLI。
  const apiPlan = providerPresets.find((p) => p.name === "Command Code");
  const goPlan = providerPresets.find((p) => p.name === "Command Code Go");

  const envOf = (preset: (typeof providerPresets)[number]) =>
    (preset.settingsConfig as { env?: Record<string, string> }).env ?? {};

  const asProvider = (
    preset: (typeof providerPresets)[number],
    id: string,
  ): Provider => ({
    id,
    name: preset.name,
    category: preset.category,
    settingsConfig: preset.settingsConfig as Record<string, any>,
    meta: { apiFormat: preset.apiFormat, apiKeyField: preset.apiKeyField },
  });

  it("keeps the upstream API plan on /provider without a manual edit", () => {
    expect(envOf(apiPlan!).ANTHROPIC_BASE_URL).toBe(
      "https://api.commandcode.ai/provider",
    );
    expect(apiPlan!.apiFormat).toBe("openai_chat");
    expect(apiPlan!.endpointCandidates).toEqual([
      "https://api.commandcode.ai/provider",
    ]);
  });

  it("keeps the Go plan on the bare host so /alpha/generate is hit as-is", () => {
    expect(envOf(goPlan!).ANTHROPIC_BASE_URL).toBe(
      "https://api.commandcode.ai",
    );
    expect(goPlan!.apiFormat).toBe("commandcode");
    expect(goPlan!.endpointCandidates).toEqual(["https://api.commandcode.ai"]);
  });

  it("routes both plans through the local proxy", () => {
    expect(providerNeedsRouting("claude", asProvider(apiPlan!, "cc"))).toBe(
      true,
    );
    expect(providerNeedsRouting("claude", asProvider(goPlan!, "cc-go"))).toBe(
      true,
    );
  });

  // 额度卡只认带 /provider 的 base（后端 detect_provider 同口径）；Go 档没有 API
  // 接入，刻意不出卡，多一条反而会答 Unknown coding plan provider。
  it("shows the quota card on the API plan only", () => {
    expect(detectCodingPlanProvider(envOf(apiPlan!).ANTHROPIC_BASE_URL)).toBe(
      "command_code",
    );
    expect(
      detectCodingPlanProvider(envOf(goPlan!).ANTHROPIC_BASE_URL),
    ).toBeNull();
  });
});

describe("AWS Bedrock Provider Presets", () => {
  const bedrockAksk = providerPresets.find(
    (p) => p.name === "AWS Bedrock (AKSK)",
  );

  it("should include AWS Bedrock (AKSK) preset", () => {
    expect(bedrockAksk).toBeDefined();
  });

  it("AKSK preset should have required AWS env variables", () => {
    const env = (bedrockAksk!.settingsConfig as any).env;
    expect(env).toHaveProperty("AWS_ACCESS_KEY_ID");
    expect(env).toHaveProperty("AWS_SECRET_ACCESS_KEY");
    expect(env).toHaveProperty("AWS_REGION");
    expect(env).toHaveProperty("CLAUDE_CODE_USE_BEDROCK", "1");
  });

  it("AKSK preset should have template values for AWS credentials", () => {
    expect(bedrockAksk!.templateValues).toBeDefined();
    expect(bedrockAksk!.templateValues!.AWS_ACCESS_KEY_ID).toBeDefined();
    expect(bedrockAksk!.templateValues!.AWS_SECRET_ACCESS_KEY).toBeDefined();
    expect(bedrockAksk!.templateValues!.AWS_REGION).toBeDefined();
    expect(bedrockAksk!.templateValues!.AWS_REGION.editorValue).toBe(
      "us-west-2",
    );
  });

  it("AKSK preset should have correct base URL template", () => {
    const env = (bedrockAksk!.settingsConfig as any).env;
    expect(env.ANTHROPIC_BASE_URL).toContain("bedrock-runtime");
    expect(env.ANTHROPIC_BASE_URL).toContain("${AWS_REGION}");
  });

  it("AKSK preset should have cloud_provider category", () => {
    expect(bedrockAksk!.category).toBe("cloud_provider");
  });

  it("AKSK preset should have Bedrock model as default", () => {
    const env = (bedrockAksk!.settingsConfig as any).env;
    expect(env.ANTHROPIC_MODEL).toContain("anthropic.claude");
  });

  const bedrockApiKey = providerPresets.find(
    (p) => p.name === "AWS Bedrock (API Key)",
  );

  it("should include AWS Bedrock (API Key) preset", () => {
    expect(bedrockApiKey).toBeDefined();
  });

  // Claude Code 只从 AWS_BEARER_TOKEN_BEDROCK 读 Bedrock API Key，顶层 apiKey 不生效
  it("API Key preset should carry the key in AWS_BEARER_TOKEN_BEDROCK", () => {
    const config = bedrockApiKey!.settingsConfig as any;
    expect(config).not.toHaveProperty("apiKey");
    expect(config.env).toHaveProperty("AWS_BEARER_TOKEN_BEDROCK", "");
    expect(config.env).toHaveProperty("AWS_REGION");
    expect(config.env).toHaveProperty("CLAUDE_CODE_USE_BEDROCK", "1");
  });

  it("API Key preset should NOT have AKSK env variables", () => {
    const env = (bedrockApiKey!.settingsConfig as any).env;
    expect(env).not.toHaveProperty("AWS_ACCESS_KEY_ID");
    expect(env).not.toHaveProperty("AWS_SECRET_ACCESS_KEY");
  });

  it("API Key preset should have template values for region only", () => {
    expect(bedrockApiKey!.templateValues).toBeDefined();
    expect(bedrockApiKey!.templateValues!.AWS_REGION).toBeDefined();
    expect(bedrockApiKey!.templateValues!.AWS_REGION.editorValue).toBe(
      "us-west-2",
    );
  });

  it("API Key preset should have cloud_provider category", () => {
    expect(bedrockApiKey!.category).toBe("cloud_provider");
  });
});
