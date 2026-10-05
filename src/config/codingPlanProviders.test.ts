import { describe, it, expect } from "vitest";
import {
  detectCodingPlanProvider,
  extractBaseUrlForUsageDetection,
  injectCodingPlanUsageScript,
} from "./codingPlanProviders";

// codex 预设的 config 由 generateThirdPartyConfig 生成，这里取其等效形态
const OPENCODE_GO_CODEX_TOML = `model_provider = "custom"
model = "glm-5.2"
model_reasoning_effort = "high"
disable_response_storage = true

[model_providers.custom]
name = "opencode_go"
base_url = "https://opencode.ai/zen/go/v1"
wire_api = "responses"
requires_openai_auth = true`;

describe("detectCodingPlanProvider (OpenCode Go)", () => {
  it("matches both base variants across apps", () => {
    // claude/claude-desktop 预设是 /zen/go，codex/opencode/pi 是 /zen/go/v1
    expect(detectCodingPlanProvider("https://opencode.ai/zen/go")).toBe(
      "opencode_go",
    );
    expect(detectCodingPlanProvider("https://opencode.ai/zen/go/v1")).toBe(
      "opencode_go",
    );
  });

  it("does not match OpenCode Zen (pay-as-you-go, no usage API)", () => {
    expect(detectCodingPlanProvider("https://opencode.ai/zen/v1")).toBeNull();
  });
});

describe("detectCodingPlanProvider (Command Code)", () => {
  // 正向用例（/provider 与 /provider/v1）由上游 commandCodeUsage.test.ts 覆盖。
  // 这里只钉边界。裸主机刻意不命中：它是 Go 档的 base，而 Go 档没有 API 接入，
  // 后端 coding_plan.rs 的 detect_provider 同样要求 path 含 /provider——放宽这条
  // 只会让前端出额度卡、后端回 Unknown coding plan provider。
  it.each([
    "https://api.commandcode.ai",
    "http://api.commandcode.ai",
    "http://127.0.0.1:55990",
    "http://localhost:55990",
    "https://api.commandcode.ai.example.com",
    "https://proxy.example.com/api.commandcode.ai",
  ])("does not treat %s as Command Code", (baseUrl) => {
    expect(detectCodingPlanProvider(baseUrl)).toBeNull();
  });
});

describe("detectCodingPlanProvider (MiniMax)", () => {
  it.each([
    "https://api.minimax.cn/v1",
    "https://api.minimaxi.com/v1",
    "https://api.minimax.io/v1",
    "https://API.MINIMAX.CN/anthropic",
  ])("recognizes the MiniMax usage provider for %s", (baseUrl) => {
    expect(detectCodingPlanProvider(baseUrl)).toBe("minimax");
  });

  it.each([
    "https://api.minimax.cn.example.com/v1",
    "https://proxy.example.com/api.minimax.io/v1",
  ])("ignores look-alike MiniMax hosts such as %s", (baseUrl) => {
    expect(detectCodingPlanProvider(baseUrl)).toBeNull();
  });
});

describe("extractBaseUrlForUsageDetection", () => {
  it("reads env.ANTHROPIC_BASE_URL for claude and claude-desktop", () => {
    const config = {
      env: { ANTHROPIC_BASE_URL: "https://opencode.ai/zen/go" },
    };
    expect(extractBaseUrlForUsageDetection("claude", config)).toBe(
      "https://opencode.ai/zen/go",
    );
    expect(extractBaseUrlForUsageDetection("claude-desktop", config)).toBe(
      "https://opencode.ai/zen/go",
    );
  });

  it("reads base_url from the codex TOML config", () => {
    expect(
      extractBaseUrlForUsageDetection("codex", {
        auth: { OPENAI_API_KEY: "" },
        config: OPENCODE_GO_CODEX_TOML,
      }),
    ).toBe("https://opencode.ai/zen/go/v1");
  });

  it("reads options.baseURL for opencode and baseUrl for pi", () => {
    expect(
      extractBaseUrlForUsageDetection("opencode", {
        options: { baseURL: "https://opencode.ai/zen/go/v1" },
      }),
    ).toBe("https://opencode.ai/zen/go/v1");
    expect(
      extractBaseUrlForUsageDetection("pi", {
        baseUrl: "https://opencode.ai/zen/go/v1",
      }),
    ).toBe("https://opencode.ai/zen/go/v1");
  });

  it("returns null for unsupported apps", () => {
    expect(
      extractBaseUrlForUsageDetection("gemini", {
        env: { GOOGLE_GEMINI_BASE_URL: "https://opencode.ai/zen/go" },
      }),
    ).toBeNull();
  });
});

type TestProvider = {
  settingsConfig?: Record<string, any>;
  meta?: Record<string, any>;
};

describe("injectCodingPlanUsageScript", () => {
  const inject = (appId: string, provider: TestProvider) =>
    injectCodingPlanUsageScript(appId, provider);
  const expectInjected = (provider: TestProvider) => {
    expect(provider.meta?.usage_script).toMatchObject({
      enabled: true,
      templateType: "token_plan",
      codingPlanProvider: "opencode_go",
    });
  };

  it("injects OpenCode Go for every app that ships its preset", () => {
    expectInjected(
      inject("claude", {
        settingsConfig: {
          env: { ANTHROPIC_BASE_URL: "https://opencode.ai/zen/go" },
        },
      }),
    );
    expectInjected(
      inject("claude-desktop", {
        settingsConfig: {
          env: { ANTHROPIC_BASE_URL: "https://opencode.ai/zen/go" },
        },
      }),
    );
    expectInjected(
      inject("codex", {
        settingsConfig: { config: OPENCODE_GO_CODEX_TOML },
      }),
    );
    expectInjected(
      inject("opencode", {
        settingsConfig: {
          options: { baseURL: "https://opencode.ai/zen/go/v1" },
        },
      }),
    );
    expectInjected(
      inject("pi", {
        settingsConfig: { baseUrl: "https://opencode.ai/zen/go/v1" },
      }),
    );
  });

  it("leaves the Go-plan bare host without a quota script", () => {
    // Go 档没有 API 接入，base 就是裸主机；注入与否由上游 commandCodeUsage.test.ts
    // 按 /provider 覆盖，这里只钉住 Go 档不被误注入。
    const injected = inject("claude", {
      settingsConfig: {
        env: {
          ANTHROPIC_BASE_URL: "https://api.commandcode.ai",
          ANTHROPIC_AUTH_TOKEN: "user-test",
        },
      },
    });
    expect(injected.meta?.usage_script).toBeUndefined();
  });

  it("keeps the existing claude behavior for other coding plans", () => {
    const injected = inject("claude", {
      settingsConfig: {
        env: { ANTHROPIC_BASE_URL: "https://api.kimi.com/coding/v1" },
      },
    });
    expect(injected.meta?.usage_script?.codingPlanProvider).toBe("kimi");
  });

  it("does not extend other coding plans to non-claude apps", () => {
    // 智谱/Kimi 等在其他 app 的自动注入未逐一验证，仅 OpenCode Go 放行
    const provider: TestProvider = {
      settingsConfig: {
        options: { baseURL: "https://open.bigmodel.cn/api/anthropic" },
      },
    };
    expect(inject("opencode", provider).meta?.usage_script).toBeUndefined();
  });

  it("never overwrites an existing usage_script", () => {
    const provider: TestProvider = {
      settingsConfig: {
        env: { ANTHROPIC_BASE_URL: "https://opencode.ai/zen/go" },
      },
      meta: { usage_script: { enabled: false, templateType: "custom" } },
    };
    expect(inject("claude", provider).meta?.usage_script).toEqual({
      enabled: false,
      templateType: "custom",
    });
  });
});
